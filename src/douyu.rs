//! 斗鱼取流：签名脚本 → 签名 → getH5Play → FLV 直链
//! 以及房间信息 / 大厅列表接口。

#![allow(dead_code)] // room_info/list 等供未来扩展，GUI 版暂未使用

use crate::logln;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/114.0.0.0 Safari/537.36";
// 设备指纹，造一个固定值即可（与网页端行为一致）
const DID: &str = "10000000000000000000000000001501";

/// 取流链路复用的 HTTP agent（native-tls/openssl）。
///
/// 一次取流会发好几个请求（签名脚本 + getH5Play，外加 300s 续流预取、切画质重取），
/// 之前每次都新建 TlsConnector —— 等于每次都重新加载一遍系统根证书，纯属浪费。
/// ureq 的 Agent 内部是 Arc + 连接池，clone 廉价且线程安全。
static AGENT: OnceLock<ureq::Agent> = OnceLock::new();

pub fn agent() -> ureq::Agent {
    // 用 openssl（native-tls）而非 rustls：斗鱼 CDN 与弹幕服务器一样用静态 RSA
    // 密钥交换，rustls 不支持。openssl 默认协商 TLS1.2 + RSA cipher。
    if let Some(a) = AGENT.get() {
        return a.clone();
    }
    let tls = native_tls::TlsConnector::new().expect("创建 TLS connector 失败");
    let a = ureq::AgentBuilder::new()
        .try_proxy_from_env(false)
        .tls_connector(std::sync::Arc::new(tls))
        .build();
    let _ = AGENT.set(a.clone());
    a
}

fn get(url: &str, referer: &str) -> Result<String, String> {
    let resp = agent()
        .get(url)
        .set("User-Agent", UA)
        .set("Referer", referer)
        .set("Accept-Language", "zh-CN,zh;q=0.9")
        .call()
        .map_err(|e| format!("请求失败 {}: {}", url, e))?;
    resp.into_string()
        .map_err(|e| format!("读取响应失败: {}", e))
}

fn post(url: &str, referer: &str, body: &str) -> Result<String, String> {
    let mut req = agent()
        .post(url)
        .set("User-Agent", UA)
        .set("Referer", referer)
        .set("Content-Type", "application/x-www-form-urlencoded");
    // 带登录态（解锁原画、避免 5 分钟断流）
    if let Some(c) = crate::login::load_cookie() {
        req = req.set("Cookie", &c);
    }
    let resp = req
        .send_string(body)
        .map_err(|e| format!("请求失败 {}: {}", url, e))?;
    resp.into_string()
        .map_err(|e| format!("读取响应失败: {}", e))
}

/// 当前使用的设备号：登录态优先用 cookie 里的 acf_did（否则签名与登录态不匹配会被降级）
fn current_did() -> String {
    if let Some(c) = crate::login::load_cookie() {
        if let Some(d) = crate::login::cookie_value(&c, "acf_did") {
            if !d.is_empty() {
                return d;
            }
        }
    }
    DID.to_string()
}

/// 按房间获取签名脚本（脚本按房间下发，不能复用）
pub fn fetch_sign_script(rid: &str) -> Result<String, String> {
    let url = format!("https://www.douyu.com/swf_api/homeH5Enc?rids={rid}");
    let body = get(&url, &format!("https://www.douyu.com/{rid}"))?;
    #[derive(Deserialize)]
    struct R {
        data: HashMap<String, String>,
    }
    let r: R = serde_json::from_str(&body).map_err(|e| format!("签名脚本解析失败: {e}"))?;
    r.data
        .get(&format!("room{rid}"))
        .cloned()
        .ok_or_else(|| "响应中缺少房间签名脚本".to_string())
}

#[derive(Deserialize)]
struct H5Resp {
    error: i32,
    #[serde(default)]
    msg: Option<String>,
    // data 可能是对象，也可能是 ""（下播/异常/频控时），用 Value 宽松解析
    #[serde(default)]
    data: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct H5Data {
    rtmp_url: String,
    rtmp_live: String,
    #[serde(default)]
    multirates: Vec<Rate>,
}

#[derive(Deserialize, Clone)]
pub struct Rate {
    #[allow(dead_code)]
    pub name: String,
    pub rate: i32,
    #[allow(dead_code)]
    #[serde(default)]
    pub bit: Option<i64>,
    /// 该档位的独立播放地址（顶层 rtmp_live 常是默认转码档，如 _4000）
    #[serde(default)]
    pub url: Option<String>,
}

/// 取流结果
pub struct Stream {
    pub url: String,
    #[allow(dead_code)]
    pub rates: Vec<Rate>,
}

/// 签名 + getH5Play 取流
pub fn get_stream(rid: &str, quality: i32) -> Result<Stream, String> {
    let script = fetch_sign_script(rid)?;
    let tt = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_secs() as i64;

    let did = current_did();
    logln!(
        "[stream] 取流请求 rid={rid} quality={quality} 登录态={} did={}…",
        if crate::login::load_cookie().is_some() { "已登录" } else { "游客" },
        &did[..did.len().min(10)]
    );
    let signed = crate::sign::sign(rid, &did, tt, &script)?;
    // cdn 参数影响斗鱼下发的线路/转码：ws-h5（H5 端）可能给 30fps 转码流，
    // ws（PC 网页端）通常给原画原始帧率。可用 DOUYU_CDN 覆盖。
    let cdn = std::env::var("DOUYU_CDN").unwrap_or_else(|_| "ws".to_string());
    // DOUYU_EXTRA 可追加任意调试参数（如 "&substream=3&basesub=6"）
    let extra = std::env::var("DOUYU_EXTRA").unwrap_or_default();
    let body = format!("{signed}&cdn={cdn}&rate={quality}&iar=0&ive=0&hevc=0&fa=0{extra}");

    let url = format!("https://www.douyu.com/lapi/live/getH5Play/{rid}");
    let resp_body = post(&url, &format!("https://www.douyu.com/{rid}"), &body)?;
    // 调试：DOUYU_DEBUG=1 时记录原始响应（确认 multirates 是否带 url 字段）。
    // 截断必须走字符边界：resp_body 是含中文的 JSON，直接 &resp_body[..2500]
    // 有 2/3 概率切在汉字中间 → panic，而 panic=abort 等于整个进程消失。
    if std::env::var("DOUYU_DEBUG").ok().as_deref() == Some("1") {
        logln!(
            "[stream] 原始响应({}B): {}",
            resp_body.len(),
            crate::log::head(&resp_body, 2500)
        );
    }
    let resp: H5Resp =
        serde_json::from_str(&resp_body).map_err(|e| format!("取流响应解析失败: {e}"))?;

    if resp.error != 0 {
        let msg = resp.msg.unwrap_or_default();
        return Err(match resp.error {
            -9 => "时间戳/签名错误（可能是频控，稍后再试）".to_string(),
            other => format!("取流失败 error={other} msg={msg}"),
        });
    }

    // data 为对象时解析，否则给出明确错误（下播/异常）
    let d: H5Data = match resp.data {
        Some(v) => serde_json::from_value(v)
            .map_err(|e| format!("取流 data 解析失败: {e}"))?,
        None => return Err("该房间未开播或不可用".to_string()),
    };
    // 优先用 multirates 里对应档位的独立 url。
    // 顶层 rtmp_url/rtmp_live 是服务端默认档（实测常是 _4000 转码流，30fps），
    // 原画（rate=0）的真实流在 multirates[i].url 里（60fps、非 _4000 后缀）。
    let chosen = d.multirates.iter().find(|r| r.rate == quality);
    let (url, src_kind) = match chosen.and_then(|r| r.url.clone()) {
        Some(u) => (u, "multirates.url"),
        None => (
            format!("{}/{}", d.rtmp_url.trim_end_matches('/'), d.rtmp_live),
            "top-level(rtmp_live)",
        ),
    };
    // 日志只记录主机部分（避免 token 刷屏，也不泄露完整鉴权串）
    let host = url.split("://").nth(1).unwrap_or("").split('/').next().unwrap_or("");
    let tail = url.split('/').next_back().unwrap_or("");
    let tail_short = if tail.len() > 46 { &tail[..46] } else { tail };
    logln!(
        "[stream] 取流成功 rid={rid} quality={quality} cdn={cdn} host={host} 来源={src_kind} 流={tail_short} 档位={}",
        d.multirates
            .iter()
            .map(|r| format!(
                "{}:{}[url={}]",
                r.rate,
                r.name,
                if r.url.is_some() { "有" } else { "无" }
            ))
            .collect::<Vec<_>>()
            .join(",")
    );
    Ok(Stream {
        url: url.clone(),
        rates: d.multirates,
    })
}

#[derive(Deserialize)]
pub struct Room {
    #[serde(default)]
    pub room_name: Option<String>,
    #[serde(default)]
    pub nickname: Option<String>,
    #[serde(default)]
    pub show_status: Option<i64>,
    #[serde(default)]
    pub cate_name: Option<String>,
}

/// 房间信息
pub fn room_info(rid: &str) -> Result<Room, String> {
    let url = format!("https://www.douyu.com/betard/{rid}");
    let body = get(&url, &format!("https://www.douyu.com/{rid}"))?;
    #[derive(Deserialize)]
    struct R {
        room: Option<Room>,
    }
    let r: R = serde_json::from_str(&body).map_err(|e| format!("房间信息解析失败: {e}"))?;
    r.room.ok_or_else(|| "未获取到房间信息".to_string())
}

#[derive(Deserialize)]
pub struct LiveRoom {
    #[serde(default)]
    pub rid: Option<serde_json::Value>,
    #[serde(default)]
    pub rn: Option<String>,
    #[serde(default)]
    pub nn: Option<String>,
    #[serde(default)]
    pub ol: Option<serde_json::Value>,
}

/// 大厅列表。cate 为二级分类 id，默认 1（推荐）
pub fn list(cate: Option<&str>, page: u32) -> Result<Vec<LiveRoom>, String> {
    let cid = cate.unwrap_or("1");
    let url = format!("https://www.douyu.com/gapi/rkc/directory/2_{cid}/{page}");
    let body = get(&url, "https://www.douyu.com/directory/all")?;
    #[derive(Deserialize)]
    struct D {
        #[serde(default)]
        rl: Vec<LiveRoom>,
    }
    #[derive(Deserialize)]
    struct R {
        #[serde(default)]
        data: Option<D>,
    }
    let r: R = serde_json::from_str(&body).map_err(|e| format!("列表解析失败: {e}"))?;
    Ok(r.data.map(|d| d.rl).unwrap_or_default())
}
