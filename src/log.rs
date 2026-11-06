//! 简易日志：同时输出到 stderr 和 douyu.log（可执行文件同级目录）
//!
//! 双击启动时 stderr 看不到，所以都要落盘。

use std::fs::File;
use std::io::Write;
use std::sync::{Mutex, OnceLock};

static LOG: OnceLock<Mutex<Option<File>>> = OnceLock::new();

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
        Some(p) => match File::create(&p) {
            Ok(f) => {
                let _ = LOG.set(Mutex::new(Some(f)));
                eprintln!("[log] 日志写入: {}", p.display());
            }
            Err(e) => {
                let _ = LOG.set(Mutex::new(None));
                eprintln!("[log] 无法创建日志文件 {}: {e}", p.display());
            }
        },
        None => {
            let _ = LOG.set(Mutex::new(None));
        }
    }
}

pub fn line(s: &str) {
    if let Some(m) = LOG.get() {
        if let Ok(mut g) = m.lock() {
            if let Some(f) = g.as_mut() {
                let _ = writeln!(f, "{s}");
                let _ = f.flush();
            }
        }
    }
}

/// 同时输出到 stderr 和日志文件
#[macro_export]
macro_rules! logln {
    ($($arg:tt)*) => {{
        let s = format!($($arg)*);
        eprintln!("{s}");
        $crate::log::line(&s);
    }};
}
