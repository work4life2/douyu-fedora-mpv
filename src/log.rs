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
    let path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("douyu.log")))
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
