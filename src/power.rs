//! 阻止系统自动待机（息屏 / 挂起）。
//!
//! 做法：`systemd-inhibit --what=idle:sleep --mode=block ... sh -c '<看门狗>'`。
//! 锁由 systemd-logind 持有，**systemd-inhibit 进程活着锁就有效**，进程退出即释放。
//!
//! 为什么不用 D-Bus 直连（zbus/dbus crate）：
//! - 不想为一个小功能引入新的异步栈依赖；
//! - `gdbus call ... Inhibit` 这类一次性命令，命令退出连接就断，锁立刻失效，
//!   根本拦不住待机 —— 必须有个常驻连接。systemd-inhibit 本身就是那个常驻壳。
//!
//! 看门狗：子进程每 3 秒 `kill -0 <本进程 pid>` 一次，本进程被强杀后子进程自己退出，
//! 锁随之释放。否则程序崩了会导致「系统永不待机」——这种副作用比功能本身更讨厌。
//!
//! `--what=idle` 覆盖 KDE/GNOME 的屏保与空闲动作（PowerDevil 走 logind 判断），
//! `--what=sleep` 覆盖直接挂起。故意**不含** `handle-lid-switch`：合盖通常是
//! 用户明确要求睡眠，不该被播放器拦下来。

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::logln;

pub struct SleepInhibitor {
    child: Option<Child>,
    /// 上一次启动失败的原因（界面用）
    last_err: Option<String>,
    /// 上次健康检查时刻（界面每秒 60 帧，不能每帧都去 waitpid）
    last_check: Instant,
    /// 连续启动失败次数：环境根本用不了（没有 systemd-inhibit）时别每 2 秒刷一次日志
    fail_count: u8,
    /// 本次启动时刻：刚拉起就死 = 环境不支持，累计几次后不再重试
    spawn_at: Instant,
}

impl Default for SleepInhibitor {
    fn default() -> Self {
        Self::new()
    }
}

impl SleepInhibitor {
    pub fn new() -> Self {
        Self {
            child: None,
            last_err: None,
            last_check: Instant::now(),
            fail_count: 0,
            spawn_at: Instant::now(),
        }
    }

    /// 启动抑制。已在运行则什么都不做，返回是否处于生效状态。
    pub fn start(&mut self) -> bool {
        if self.alive() {
            return true;
        }
        // 连续失败 3 次就认定这台机器不支持，不再自动重试（用户手动开一次可重置）
        if self.fail_count >= 3 {
            return false;
        }
        // 守护子进程：每 3 秒确认本进程还活着，本进程被 kill -9 后自动退出，
        // 锁随 systemd-inhibit 退出释放。否则程序被强杀会导致「系统永不待机」，
        // 这种副作用比功能本身更让人难受。
        let me = std::process::id();
        let watch = format!("while kill -0 {me} 2>/dev/null; do sleep 3; done");
        match Command::new("systemd-inhibit")
            .args([
                "--what=idle:sleep",
                "--mode=block",
                "--who=douyu",
                "--why=正在播放斗鱼直播",
                "sh",
                "-c",
                &watch,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            // 独立进程组：退出时整组 kill，避免留下孤儿进程
            .process_group(0)
            .spawn()
        {
            Ok(mut ch) => {
                let pid = ch.id();
                // systemd-inhibit 连不上 bus 会立刻退出并打一行原因，转进日志便于排查
                if let Some(mut err) = ch.stderr.take() {
                    std::thread::spawn(move || {
                        let mut s = String::new();
                        let _ = err.read_to_string(&mut s);
                        let s = s.trim();
                        if !s.is_empty() {
                            logln!("[power] systemd-inhibit: {s}");
                        }
                    });
                }
                self.child = Some(ch);
                self.spawn_at = Instant::now();
                self.last_err = None;
                self.fail_count = 0;
                logln!("[power] 已阻止系统待机（pid={pid}, what=idle:sleep）");
                true
            }
            Err(e) => {
                self.fail_count = self.fail_count.saturating_add(1);
                let msg = format!("无法阻止系统待机: {e}");
                logln!("[power] {msg}（第 {} 次失败）", self.fail_count);
                self.last_err = Some(msg);
                false
            }
        }
    }

    /// 释放抑制（关闭开关 / 程序退出）
    pub fn stop(&mut self) {
        // 用户主动关再开 = 重新给机会
        self.fail_count = 0;
        let Some(mut ch) = self.child.take() else {
            return;
        };
        let pid = ch.id();
        // 整组 SIGTERM：systemd-inhibit 与它拉起的 sleep 一起走，锁随进程退出释放
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(format!("-{pid}"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = ch.kill();
        let _ = ch.wait();
        logln!("[power] 已释放待机抑制（pid={pid}）");
    }

    /// 子进程还活着吗（被系统或用户杀掉后要能自动重来）
    pub fn alive(&mut self) -> bool {
        let Some(ch) = self.child.as_mut() else {
            return false;
        };
        match ch.try_wait() {
            Ok(None) => true,
            Ok(Some(st)) => {
                // 刚拉起就退出 = 环境不支持（没有 logind/bus），计入失败次数，
                // 攒够 3 次 ensure() 就不再反复拉起了
                if self.spawn_at.elapsed() < Duration::from_secs(3) {
                    self.fail_count = self.fail_count.saturating_add(1);
                }
                logln!(
                    "[power] systemd-inhibit 已退出（{st}，存活 {:.1}s），待机抑制失效",
                    self.spawn_at.elapsed().as_secs_f32()
                );
                if self.last_err.is_none() {
                    self.last_err = Some("待机抑制进程已退出".to_string());
                }
                self.child = None;
                false
            }
            Err(e) => {
                logln!("[power] 检查待机抑制状态失败: {e}");
                false
            }
        }
    }

    /// 每次界面刷新调用：开关是开的但进程没了 → 重新拉起（睡醒/被杀后自愈）。
    /// 内部限流，2 秒最多检查一次。
    pub fn ensure(&mut self, wanted: bool) {
        if !wanted || self.last_check.elapsed() < Duration::from_secs(2) {
            return;
        }
        self.last_check = Instant::now();
        if !self.alive() {
            self.start();
        }
    }

    pub fn last_err(&self) -> Option<&str> {
        self.last_err.as_deref()
    }
}

impl Drop for SleepInhibitor {
    fn drop(&mut self) {
        self.stop();
    }
}
