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
            logln!("[login] 扫码确认，正在换取登录态...");
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

/// 跟随登录回调的重定向链，收集所有 Set-Cookie
fn exchange_cookie(start: &str) -> Result<String, String> {
    let agent = ureq::AgentBuilder::new()
        .redirects(0)
        .try_proxy_from_env(false)
        .build();
    let mut cookies: Vec<String> = Vec::new();
    let mut cur = start.to_string();
    for _ in 0..8 {
        let resp = agent
            .get(&cur)
            .set("User-Agent", UA)
            .call()
            .map_err(|e| format!("登录回调请求失败: {e}"))?;
        for h in resp.all("set-cookie") {
            if let Some(kv) = h.split(';').next() {
                let kv = kv.trim();
                if !kv.is_empty() && !cookies.iter().any(|c| c.starts_with(&format!("{}=", kv.split('=').next().unwrap_or("")))) {
                    cookies.push(kv.to_string());
                }
            }
        }
        let status = resp.status();
        if (300..400).contains(&status) {
            match resp.header("location") {
                Some(loc) => {
                    cur = resolve_url(&cur, loc);
                    continue;
                }
                None => break,
            }
        }
        break;
    }
    if cookies.is_empty() {
        return Err("登录回调未返回任何 Cookie".to_string());
    }
    Ok(cookies.join("; "))
}

fn resolve_url(base: &str, loc: &str) -> String {
    if loc.starts_with("http://") || loc.starts_with("https://") {
        return loc.to_string();
    }
    // 相对路径：拼到 scheme://host 后
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

fn save_cookie(c: &str) {
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
    let code = qrcode::QrCode::new(text.as_bytes())
        .map_err(|e| format!("二维码生成失败: {e}"))?;
    let w = code.width();
    let colors = code.to_colors();
    let px = colors.iter().map(|c| *c == qrcode::Color::Dark).collect();
    Ok((w, px))
}
