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

use crate::logln;

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
const QR_GENERATE: &str = "https://passport.douyu.com/scan/generateCode";
const QR_POLL: &str = "https://passport.douyu.com/japi/scan/auth";
const LOGIN_REFERER: &str = "https://passport.douyu.com/index/login";

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
    Success(String),
}

/// 生成登录二维码
pub fn generate() -> Result<QrSession, String> {
    let agent = crate::douyu::agent();
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
    let agent = crate::douyu::agent();
    let resp = agent
        .get(&url)
        .set("User-Agent", UA)
        .set("Referer", LOGIN_REFERER)
        .set("X-Requested-With", "XMLHttpRequest")
        .call()
        .map_err(|e| format!("轮询失败: {e}"))?;
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
            let cookie = exchange_cookie(&cb)?;
            let uid = cookie_value(&cookie, "acf_uid").unwrap_or_default();
            save_cookie(&cookie);
            logln!("[login] 登录成功 uid={uid}（cookie 已保存）");
            Ok(PollState::Success(cookie))
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
fn exchange_cookie(start: &str) -> Result<String, String> {
    let agent = ureq::AgentBuilder::new()
        .redirects(0)
        .try_proxy_from_env(false)
        .build();
    let mut seen: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    let mut hops: Vec<String> = Vec::new();
    let mut cur = start.to_string();
    let mut converged = false;

    for hop in 0..8 {
        logln!("[login] 回调第 {} 跳: {}", hop + 1, short_url(&cur));
        // 非 2xx（含 4xx/5xx）也要拿到响应头分析
        let resp = match agent
            .get(&cur)
            .set("User-Agent", UA)
            .set("Referer", LOGIN_REFERER)
            .call()
        {
            Ok(r) => r,
            Err(ureq::Error::Status(_code, r)) => r,
            Err(e) => {
                logln!("[login]   第 {} 跳请求失败: {e}", hop + 1);
                return Err(format!("回调请求失败: {e}"));
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

fn cookie_path() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("douyu_cookies.txt")))
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".douyu_cookies.txt"))
        })
}

pub fn load_cookie() -> Option<String> {
    let p = cookie_path()?;
    let s = std::fs::read_to_string(p).ok()?;
    let s = s.trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

pub fn save_cookie(c: &str) {
    if let Some(p) = cookie_path() {
        if std::fs::write(&p, c).is_ok() {
            logln!("[login] cookie 已写入 {}", p.display());
        }
    }
}

/// 登录昵称（用于界面显示）
pub fn nickname(cookie: &str) -> String {
    cookie_value(cookie, "acf_nickname")
        .map(|s| {
            // 可能是 URL 编码
            s.replace("%20", " ")
        })
        .unwrap_or_else(|| "已登录".to_string())
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
