//! 斗鱼二维码登录。
//!
//! 流程（参考 bililive-go 的实现）：
//! 1. POST passport.douyu.com/scan/generateCode  → data.code（轮询码）+ data.url（二维码内容）
//! 2. GET  passport.douyu.com/japi/scan/auth?code=xxx
//!    error=0 成功 / 1 已扫待确认 / -2 未扫 / -1 过期
//! 3. error=0 时 data.url 是登录回调地址，跟随重定向收集 Set-Cookie
//!
//! 登录态能解锁原画（60fps）并避免斗鱼每 5 分钟断流。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::logln;

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
const QR_GENERATE: &str = "https://passport.douyu.com/scan/generateCode";
const QR_POLL: &str = "https://passport.douyu.com/japi/scan/auth";
const LOGIN_REFERER: &str = "https://passport.douyu.com/index/login";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
const REQ_TIMEOUT: Duration = Duration::from_secs(15);

/// 登录链路专用 HTTP agent（**必须显式挂 TLS**）。
///
/// ureq 2.x 的 `default_tls_config()` 只认 `tls`（rustls）这个 feature；本项目
/// Cargo.toml 开的是 `native-tls`，所以 `AgentBuilder::new()` 在没显式
/// `.tls_connector()` 时会退化成 NoTls，任何 HTTPS 请求直接失败：
///   "cannot make HTTPS request because no TLS backend is configured"
/// 这就是「扫码确认后换登录态必失败」的根因（生成二维码/轮询走的是
/// `douyu::agent()`，那里设过 tls_connector，所以看起来前半段一切正常）。
fn agent(redirects: u32) -> Result<ureq::Agent, String> {
    let tls = native_tls::TlsConnector::new().map_err(|e| format!("TLS 初始化失败: {e}"))?;
    Ok(ureq::AgentBuilder::new()
        .redirects(redirects)
        .try_proxy_from_env(false)
        .tls_connector(Arc::new(tls))
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout(REQ_TIMEOUT)
        .build())
}

pub struct QrSession {
    pub code: String,
    /// 二维码内容（斗鱼中间页链接），需要本地生成二维码图
    pub url: String,
}

#[derive(Debug, PartialEq)]
pub enum PollState {
    Waiting,
    Scanned,
    Expired,
    Rejected,
    /// 手机上已点确认，但换登录态失败。二维码此时已作废，重试无意义 → 终态。
    Failed(String),
    Success(String),
}

/// 生成登录二维码
pub fn generate() -> Result<QrSession, String> {
    let agent = agent(5)?;
    let resp = agent
        .post(QR_GENERATE)
        .set("User-Agent", UA)
        .set("Referer", LOGIN_REFERER)
        .set("Origin", "https://passport.douyu.com")
        .set("X-Requested-With", "XMLHttpRequest")
        .send_form(&[("client_id", "1"), ("isMultiAccount", "0")])
        .map_err(|e| format!("请求二维码失败: {e}"))?;
    let body = resp.into_string().map_err(|e| e.to_string())?;
    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|_| "二维码接口返回非 JSON（疑似风控）".to_string())?;
    if v["error"].as_i64() != Some(0) {
        return Err(format!("斗鱼返回错误: {}", v["data"]));
    }
    let code = v["data"]["code"]
        .as_str()
        .ok_or("二维码响应缺少 code")?
        .to_string();
    let url = v["data"]["url"]
        .as_str()
        .ok_or("二维码响应缺少 url")?
        .to_string();
    logln!("[login] 二维码已生成（code={}）", &code[..code.len().min(24)]);
    Ok(QrSession { code, url })
}

/// 轮询一次扫码状态
pub fn poll(code: &str) -> Result<PollState, String> {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let url = format!("{QR_POLL}?time={ms}&code={code}");
    let agent = agent(5)?;
    let resp = agent
        .get(&url)
        .set("User-Agent", UA)
        .set("Referer", LOGIN_REFERER)
        .set("X-Requested-With", "XMLHttpRequest")
        .call()
        .map_err(|e| format!("轮询失败: {}", redact(&e.to_string(), &url)))?;
    let body = resp.into_string().map_err(|e| e.to_string())?;
    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|_| "轮询返回非 JSON（疑似风控）".to_string())?;
    let err = v["error"].as_i64().ok_or("轮询响应缺少 error")?;
    let msg = v["msg"].as_str().unwrap_or("").to_string();
    match err {
        0 => {
            let cb = v["data"]["url"]
                .as_str()
                .ok_or("确认成功但缺少回调地址")?
                .to_string();
            logln!("[login] 扫码已确认，回调地址: {}", short_url(&cb));
            logln!("[login] 开始换取登录态（逐跳收集 Cookie）...");
            match exchange_cookie(&cb) {
                Ok(cookie) => {
                    let uid = cookie_value(&cookie, "acf_uid").unwrap_or_default();
                    let nick = nickname(&cookie);
                    let saved = save_cookie(&cookie);
                    logln!(
                        "[login] 登录成功 uid={uid} 昵称={nick} 登录态已保存={}",
                        saved.is_some()
                    );
                    Ok(PollState::Success(cookie))
                }
                Err(e) => {
                    logln!("[login] 换取登录态失败: {e}");
                    Ok(PollState::Failed(e))
                }
            }
        }
        1 => Ok(PollState::Scanned),
        -2 => Ok(PollState::Waiting),
        -1 => Ok(PollState::Expired),
        other => {
            logln!("[login] 扫码状态 {other}: {msg}");
            Ok(PollState::Rejected)
        }
    }
}

/// 跟随登录回调的重定向链，逐跳收集 Set-Cookie。
/// 要点（对齐 bililive-go）：
/// - 同名 cookie 后跳覆盖前跳（终点才是权威登录态）
/// - 跳过空值（空 Set-Cookie 是"清除"语义，不能拿来覆盖真值）
/// - 每跳记录 URL(脱敏)/状态/cookie 名，失败时输出完整轨迹便于定位
///
/// 注意：这里的 agent **必须**带 TLS（见 `agent()`）；且要 redirects(0)，
/// 手动跟跳才能逐跳捞 Set-Cookie。
pub fn exchange_cookie(start: &str) -> Result<String, String> {
    let agent = agent(0)?;
    let mut seen: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    let mut hops: Vec<String> = Vec::new();
    let mut cur = start.to_string();
    let mut converged = false;
    let mut last_err = String::new();

    for hop in 0..8 {
        logln!("[login] 回调第 {} 跳: {}", hop + 1, short_url(&cur));
        // 已收集的 cookie 要跟着后续跳走（agent 没开 cookie store，
        // 若不手动携带，需要「先拿临时票据再换正式票据」的链路会断）
        let carried = if seen.is_empty() {
            String::new()
        } else {
            seen.iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ")
        };
        let mut req = agent
            .get(&cur)
            .set("User-Agent", UA)
            .set("Referer", LOGIN_REFERER);
        if !carried.is_empty() {
            req = req.set("Cookie", &carried);
        }
        // 非 2xx（含 4xx/5xx）也要拿到响应头分析
        let resp = match req.call() {
            Ok(r) => r,
            Err(ureq::Error::Status(_code, r)) => r,
            Err(e) => {
                let em = redact(&e.to_string(), &cur);
                logln!("[login]   第 {} 跳请求失败: {em}", hop + 1);
                last_err = em;
                break;
            }
        };
        let status = resp.status();

        let mut names: Vec<String> = Vec::new();
        for h in resp.all("set-cookie") {
            if let Some(kv) = h.split(';').next() {
                let kv = kv.trim();
                if let Some(eq) = kv.find('=') {
                    let (k, v) = (&kv[..eq], &kv[eq + 1..]);
                    if !v.is_empty() {
                        seen.insert(k.to_string(), v.to_string());
                        names.push(k.to_string());
                    }
                }
            }
        }
        logln!(
            "[login]   状态 {status}，cookie: {}",
            if names.is_empty() {
                "无".to_string()
            } else {
                names.join(",")
            }
        );
        hops.push(format!(
            "{} -> {} [{}]",
            short_url(&cur),
            status,
            names.join(",")
        ));

        if (300..400).contains(&status) {
            match resp.header("location") {
                Some(loc) => {
                    cur = resolve_url(&cur, loc);
                    continue;
                }
                None => {
                    return Err(format!(
                        "登录回调 {status} 但无 Location，链路中断；轨迹: {}",
                        hops.join(" | ")
                    ))
                }
            }
        }
        converged = true;
        break;
    }

    if !last_err.is_empty() {
        return Err(format!(
            "回调第 {} 跳请求失败: {last_err}；轨迹: {}",
            hops.len() + 1,
            if hops.is_empty() { "无".to_string() } else { hops.join(" | ") }
        ));
    }
    if !converged {
        return Err(format!("回调 8 跳未到终点；轨迹: {}", hops.join(" | ")));
    }
    if seen.is_empty() {
        return Err(format!("回调未返回任何 Cookie；轨迹: {}", hops.join(" | ")));
    }
    logln!(
        "[login] 登录态字段({}): {}",
        seen.len(),
        seen.keys().cloned().collect::<Vec<_>>().join(",")
    );
    Ok(seen
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("; "))
}

/// 错误串脱敏：ureq 的错误 Display 会把整条 URL（含 uid/一次性 code）带出来，
/// 进日志前先替换成脱敏形式。
fn redact(msg: &str, url: &str) -> String {
    if url.is_empty() {
        return msg.to_string();
    }
    msg.replace(url, &short_url(url))
}

/// 保留 scheme://host/path + 参数名，参数值打码（一次性 token 不能进日志）
fn short_url(u: &str) -> String {
    let (base, query) = match u.find('?') {
        Some(i) => (&u[..i], Some(&u[i + 1..])),
        None => (u, None),
    };
    let masked_base = if let Some(i) = base.find("://") {
        let scheme = &base[..i];
        let rest = &base[i + 3..];
        let host_end = rest.find('/').unwrap_or(rest.len());
        format!("{scheme}://{}", &rest[..host_end.max(1).min(rest.len())])
            .to_string()
            + &rest[host_end..]
    } else {
        base.to_string()
    };
    match query {
        Some(q) if !q.is_empty() => {
            let names: Vec<String> = q
                .split('&')
                .map(|kv| {
                    let k = kv.split('=').next().unwrap_or(kv);
                    format!("{k}=***")
                })
                .collect();
            format!("{masked_base}?{}", names.join("&"))
        }
        _ => masked_base,
    }
}

/// 解析重定向 Location（相对路径拼到当前 URL 的 scheme://host 后）
fn resolve_url(base: &str, loc: &str) -> String {
    if loc.starts_with("http://") || loc.starts_with("https://") {
        return loc.to_string();
    }
    let scheme_host = match base.find("://") {
        Some(i) => match base[i + 3..].find('/') {
            Some(j) => &base[..i + 3 + j],
            None => base,
        },
        None => return loc.to_string(),
    };
    format!("{scheme_host}{loc}")
}

/// 从 "a=1; b=2" 形式的 cookie 串里取某个值
pub fn cookie_value(cookie: &str, name: &str) -> Option<String> {
    cookie.split(';').find_map(|kv| {
        let kv = kv.trim();
        kv.strip_prefix(&format!("{name}=")).map(|v| v.to_string())
    })
}

/// 登录态候选路径（按优先级）：
/// 1) 可执行文件同级（绿色版/本地构建，最直观）
/// 2) $XDG_CONFIG_HOME/douyu/cookies.txt（装到 /usr/local/bin 这类不可写目录时用）
fn cookie_candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(p) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("douyu_cookies.txt")))
    {
        v.push(p);
    }
    if let Some(cfg) = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    {
        v.push(cfg.join("douyu").join("cookies.txt"));
    }
    v
}

/// 读登录态：多个候选里取「最近修改」的那个（save 可能已回退到 XDG 目录）
pub fn load_cookie() -> Option<String> {
    let mut best: Option<(std::time::SystemTime, String)> = None;
    for p in cookie_candidates() {
        let Ok(s) = std::fs::read_to_string(&p) else {
            continue;
        };
        let s = s.trim().to_string();
        if s.is_empty() {
            continue;
        }
        let mt = std::fs::metadata(&p)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        if best.as_ref().map(|(t, _)| mt > *t).unwrap_or(true) {
            best = Some((mt, s));
        }
    }
    best.map(|(_, s)| s)
}

/// 存登录态：写到第一个可写的候选路径，返回落盘位置
pub fn save_cookie(c: &str) -> Option<PathBuf> {
    for p in cookie_candidates() {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match std::fs::write(&p, c) {
            Ok(()) => {
                logln!("[login] cookie 已写入 {}", p.display());
                return Some(p);
            }
            Err(e) => logln!("[login] cookie 写入 {} 失败: {e}", p.display()),
        }
    }
    None
}

/// 登录昵称（用于界面显示）。acf_nickname 常是 URL 编码的中文。
pub fn nickname(cookie: &str) -> String {
    match cookie_value(cookie, "acf_nickname") {
        Some(s) if !s.trim().is_empty() => percent_decode(&s),
        _ => "已登录".to_string(),
    }
}

/// 解 URL 百分号编码（%E4%B8%AD → 中）。非法/不完整编码原样保留。
fn percent_decode(s: &str) -> String {
    fn hex(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 生成二维码点阵（true=黑块）
pub fn qr_matrix(text: &str) -> Result<(usize, Vec<bool>), String> {
    // 用 L 级纠错（7% 容错）：模块更少、单块更大，屏幕扫码更容易识别
    let code = qrcode::QrCode::with_error_correction_level(
        text.as_bytes(),
        qrcode::EcLevel::L,
    )
    .map_err(|e| format!("二维码生成失败: {e}"))?;
    let w = code.width();
    let colors = code.to_colors();
    let px = colors.iter().map(|c| *c == qrcode::Color::Dark).collect();
    Ok((w, px))
}

/// 把 RGBA 像素保存成 PNG（手机相册识别的备用方案）
pub fn save_png(rgba: &[u8], size: usize, filename: &str) -> Result<PathBuf, String> {
    let path = std::env::current_dir()
        .ok()
        .map(|d| d.join(filename))
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.join(filename)))
        })
        .unwrap_or_else(|| PathBuf::from(filename));
    let file = std::fs::File::create(&path).map_err(|e| format!("创建文件失败: {e}"))?;
    let w = std::io::BufWriter::new(file);
    let mut enc = png::Encoder::new(w, size as u32, size as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().map_err(|e| format!("PNG 头写入失败: {e}"))?;
    writer
        .write_image_data(rgba)
        .map_err(|e| format!("PNG 数据写入失败: {e}"))?;
    Ok(path)
}

/// 规范化用户粘贴的 Cookie：去换行、合并多余空格，并校验含关键字段
pub fn normalize_cookie(raw: &str) -> Result<String, String> {
    let joined = raw
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let cleaned = joined.replace("; ", "; ").replace(";;", ";");
    if cookie_value(&cleaned, "acf_uid").is_none() {
        return Err("缺少 acf_uid，请从已登录的斗鱼网页复制完整 Cookie 请求头".into());
    }
    Ok(cleaned)
}
