//! FLV 续流中转：解决斗鱼直链 300 秒强制断开。
//!
//! 原理（参考 DTV 的 flv_relay.rs）：
//! - 斗鱼直链 TTL 固定 300s，到点 CDN 主动 EOF。
//! - 本地起 HTTP 服务，播放器连本地；中转线程从 CDN 拉流转发。
//! - token 到期前 5s 预取下一条直链；旧流 EOF 时在 FLV tag 边界接续。
//! - 斗鱼 FLV tag 用推流会话绝对时间戳，接续无需改写时间戳。
//!
//! 状态共享：`Arc<Mutex<(rid, quality)>>`，换房间/换画质只需改状态，
//! 中转循环检测变化后取新流并在 tag 边界无缝拼接。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const FLV_HEADER: [u8; 9] = [b'F', b'L', b'V', 1, 0b0000_0101, 0, 0, 0, 9];
/// FLV header 后紧跟的 PreviousTagSize0（恒为 0）
const PREV_TAG_SIZE0: [u8; 4] = [0, 0, 0, 0];
const STREAM_TTL: Duration = Duration::from_secs(300);
const RENEW_LEAD: Duration = Duration::from_secs(5);
const RETRY_DELAY: Duration = Duration::from_millis(800);

pub type StreamState = Arc<Mutex<(String, i32)>>;

/// 一个 FLV tag（不含 11 字节 tag 头和 4 字节 previous-tag-size）
struct Tag {
    tag_type: u8,
    timestamp: u32,
    data: Vec<u8>,
}

impl Tag {
    fn is_script(&self) -> bool {
        self.tag_type == 18
    }
    fn is_video(&self) -> bool {
        self.tag_type == 9
    }
    fn is_audio(&self) -> bool {
        self.tag_type == 8
    }
    fn is_sequence_header(&self) -> bool {
        if self.data.len() < 2 {
            return false;
        }
        (self.is_video() || self.is_audio()) && self.data[1] == 0
    }
    fn is_keyframe(&self) -> bool {
        self.is_video() && !self.data.is_empty() && (self.data[0] >> 4) == 1
    }
}

struct FlvSource {
    reader: Box<dyn Read + Send>,
    video_ts: u32,
}

impl FlvSource {
    fn open(url: &str) -> Result<Self, String> {
        let agent = crate::douyu::agent();
        let resp = agent
            .get(url)
            .set(
                "User-Agent",
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) Chrome/114.0.0.0 Safari/537.36",
            )
            .call()
            .map_err(|e| format!("连接 CDN 失败: {e}"))?;
        let mut reader: Box<dyn Read + Send> = resp.into_reader();
        let mut header = [0u8; 9];
        reader
            .read_exact(&mut header)
            .map_err(|e| format!("读 FLV 头失败: {e}"))?;
        if &header[..3] != b"FLV" {
            return Err("非 FLV 流".to_string());
        }
        let mut prev0 = [0u8; 4];
        reader
            .read_exact(&mut prev0)
            .map_err(|e| format!("读 PreviousTagSize0 失败: {e}"))?;
        Ok(FlvSource {
            reader,
            video_ts: 0,
        })
    }

    fn read_tag(&mut self) -> Result<Option<Tag>, String> {
        let mut th = [0u8; 11];
        match self.reader.read_exact(&mut th) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(format!("读 tag 头失败: {e}")),
        }
        let tag_type = th[0];
        let data_size = ((th[1] as usize) << 16) | ((th[2] as usize) << 8) | th[3] as usize;
        let ts = ((th[4] as u32) << 16) | ((th[5] as u32) << 8) | th[6] as u32;
        let timestamp = ((th[7] as u32) << 24) | ts;

        let mut data = vec![0u8; data_size];
        self.reader
            .read_exact(&mut data)
            .map_err(|e| format!("读 tag 数据失败: {e}"))?;
        let mut pts = [0u8; 4];
        self.reader
            .read_exact(&mut pts)
            .map_err(|e| format!("读 previous tag size 失败: {e}"))?;

        Ok(Some(Tag {
            tag_type,
            timestamp,
            data,
        }))
    }
}

fn tag_to_bytes(tag: &Tag) -> Vec<u8> {
    let mut out = Vec::with_capacity(11 + tag.data.len() + 4);
    out.push(tag.tag_type);
    let n = tag.data.len();
    out.push(((n >> 16) & 0xff) as u8);
    out.push(((n >> 8) & 0xff) as u8);
    out.push((n & 0xff) as u8);
    let ts = tag.timestamp & 0xffffff;
    out.push(((ts >> 16) & 0xff) as u8);
    out.push(((ts >> 8) & 0xff) as u8);
    out.push((ts & 0xff) as u8);
    out.push(((tag.timestamp >> 24) & 0xff) as u8);
    out.extend_from_slice(&[0, 0, 0]);
    out.extend_from_slice(&tag.data);
    let prev = 11 + tag.data.len() as u32;
    out.extend_from_slice(&prev.to_be_bytes());
    out
}

pub struct Relay {
    state: StreamState,
}

impl Relay {
    pub fn new(state: StreamState) -> Self {
        Relay { state }
    }

    /// 启动中转，返回本地监听端口
    pub fn start(self) -> std::io::Result<u16> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                if let Ok(stream) = conn {
                    if let Err(e) = self.handle(stream) {
                        eprintln!("[relay] 连接结束: {e}");
                    }
                }
            }
        });
        Ok(port)
    }

    fn fetch(&self, rid: &str, q: i32) -> Result<FlvSource, String> {
        let s = crate::douyu::get_stream(rid, q)?;
        FlvSource::open(&s.url)
    }

    fn handle(&self, mut stream: TcpStream) -> Result<(), String> {
        // 读 HTTP 请求头
        let mut buf = [0u8; 4096];
        let mut req = Vec::new();
        loop {
            let n = stream.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            req.extend_from_slice(&buf[..n]);
            if req.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: video/x-flv\r\nConnection: close\r\n\r\n")
            .map_err(|e| e.to_string())?;

        stream
            .write_all(&FLV_HEADER)
            .map_err(|e| e.to_string())?;
        stream
            .write_all(&PREV_TAG_SIZE0)
            .map_err(|e| e.to_string())?;

        let mut current = self.state.lock().unwrap().clone();
        // 等待有效的房间号
        while current.0.is_empty() {
            std::thread::sleep(RETRY_DELAY);
            current = self.state.lock().unwrap().clone();
        }

        let mut src = self.fetch(&current.0, current.1)?;
        let mut renewal: Option<FlvSource> = None;
        let mut next_renew = Instant::now() + STREAM_TTL - RENEW_LEAD;

        loop {
            // 房间/画质变化 → 无缝切流
            let want = self.state.lock().unwrap().clone();
            if want != current && !want.0.is_empty() {
                eprintln!(
                    "[relay] 切流: rid={}\u{2192}{}, quality={}\u{2192}{}",
                    current.0, want.0, current.1, want.1
                );
                current = want;
                let old_vts = src.video_ts;
                match crate::douyu::get_stream(&current.0, current.1)
                    .and_then(|s| FlvSource::open(&s.url))
                {
                    Ok(mut new_src) => {
                        self.splice(&mut new_src, old_vts, &mut stream)?;
                        src = new_src;
                        renewal = None;
                        next_renew = Instant::now() + STREAM_TTL - RENEW_LEAD;
                        continue;
                    }
                    Err(e) => eprintln!("[relay] 切流失败: {e}"),
                }
            }

            // 到期前预取
            if renewal.is_none() && Instant::now() >= next_renew {
                match self.fetch(&current.0, current.1) {
                    Ok(s) => renewal = Some(s),
                    Err(e) => eprintln!("[relay] 预取失败: {e}"),
                }
            }

            match src.read_tag() {
                Ok(Some(tag)) => {
                    if tag.is_video() {
                        src.video_ts = tag.timestamp;
                    }
                    stream
                        .write_all(&tag_to_bytes(&tag))
                        .map_err(|e| e.to_string())?;
                }
                Ok(None) | Err(_) => {
                    let old_vts = src.video_ts;
                    let mut new_src = match renewal.take() {
                        Some(s) => s,
                        None => loop {
                            match self.fetch(&current.0, current.1) {
                                Ok(s) => break s,
                                Err(e) => {
                                    eprintln!("[relay] 重连失败: {e}");
                                    std::thread::sleep(RETRY_DELAY);
                                }
                            }
                        },
                    };
                    self.splice(&mut new_src, old_vts, &mut stream)?;
                    src = new_src;
                    next_renew = Instant::now() + STREAM_TTL - RENEW_LEAD;
                }
            }
        }
    }

    /// 接续：丢弃 script/seq header/已过期 tag，从视频关键帧接上。
    /// 保护：若丢弃超过 3MB 仍未找到关键帧（时间戳可能重置），放弃过滤直接转发。
    fn splice(
        &self,
        new_src: &mut FlvSource,
        old_video_ts: u32,
        stream: &mut TcpStream,
    ) -> Result<(), String> {
        let mut started = false;
        let mut discarded: usize = 0;
        while let Some(tag) = new_src.read_tag()? {
            if tag.is_script() || tag.is_sequence_header() {
                continue;
            }
            if !started {
                if tag.is_video() && tag.is_keyframe() && tag.timestamp > old_video_ts {
                    started = true;
                } else {
                    discarded += tag.data.len() + 15;
                    if discarded > 3_000_000 {
                        eprintln!(
                            "[relay] splice 丢弃 {discarded}B 仍未找到关键帧，直接转发（时间戳可能已重置）"
                        );
                        started = true;
                    } else {
                        continue;
                    }
                }
            }
            if tag.is_video() {
                new_src.video_ts = tag.timestamp;
            }
            stream
                .write_all(&tag_to_bytes(&tag))
                .map_err(|e| e.to_string())?;
        }
        let _ = old_video_ts;
        Ok(())
    }
}
