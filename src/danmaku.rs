//! 弹幕客户端：WebSocket + STT 二进制协议。
//!
//! 关键约束（实测踩过的坑）：
//! - 握手必须带 Sec-WebSocket-Protocol: binary（浏览器/Node 内置 WS 做不到）
//! - TLS 必须限制到 1.2（服务端拒绝 TLS1.3 握手）
//! - STT 帧：4B总长 + 4B同值 + 2B类型(689发/690收) + 1B加密 + 1B保留 + body + \0
//! - 流程：loginreq → joingroup → 服务端 pingreq 必须回 mrkl，45s 主动心跳
//! - **读超时必须设在裸 TcpStream 上**（握手前）。设在 `ws.get_ref()` 上对
//!   native-tls 分支根本不生效（拿到的不是 Plain），会导致空闲时读永久阻塞，
//!   stop 标志和心跳都失灵。
//!
//! 密集弹幕下的三处开销控制：
//! - 非 chatmsg 帧**不解码 UTF-8、不建 String**（直接按字节比 type）
//! - 字段查找零分配（不再为每个 key 拼 `format!`）
//! - 日志按时间节流（原来每 20 条一行，热房里等于持续写盘，反压读线程）

use std::io::ErrorKind;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, Instant};

use native_tls::{Protocol, TlsConnector};
use tungstenite::client::IntoClientRequest;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{client_tls_with_config, Error as WsError, Message, WebSocket};

use crate::logln;

type WsStream = MaybeTlsStream<TcpStream>;

const HOST: &str = "danmuproxy.douyu.com:8506";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(6);
/// 单次 read 的阻塞上限：只为让循环能周期性检查 stop / 心跳，不追求实时性
const READ_TIMEOUT: Duration = Duration::from_millis(500);
const HEARTBEAT: Duration = Duration::from_secs(45);
/// 日志节流：热房每秒几十条，按条打日志会把读线程拖死
const LOG_EVERY: Duration = Duration::from_secs(10);

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

/// 按 STT 帧头把二进制块切成若干条消息体（零拷贝）。
///
/// 注意 `sz < 13` 也必须丢：帧体至少要有 12B 头 + 1B '\0'，
/// 旧代码只判 `sz < 12`，遇到畸形帧会切出 `12..11` 这种区间直接 panic（整程序崩）。
fn split_frames(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut off = 0usize;
    while off + 12 <= data.len() {
        let sz = u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]) as usize;
        if sz < 13 || off + sz > data.len() {
            break;
        }
        out.push(&data[off + 12..off + sz - 1]);
        off += sz;
    }
    out
}

/// 取 `key@=value/` 里的 value（按字节，零分配）。
/// 只认「段开头就是 key@=」，避免 `xxxnn@=` 这类后缀误匹配。
fn find_field<'a>(body: &'a [u8], key: &str) -> Option<&'a [u8]> {
    let k = key.as_bytes();
    let mut start = 0usize;
    while start < body.len() {
        let end = body[start..]
            .iter()
            .position(|&b| b == b'/')
            .map(|i| start + i)
            .unwrap_or(body.len());
        let seg = &body[start..end];
        if seg.len() > k.len() + 2
            && seg.starts_with(k)
            && &seg[k.len()..k.len() + 2] == b"@="
        {
            return Some(&seg[k.len() + 2..]);
        }
        start = end + 1;
    }
    None
}

/// 还原斗鱼的值转义：`@S` → `/`，`@A` → `@`。
/// 不做这步的话，带 `/` 的弹幕会被切成半截、带 `@` 的会显示成 `@A`。
fn unescape(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'@' && i + 1 < raw.len() {
            match raw[i + 1] {
                b'S' => {
                    out.push(b'/');
                    i += 2;
                    continue;
                }
                b'A' => {
                    out.push(b'@');
                    i += 2;
                    continue;
                }
                _ => {}
            }
        }
        out.push(raw[i]);
        i += 1;
    }
    out
}

fn text_field(body: &[u8], key: &str) -> String {
    match find_field(body, key) {
        Some(v) => String::from_utf8_lossy(&unescape(v)).into_owned(),
        None => String::new(),
    }
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
    req.headers_mut().insert(
        "User-Agent",
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) Chrome/114.0.0.0 Safari/537.36"
            .parse()
            .unwrap(),
    );

    // 逐个候选地址试连（DNS 多 A 记录时，某个 IP 不通不至于整条链路失败）
    let addrs: Vec<_> = HOST
        .to_socket_addrs()
        .map_err(|e| format!("解析 {HOST} 失败: {e}"))?
        .collect();
    let mut last = String::from("无可用地址");
    let mut stream = None;
    for a in &addrs {
        match TcpStream::connect_timeout(a, CONNECT_TIMEOUT) {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(e) => last = format!("连接 {a} 失败: {e}"),
        }
    }
    let stream = stream.ok_or(last)?;
    // 读超时必须在**握手前**设在裸 TCP 上：握手后 ws.get_ref() 拿到的已是
    // NativeTls 分支，设不上超时（旧代码就是这么失效的）
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let _ = stream.set_nodelay(true);

    let connector = tungstenite::Connector::NativeTls(connector);
    let (ws, _) = client_tls_with_config(req, stream, None, Some(connector))
        .map_err(|e| format!("TLS/WS 握手失败: {e}"))?;
    Ok(ws)
}

/// 弹幕客户端主循环。tx 发送 (昵称, 内容)。stop 置 true 退出。
pub fn run(rid: String, stop: Arc<AtomicBool>, tx: Sender<Chat>) {
    let mut backoff = 3u64;
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        match connect_danmaku() {
            Ok(ws) => {
                logln!("[danmaku] 已连接房间 {rid}");
                match session(ws, &rid, &tx, &stop) {
                    Ok(()) => return, // 主动停止
                    Err(e) => {
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                        logln!("[danmaku] 断开: {e}，{backoff} 秒后重连");
                    }
                }
            }
            Err(e) => {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                logln!("[danmaku] {e}，{backoff} 秒后重连");
            }
        }
        sleep_stoppable(&stop, backoff);
        // 反复失败拉长间隔（热房封 IP 时别硬顶），成功过一次后退回 3 秒
        backoff = (backoff * 2).min(30);
    }
}

fn sleep_stoppable(stop: &Arc<AtomicBool>, secs: u64) {
    for _ in 0..secs * 4 {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        std::thread::sleep(Duration::from_millis(250));
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

    let mut last_hb = Instant::now();
    let mut total: u64 = 0;
    let mut window: u64 = 0;
    let mut last_log = Instant::now();
    let mut other_types = 0u32;

    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        if last_hb.elapsed() >= HEARTBEAT {
            ws.send(Message::Binary(encode_msg("type@=mrkl/")))
                .map_err(|e| e.to_string())?;
            last_hb = Instant::now();
        }

        match ws.read() {
            Ok(Message::Binary(data)) => {
                for body in split_frames(&data) {
                    // 先按字节判类型：非 chatmsg 完全不碰 UTF-8 解码
                    let ty = find_field(body, "type").unwrap_or(b"");
                    if ty == b"chatmsg" {
                        let txt = text_field(body, "txt");
                        if !txt.is_empty() {
                            let nn = text_field(body, "nn");
                            if tx.send((nn, txt)).is_err() {
                                return Ok(()); // 接收端已关闭，退出
                            }
                        }
                        total += 1;
                        window += 1;
                    } else if ty == b"pingreq" {
                        ws.send(Message::Binary(encode_msg("type@=mrkl/")))
                            .map_err(|e| e.to_string())?;
                    } else if ty != b"mrkl" {
                        other_types += 1;
                        if other_types <= 5 {
                            // 截断必须按字符边界：直接切字节会 panic（中文/emoji 在 120 处被切开）
                            let preview: String = String::from_utf8_lossy(body)
                                .chars()
                                .take(120)
                                .collect();
                            logln!("[danmaku] 收到类型 {}: {preview}", String::from_utf8_lossy(ty));
                        }
                    }
                }
            }
            Err(WsError::Io(e))
                if e.kind() == ErrorKind::WouldBlock
                    || e.kind() == ErrorKind::TimedOut
                    || e.kind() == ErrorKind::Interrupted =>
            {
                // 读超时 / 信号中断：回到循环顶检查 stop 和心跳
            }
            Ok(Message::Close(_)) => return Err("服务端关闭连接".to_string()),
            Err(e) => return Err(format!("{e}")),
            Ok(_) => {}
        }

        // 节流日志：10 秒一行，带速率，热房也不写盘
        if last_log.elapsed() >= LOG_EVERY {
            let secs = last_log.elapsed().as_secs_f32().max(0.001);
            logln!(
                "[danmaku] 累计 {total} 条，近 {}s {:.1} 条/秒",
                secs.round(),
                window as f32 / secs
            );
            last_log = Instant::now();
            window = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 按服务端下发的帧布局造一帧。
    /// 注意长度字段是**整帧字节数**（= 8B 双长度 + 4B 类型/标记 + body + 1B '\0'），
    /// 解码端就是按这个语义切片的。
    fn frame(body: &str) -> Vec<u8> {
        let b = body.as_bytes();
        let len = b.len() + 13;
        let mut out = Vec::with_capacity(len);
        out.extend_from_slice(&(len as u32).to_le_bytes());
        out.extend_from_slice(&(len as u32).to_le_bytes());
        out.extend_from_slice(&690u16.to_le_bytes());
        out.push(0);
        out.push(0);
        out.extend_from_slice(b);
        out.push(0);
        out
    }

    #[test]
    fn split_multiple_frames() {
        let mut buf = frame("type@=chatmsg/txt@=a/");
        buf.extend_from_slice(&frame("type@=chatmsg/txt@=b/"));
        let parts = split_frames(&buf);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0], b"type@=chatmsg/txt@=a/");
        assert_eq!(parts[1], b"type@=chatmsg/txt@=b/");
    }

    /// 帧长 12（body 为空）在旧代码里会切出 12..11 直接 panic，这里必须安全丢弃
    #[test]
    fn malformed_frame_does_not_panic() {
        let mut bad = Vec::new();
        bad.extend_from_slice(&12u32.to_le_bytes());
        bad.extend_from_slice(&12u32.to_le_bytes());
        bad.extend_from_slice(&[0u8; 20]);
        assert!(split_frames(&bad).is_empty());
        // 后半截拼接的残帧也不能切出错误内容
        let mut mixed = frame("type@=chatmsg/txt@=ok/");
        mixed.extend_from_slice(&bad);
        let parts = split_frames(&mixed);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0], b"type@=chatmsg/txt@=ok/");
    }

    #[test]
    fn field_lookup_no_prefix_false_positive() {
        // "xnn@=" 不能命中 key="nn"
        let body = b"type@=chatmsg/xnn@=bad/nn@=good/txt@=hi/";
        assert_eq!(find_field(body, "nn").unwrap(), b"good");
        assert_eq!(find_field(body, "type").unwrap(), b"chatmsg");
        assert!(find_field(body, "nnn").is_none());
    }

    #[test]
    fn escape_roundtrip() {
        // 斗鱼把 / 转义成 @S、@ 转义成 @A，取出来后要还原
        let body = b"type@=chatmsg/nn@=a@Sb/txt@=1+1@A=2/";
        assert_eq!(text_field(body, "nn"), "a/b");
        assert_eq!(text_field(body, "txt"), "1+1@=2");
    }

    #[test]
    fn only_chatmsg_counts() {
        let body = b"type@=uenter/nn@=someone/";
        assert_ne!(find_field(body, "type").unwrap(), b"chatmsg");
        assert_eq!(find_field(body, "type").unwrap(), b"uenter");
    }
}
