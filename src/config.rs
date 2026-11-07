//! 本地配置：记住上次看的直播间、待机策略、弹幕开关、画质档位。
//!
//! 落盘位置与 cookie 同一套优先级（贴近用户直觉，绿色版也好找）：
//! 1) 可执行文件同级 `douyu_config.txt`
//! 2) `$XDG_CONFIG_HOME/douyu/config.txt`（装到只读目录时用）
//!
//! 格式是极简 `key=value` 文本：人能看懂、坏了能手改、不引依赖。

use std::path::PathBuf;

use crate::logln;

const EXE_NAME: &str = "douyu_config.txt";

#[derive(Debug, Clone)]
pub struct Config {
    /// 上一次成功加载的直播间房间号（纯数字）
    pub last_room: Option<String>,
    /// 播放时阻止系统自动待机（屏保/休眠）
    pub prevent_sleep: bool,
    /// 弹幕总开关（下次启动沿用）
    pub danmaku_on: bool,
    /// 画质档位：0 原画 / 4 蓝光4M / 3 超清 / 2 高清
    pub quality: i32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            last_room: None,
            prevent_sleep: true,
            danmaku_on: true,
            quality: 0,
        }
    }
}

fn candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(p) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(EXE_NAME)))
    {
        v.push(p);
    }
    if let Some(cfg) = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    {
        v.push(cfg.join("douyu").join("config.txt"));
    }
    v
}

impl Config {
    pub fn load() -> Self {
        // 多个候选里取「最近修改」的：装到只读目录时会写到 XDG，
        // 如果按顺序取第一个，可能在两个位置之间读到旧值
        let mut best: Option<(std::time::SystemTime, PathBuf, String)> = None;
        for p in candidates() {
            let Ok(s) = std::fs::read_to_string(&p) else {
                continue;
            };
            let mt = std::fs::metadata(&p)
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            if best.as_ref().map(|(t, _, _)| mt > *t).unwrap_or(true) {
                best = Some((mt, p, s));
            }
        }
        let mut cfg = Self::default();
        match best {
            Some((_, p, s)) => {
                cfg.apply(&s);
                logln!("[config] 已读取 {}", p.display());
            }
            None => logln!("[config] 无配置文件，使用默认值"),
        }
        cfg
    }

    fn apply(&mut self, text: &str) {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let (k, v) = (k.trim(), v.trim());
            match k {
                "last_room" => {
                    let digits: String = v.chars().filter(|c| c.is_ascii_digit()).collect();
                    if !digits.is_empty() {
                        self.last_room = Some(digits);
                    }
                }
                "prevent_sleep" => self.prevent_sleep = parse_bool(v, self.prevent_sleep),
                "danmaku_on" => self.danmaku_on = parse_bool(v, self.danmaku_on),
                "quality" => {
                    if let Ok(q) = v.parse::<i32>() {
                        // 只认界面上存在的档位，避免手改出个不存在的档位导致取流失败
                        if [0, 2, 3, 4].contains(&q) {
                            self.quality = q;
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn serialize(&self) -> String {
        format!(
            "# douyu 配置文件（删掉即可恢复默认）\n\
             last_room={}\n\
             prevent_sleep={}\n\
             danmaku_on={}\n\
             quality={}\n",
            self.last_room.clone().unwrap_or_default(),
            self.prevent_sleep as u8,
            self.danmaku_on as u8,
            self.quality
        )
    }

    /// 保存到第一个可写位置。失败只记日志，不影响播放。
    pub fn save(&self) {
        let data = self.serialize();
        for p in candidates() {
            if let Some(dir) = p.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            match std::fs::write(&p, &data) {
                Ok(()) => {
                    logln!("[config] 已写入 {}", p.display());
                    return;
                }
                Err(e) => logln!("[config] 写入 {} 失败: {e}", p.display()),
            }
        }
        logln!("[config] 配置未能落盘（无可写目录）");
    }
}

fn parse_bool(v: &str, default: bool) -> bool {
    match v {
        "1" | "true" | "yes" | "on" => true,
        "0" | "false" | "no" | "off" => false,
        _ => default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut c = Config::default();
        c.apply("last_room=123456\nprevent_sleep=0\ndanmaku_on=0\nquality=4\n");
        assert_eq!(c.last_room.as_deref(), Some("123456"));
        assert!(!c.prevent_sleep);
        assert!(!c.danmaku_on);
        assert_eq!(c.quality, 4);

        // 序列化回去再读一遍，字段不能漂
        let mut c2 = Config::default();
        c2.apply(&c.serialize());
        assert_eq!(c2.last_room, c.last_room);
        assert_eq!(c2.prevent_sleep, c.prevent_sleep);
        assert_eq!(c2.danmaku_on, c.danmaku_on);
        assert_eq!(c2.quality, c.quality);
    }

    #[test]
    fn tolerates_garbage() {
        let mut c = Config::default();
        // 手改坏的配置不能让程序崩，也不能塞进不存在的画质档位
        c.apply("# 注释\n???\nlast_room=abc\nquality=99\nprevent_sleep=maybe\n");
        assert_eq!(c.last_room, None);
        assert_eq!(c.quality, Config::default().quality);
        assert_eq!(c.prevent_sleep, Config::default().prevent_sleep);
    }

    #[test]
    fn last_room_accepts_pasted_url() {
        let mut c = Config::default();
        c.apply("last_room=https://www.douyu.com/9999\n");
        assert_eq!(c.last_room.as_deref(), Some("9999"));
    }
}
