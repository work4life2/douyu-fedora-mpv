//! 简易日志：同时输出到 stderr 和 douyu.log（可执行文件同级目录）
//!
//! 追加写入（不覆盖历史），每行带时间戳，启动时写分隔线便于区分会话。

use std::fs::OpenOptions;
use std::io::Write;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

static LOG: OnceLock<Mutex<Option<std::fs::File>>> = OnceLock::new();

/// 本地时间 HH:MM:SS（UTC+8，无需 chrono）
pub fn clock() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let t = (secs + 8 * 3600) % 86400;
    format!("{:02}:{:02}:{:02}", t / 3600, (t % 3600) / 60, t % 60)
}

pub fn init() {
    // 优先当前工作目录（用户在哪运行就在哪，最好找），失败回退 exe 目录、HOME
    let path = std::env::current_dir()
        .ok()
        .map(|d| d.join("douyu.log"))
        .filter(|p| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
                .is_ok()
        })
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.join("douyu.log")))
        })
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| std::path::PathBuf::from(h).join("douyu.log"))
        });
    match path {
        Some(p) => {
            // 追加模式：保留历史日志，不覆盖
            match OpenOptions::new().create(true).append(true).open(&p) {
                Ok(mut f) => {
                    let _ = writeln!(f, "\n========== douyu 启动 @ {} ==========", clock());
                    let _ = f.flush();
                    let _ = LOG.set(Mutex::new(Some(f)));
                    eprintln!("[log] 日志追加写入: {}", p.display());
                }
                Err(e) => {
                    let _ = LOG.set(Mutex::new(None));
                    eprintln!("[log] 无法打开日志文件 {}: {e}", p.display());
                }
            }
        }
        None => {
            let _ = LOG.set(Mutex::new(None));
        }
    }
}

/// 按字节上限截断，并保证落在 UTF-8 字符边界上。
///
/// 直接 `&s[..n]` 在中文上会 panic（"byte index is not a char boundary"），
/// 而 release 构建是 `panic = "abort"` —— 不是一个线程挂掉，是整个进程直接消失
/// 且不留任何日志。所有「截一段进日志」的地方都必须走这里。
pub fn head(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

pub fn line(s: &str) {
    if let Some(m) = LOG.get() {
        if let Ok(mut g) = m.lock() {
            if let Some(f) = g.as_mut() {
                let _ = writeln!(f, "[{}] {s}", clock());
                let _ = f.flush();
            }
        }
    }
}

/// 同时输出到 stderr 和日志文件（带时间戳）
#[macro_export]
macro_rules! logln {
    ($($arg:tt)*) => {{
        let s = format!($($arg)*);
        eprintln!("[{}] {s}", $crate::log::clock());
        $crate::log::line(&s);
    }};
}
