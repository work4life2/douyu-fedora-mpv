//! 弹幕客户端：WebSocket + STT 二进制协议。
//!
//! 关键约束（实测踩过的坑）：
//! - 握手必须带 Sec-WebSocket-Protocol: binary（浏览器/Node 内置 WS 做不到）
//! - TLS 必须限制到 1.2（服务端拒绝 TLS1.3 握手）
//! - STT 帧：4B总长 + 4B同值 + 2B类型(689发/690收) + 1B加密 + 1B保留 + body + \0
//! - 流程：loginreq → joingroup → 服务端 pingreq 必须回 mrkl，45s 主动心跳

use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

use native_tls::{Protocol, TlsConnector};
use tungstenite::client::IntoClientRequest;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{client_tls_with_config, Error as WsError, Message, WebSocket};
use crate::logln;

type WsStream = MaybeTlsStream<TcpStream>;

/// 一条弹幕（昵称, 内容）
pub type Chat = (String, String);

fn encode_msg(msg: &str) -> Vec<u8> {
    let body = msg.as_bytes();
    let len = body.len() + 9;
    let mut out = Vec::with_capacity(len);
    out.extend_from_slice(&(len as u32).to_le_bytes());
    out.extend_from_slice(&(len as u32).to_le_bytes());
    out.extend_from_slice(&689u16.to_le_bytes());
    out.push(0);
    out.push(0);
    out.extend_from_slice(body);
    out.push(0);
    out
}

/// 从二进制帧拆出若干条 STT 消息
fn decode_msgs(data: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut off = 0usize;
    while off + 12 <= data.len() {
        let sz = u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
            as usize;
        if sz < 12 || off + sz > data.len() {
            break;
        }
        let body = &data[off + 12..off + sz - 1];
        out.push(String::from_utf8_lossy(body).into_owned());
        off += sz;
    }
    out
}

fn field<'a>(msg: &'a str, key: &str) -> Option<&'a str> {
    msg.split('/')
        .find_map(|kv| kv.strip_prefix(&format!("{key}@=")))
}

/// 建立到斗鱼弹幕服务器的连接（TLS1.2 + openssl，支持静态 RSA 密钥交换）
fn connect_danmaku() -> Result<WebSocket<WsStream>, String> {
    // 斗鱼弹幕服务器只用静态 RSA 密钥交换（TLS_RSA_WITH_AES_256_GCM_SHA384），
    // rustls 不支持该老式 cipher，必须用 openssl（native-tls）并限制 TLS1.2。
    let connector = TlsConnector::builder()
        .min_protocol_version(Some(Protocol::Tlsv12))
        .max_protocol_version(Some(Protocol::Tlsv12))
        .build()
        .map_err(|e| format!("构建 TLS connector 失败: {e}"))?;

    let mut req = "wss://danmuproxy.douyu.com:8506/"
        .into_client_request()
        .map_err(|e| e.to_string())?;
    req.headers_mut()
        .insert("Origin", "https://www.douyu.com".parse().unwrap());
    req.headers_mut()
        .insert(
            "User-Agent",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) Chrome/114.0.0.0 Safari/537.36"
                .parse()
                .unwrap(),
        );

    let stream = TcpStream::connect("danmuproxy.douyu.com:8506")
        .map_err(|e| format!("TCP 连接失败: {e}"))?;
    let connector = tungstenite::Connector::NativeTls(connector);
    let (ws, _) = client_tls_with_config(req, stream, None, Some(connector))
        .map_err(|e| format!("TLS/WS 握手失败: {e}"))?;
    // 读超时：让会话循环能周期检查停止标志 / 心跳
    if let MaybeTlsStream::Plain(s) = ws.get_ref() {
        let _ = s.set_read_timeout(Some(Duration::from_secs(1)));
    }
    Ok(ws)
}

/// 弹幕客户端主循环。tx 发送 (昵称, 内容)。stop 置 true 退出。
pub fn run(rid: String, stop: Arc<AtomicBool>, tx: Sender<Chat>) {
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        match connect_danmaku() {
            Ok(ws) => {
                logln!("[danmaku] 已连接房间 {rid}");
                if let Err(e) = session(ws, &rid, &tx, &stop) {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    logln!("[danmaku] 断开: {e}，3 秒后重连");
                } else {
                    return; // 主动停止
                }
            }
            Err(e) => {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                logln!("[danmaku] {e}，3 秒后重连");
            }
        }
        for _ in 0..30 {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

fn session(
    mut ws: WebSocket<WsStream>,
    rid: &str,
    tx: &Sender<Chat>,
    stop: &Arc<AtomicBool>,
) -> Result<(), String> {
    ws.send(Message::Binary(encode_msg(&format!(
        "type@=loginreq/roomid@={rid}/"
    ))))
    .map_err(|e| e.to_string())?;
    ws.send(Message::Binary(encode_msg(&format!(
        "type@=joingroup/rid@={rid}/gid@=1/"
    ))))
    .map_err(|e| e.to_string())?;

    let mut last_hb = std::time::Instant::now();
    let mut msg_count: u64 = 0;
    let mut other_types: u64 = 0;
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        if last_hb.elapsed() >= Duration::from_secs(45) {
            ws.send(Message::Binary(encode_msg("type@=mrkl/")))
                .map_err(|e| e.to_string())?;
            last_hb = std::time::Instant::now();
        }

        match ws.read() {
            Ok(Message::Binary(data)) => {
                for m in decode_msgs(&data) {
                    let t = field(&m, "type").unwrap_or("");
                    match t {
                        "pingreq" => {
                            ws.send(Message::Binary(encode_msg("type@=mrkl/")))
                                .map_err(|e| e.to_string())?;
                        }
                        "chatmsg" => {
                            let nn = field(&m, "nn").unwrap_or("").to_string();
                            let txt = field(&m, "txt").unwrap_or("").to_string();
                            if !txt.is_empty() && tx.send((nn, txt)).is_err() {
                                return Ok(()); // 接收端已关闭，退出
                            }
                            msg_count += 1;
                            if msg_count % 20 == 1 {
                                logln!(
                                    "[danmaku] 已收 {msg_count} 条（最新: {}）",
                                    field(&m, "txt").unwrap_or("")
                                );
                            }
                        }
                        other => {
                            other_types += 1;
                            // 首次收到非 chatmsg 类型时记录，便于诊断（如 loginres/error）
                            if other_types <= 5 && other != "mrkl" {
                                logln!("[danmaku] 收到类型 {other}: {}", &m[..m.len().min(120)]);
                            }
                        }
                    }
                }
            }
            Err(WsError::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                // 读超时：回到循环顶检查 stop / 心跳
            }
            Ok(Message::Close(_)) => return Err("服务端关闭连接".to_string()),
            Err(e) => return Err(format!("{e}")),
            Ok(_) => {}
        }
    }
}
