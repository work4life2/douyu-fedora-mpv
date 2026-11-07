//! 斗鱼直播客户端 —— 原生 GUI、低开销、视频内嵌单窗口。
//!
//! 双击启动 → GUI 输入房间号 → 点确定/回车 → 视频内嵌在窗口中央播放。
//! 侧边栏画质按钮无缝切换，弹幕可开关，播放时默认阻止系统待机。
//! 退出后记住上次的直播间/画质/弹幕开关，下次直接接着看。
//!
//! 视频：libmpv 渲染（dlopen libmpv.so.2，优先 GL 零拷贝）→ egui 纹理显示。
//! 硬解：auto-copy-safe（vaapi/nvdec 自动探测，失败回退软解）。弹幕：egui 画在视频层上（车道排程 + 密度自适应）。
//! 续流：本地 FLV 中转解决 300s 断流。

mod alloc_stat;
mod config;
mod danmaku;
mod douyu;
mod log;
mod login;
mod mpvffi;
mod power;
mod relay;
mod sign;

/// 计数分配器：只为把「Rust 活字节」和「RSS」分开看（见 alloc_stat）
#[global_allocator]
static ALLOC: alloc_stat::Counting = alloc_stat::Counting;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui;
use eframe::glow::{self, HasContext};

const QUALITY_NAMES: &[(i32, &str)] = &[(0, "原画"), (4, "蓝光4M"), (3, "超清"), (2, "高清")];

/// 弹幕队列上限（生产者侧）。热房瞬间涌入上千条时，宁可丢最旧的，
/// 也不能让内存和单帧工作量无上限增长。
const QUEUE_CAP: usize = 400;
/// 每帧最多上新条数：把「一口气上 500 条」摊到多帧，避免某帧直接卡住
const INTAKE_PER_FRAME: usize = 16;
/// 屏幕上同时在飞的弹幕条数硬上限
const MAX_ITEMS: usize = 200;
/// 内存看门狗阈值：有画面后 RSS 比基线涨这么多就重建渲染上下文。
/// 这套 libmpv 渲染外部 FBO 时每帧漏约 5KB（约 1GB/小时），
/// 取 300MB 意味着正常观看大约 20~30 分钟回收一次，用户无感。
const MEM_GROWTH_MB: u64 = 300;

/// 密度档位表：(字号系数, 横穿整屏秒数)。
/// 弹幕越密 → 字号越小（车道变多）+ 移动越快（车道腾空更快），两者相乘才够吞吐。
/// 到最后一档还挤不下就丢（保可读性，不叠成一团）。
const DENSITY: [(f32, f32); 4] = [(1.0, 8.0), (0.88, 7.2), (0.80, 6.4), (0.72, 5.8)];

type Chat = (String, String);

/// OpenGL 渲染目标：FBO + 绑定的纹理 + 注册给 egui 的 TextureId
struct GlVideoTarget {
    texture: glow::Texture,
    fbo: glow::Framebuffer,
    tex_id: egui::TextureId,
    w: usize,
    h: usize,
}

/// 后台登录线程回传的消息
enum LoginMsg {
    Qr(Result<(String, String), String>),
    Poll(Result<login::PollState, String>),
}

/// 扫码登录的界面状态（网络请求全部在后台线程，避免卡 UI）
struct LoginUi {
    qr_tex: Option<egui::TextureHandle>,
    qr_px: usize,
    status: String,
    /// 出错态：状态文字标红，按钮提示「刷新二维码」
    failed: bool,
    /// 成功后停在弹窗里等用户点「完成」（以前是瞬间关闭，用户看不到任何反馈）
    done: bool,
}

/// 顶部漂浮提示（登录成功/失败要让人一眼看到，不能只写侧边栏小字）
struct Notice {
    text: String,
    until: Instant,
    ok: bool,
}

impl Notice {
    fn new(text: impl Into<String>, ok: bool) -> Self {
        Self {
            text: text.into(),
            until: Instant::now() + Duration::from_secs(if ok { 6 } else { 10 }),
            ok,
        }
    }
}

/// 一条待显示的弹幕（位置由绘制时按出生时间计算）
struct Danmaku {
    text: String,
    /// 车道号（0 在最上）。上车时按「该车道尾部是否已完全进场」判定，
    /// 保证同车道内不会追尾重叠。
    lane: usize,
    spawn: Instant,
    /// 缓存的文字排版结果（避免每帧重复 layout，这是 60fps 掉 30fps 的主因）
    galley: Option<std::sync::Arc<egui::Galley>>,
    /// 排版时用的字号，字号变了要重排（窗口缩放/密度档位切换）
    galley_size: f32,
    /// 排版后的实际像素宽度（出屏判定用它，比「字符数 × 字号」准得多：
    /// 中文/emoji 是 1 em，ASCII 只有 0.5 em，估算法会差出一倍）
    width: f32,
}

struct DouyuApp {
    room_input: String,
    status: String,
    fullscreen: bool,

    // 流状态（relay 共享）与本地中转地址
    stream_state: relay::StreamState,
    relay_url: String,

    // libmpv 播放器
    player: Option<Arc<mpvffi::MpvPlayer>>,
    lib_err: Option<String>,

    // OpenGL 渲染（零拷贝）；None 表示回退软件渲染
    gl: Option<Arc<glow::Context>>,
    gl_video: Option<GlVideoTarget>,
    /// resize 防抖：拖动窗口时目标尺寸逐帧变化，等尺寸稳定后再重建纹理/FBO，
    /// 否则每帧 delete+create 纹理外加一次全量初始化上传，拖动直接卡成幻灯片
    pending_size: Option<(usize, usize)>,
    size_stable_at: Option<Instant>,

    // 视频纹理（GL 模式用 gl_video.tex_id，SW 模式用 video_tex）
    video_tex: Option<egui::TextureHandle>,
    frame_buf: Vec<u8>,
    frame_w: usize,
    frame_h: usize,
    frame_ready: bool,
    /// mpv 有新帧时置位（回调线程写），渲染前消费。渲染节奏由
    /// request_repaint_after(16ms) 驱动，这个标志只用于「没新帧就不重绘纹理」。
    need_frame: Arc<AtomicBool>,
    render_err_logged: bool,
    render_ok_logged: bool,

    // 帧率统计
    frame_count: u32,
    fps_timer: Instant,
    last_fps: f32,
    source_fps: f32,

    // 登录
    login: Option<LoginUi>,
    login_rx: Option<mpsc::Receiver<LoginMsg>>,
    login_stop: Option<Arc<AtomicBool>>,
    notice: Option<Notice>,
    /// 手动粘贴 Cookie 弹窗的内容（None=关闭）
    cookie_input: Option<String>,
    logged_in_as: Option<String>,

    // 弹幕
    danmaku_on: Arc<AtomicBool>,
    danmaku_stop: Arc<AtomicBool>,
    danmaku_queue: Arc<Mutex<VecDeque<Chat>>>,
    /// 队列撑满被丢掉的条数（生产者线程写，界面侧取走加到丢帧计数里）
    danmaku_cap_drops: Arc<std::sync::atomic::AtomicU32>,
    danmaku_list: Vec<Danmaku>,
    /// 每条车道「下一次可以再上车」的时刻。用单调时间而不是逐条扫描列表：
    /// 弹幕匀速左移，一条宽度 w 的弹幕占住车道 (w+间距)/速度 秒，之后车道必然空出。
    lane_free_at: Vec<Instant>,
    /// 密度档位 0/1/2：热房自动缩小字号、加快横穿速度，换取更多车道与吞吐
    dense: u8,
    /// 当前 1 秒窗口内因车道占满而丢弃的条数
    drops_window: u32,
    /// 连续多少个 1 秒窗口一条没丢（用于把密度档位降回去）
    idle_windows: u8,
    drops_total: u64,
    drops_timer: Instant,
    /// 上一帧视频区的逻辑尺寸：弹幕字号/车道数按它算，避免入队时还不知道尺寸
    vw_last: f32,
    vh_last: f32,

    // 本机配置（记住上次直播间 / 待机策略 / 画质）
    cfg: config::Config,
    /// 播放时阻止系统自动待机
    sleep: power::SleepInhibitor,
    /// 首帧自动恢复上次的直播间
    autostart: bool,
    /// 上一秒内存快照 (Rust 活字节, RSS_KB)，用于计算增速
    mem_snap: (u64, u64),
    /// 有画面后的 RSS 基线（KB），0 = 还没建立（内存看门狗用）
    rss_base: u64,
    /// 上次重建渲染上下文的时刻
    rc_recycled_at: Instant,
}

impl DouyuApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        setup_fonts(&cc.egui_ctx);

        // 中转（App 生命周期内只启动一次）
        let stream_state: relay::StreamState = Arc::new(Mutex::new((String::new(), 0)));
        let relay_url = match relay::Relay::new(stream_state.clone()).start() {
            Ok(p) => format!("http://127.0.0.1:{p}/live.flv"),
            Err(e) => {
                logln!("启动中转失败: {e}");
                String::new()
            }
        };

        // libmpv 播放器（启动即创建，换房只 loadfile）。优先 OpenGL 渲染，失败回退软件渲染。
        let need_frame = Arc::new(AtomicBool::new(false));
        let ctx2 = cc.egui_ctx.clone();
        let nf = need_frame.clone();
        let first_cb = Arc::new(AtomicBool::new(false));
        let make_cb = || -> Box<dyn Fn() + Send> {
            let nf = nf.clone();
            let ctx2 = ctx2.clone();
            let fc = first_cb.clone();
            Box::new(move || {
                if !fc.swap(true, Ordering::Relaxed) {
                    logln!("[video] mpv 帧回调已激活");
                }
                nf.store(true, Ordering::Relaxed);
                ctx2.request_repaint();
            })
        };

        let (player, gl) = if let Some(g) = cc.gl.clone() {
            match mpvffi::MpvPlayer::new_opengl(make_cb()) {
                Ok(p) => {
                    logln!("[video] 使用 OpenGL 零拷贝渲染");
                    (Some(Arc::new(p)), Some(g))
                }
                Err(e) => {
                    logln!("[video] OpenGL 渲染初始化失败，回退软件渲染: {e}");
                    match mpvffi::MpvPlayer::new(make_cb()) {
                        Ok(p) => (Some(Arc::new(p)), None),
                        Err(e2) => {
                            logln!("{e2}");
                            (None, None)
                        }
                    }
                }
            }
        } else {
            match mpvffi::MpvPlayer::new(make_cb()) {
                Ok(p) => (Some(Arc::new(p)), None),
                Err(e) => {
                    logln!("{e}");
                    (None, None)
                }
            }
        };

        let lib_err = if player.is_none() {
            Some("libmpv 加载失败，请安装: sudo dnf install mpv-libs".to_string())
        } else {
            None
        };

        // 本机配置：上次的直播间、待机策略、画质、弹幕开关
        let cfg = config::Config::load();
        let last = cfg.last_room.clone().unwrap_or_default();
        // 播放时阻止系统待机（默认开），起不来只记日志，不影响播放
        let mut sleep = power::SleepInhibitor::new();
        if cfg.prevent_sleep {
            sleep.start();
        }
        let status = if last.is_empty() {
            "请输入房间号，点「确定」或回车加载".to_string()
        } else {
            format!("将恢复上次的直播间 {last}（可直接改房间号）")
        };

        DouyuApp {
            room_input: last.clone(),
            status,
            fullscreen: false,
            stream_state,
            relay_url,
            player,
            lib_err,
            gl,
            gl_video: None,
            pending_size: None,
            size_stable_at: None,
            video_tex: None,
            frame_buf: Vec::new(),
            frame_w: 0,
            frame_h: 0,
            frame_ready: false,
            need_frame,
            render_err_logged: false,
            render_ok_logged: false,
            frame_count: 0,
            fps_timer: Instant::now(),
            last_fps: 0.0,
            source_fps: 0.0,
            login: None,
            login_rx: None,
            login_stop: None,
            notice: None,
            cookie_input: None,
            logged_in_as: login::load_cookie().map(|c| login::nickname(&c)),
            danmaku_on: Arc::new(AtomicBool::new(cfg.danmaku_on)),
            danmaku_stop: Arc::new(AtomicBool::new(false)),
            danmaku_queue: Arc::new(Mutex::new(VecDeque::new())),
            danmaku_cap_drops: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            danmaku_list: Vec::new(),
            lane_free_at: Vec::new(),
            dense: 0,
            drops_window: 0,
            idle_windows: 0,
            drops_total: 0,
            drops_timer: Instant::now(),
            vw_last: 1280.0,
            vh_last: 720.0,
            cfg,
            sleep,
            autostart: !last.is_empty(),
            mem_snap: (0, 0),
            rss_base: 0,
            rc_recycled_at: Instant::now(),
        }
    }

    /// 按显示区宽度封顶取流档位：解码分辨率远超显示区时，多出来的像素
    /// 全部在缩放里丢掉（904px 窗口看 1440p 原画 = 解码面积 8 倍于显示，
    /// 87.5% 的解码算力每秒白烧 60 次）。只降不升：用户主动选低档（省流量）就尊重。
    fn display_capped_quality(&self, want: i32) -> i32 {
        let auto_max = if self.vw_last < 1100.0 {
            3 // 超清
        } else if self.vw_last < 1700.0 {
            4 // 蓝光4M
        } else {
            0 // 原画
        };
        let rank = |r: i32| match r {
            0 => 3,  // 原画
            4 => 2,  // 蓝光4M
            3 => 1,  // 超清
            _ => 0,  // 高清
        };
        if rank(auto_max) < rank(want) {
            auto_max
        } else {
            want
        }
    }

    fn load_room(&mut self, ctx: &egui::Context) {
        let rid: String = self
            .room_input
            .trim()
            .chars()
            .filter(|c| c.is_ascii_digit())
            .collect();
        if rid.is_empty() {
            self.status = "房间号无效（输入纯数字或粘贴直播间链接）".to_string();
            return;
        }
        if self.player.is_none() || self.relay_url.is_empty() {
            self.status = self
                .lib_err
                .clone()
                .unwrap_or_else(|| "播放器未就绪".to_string());
            return;
        }

        // 停旧弹幕
        self.danmaku_stop.store(true, Ordering::Relaxed);
        self.danmaku_list.clear();
        self.danmaku_queue.lock().unwrap().clear();
        // 上一个房间遗留的丢弃计数不带进新房，否则密度会误升档
        self.danmaku_cap_drops.store(0, Ordering::Relaxed);
        self.drops_window = 0;
        self.idle_windows = 0;
        self.reset_lanes();

        // 换流（relay 检测状态变化无缝切流）+ 播放。
        // 档位按显示区封顶：小区窗口解码 2K 原画纯属浪费（见 display_capped_quality）
        let rate = self.display_capped_quality(self.cfg.quality);
        if rate != self.cfg.quality {
            logln!(
                "[room] 显示区宽 {:.0}px，原画无收益，自动取流降档 rate={}（点侧边栏画质按钮可手动改回）",
                self.vw_last, rate
            );
        }
        *self.stream_state.lock().unwrap() = (rid.clone(), rate);
        let url = self.relay_url.clone();
        // 直接调用（mpv_command 本身很快，不要 spawn+join 阻塞 UI 线程）
        if let Err(e) = self.player.as_ref().unwrap().loadfile(&url) {
            self.status = format!("加载失败: {e}");
            return;
        }

        // 记住这个房间（下次启动直接恢复），落盘失败不影响播放
        if self.cfg.last_room.as_deref() != Some(rid.as_str()) {
            self.cfg.last_room = Some(rid.clone());
            self.cfg.save();
            logln!("[room] 已记住直播间 {rid}");
        }

        // 新弹幕线程
        let stop = Arc::new(AtomicBool::new(false));
        self.danmaku_stop = stop.clone();
        let (tx, rx) = mpsc::channel();
        let rid2 = rid.clone();
        std::thread::spawn(move || danmaku::run(rid2, stop, tx));
        let queue = self.danmaku_queue.clone();
        let cap_drops = self.danmaku_cap_drops.clone();
        let ctx2 = ctx.clone();
        // 桥接线程：攒够一批再推队列 + 只请求一次重绘。
        // 旧写法每条弹幕都 request_repaint()，热房一秒几百次唤醒 UI 线程。
        std::thread::spawn(move || {
            while let Ok(first) = rx.recv() {
                let mut n = 1;
                {
                    let mut q = queue.lock().unwrap();
                    if push_capped(&mut q, first) {
                        cap_drops.fetch_add(1, Ordering::Relaxed);
                    }
                    // 一次最多搬 64 条，剩下的留到下一轮，避免长时间持锁
                    while n < 64 {
                        match rx.try_recv() {
                            Ok(c) => {
                                if push_capped(&mut q, c) {
                                    cap_drops.fetch_add(1, Ordering::Relaxed);
                                }
                                n += 1;
                            }
                            Err(_) => break,
                        }
                    }
                }
                ctx2.request_repaint();
            }
        });

        self.status = format!("正在播放房间 {rid}");

        // 换房重开一条播放链路：内存基线和回收计时都要重置，
        // 否则会拿着上个房间的高基线立刻触发一次无谓的重建
        self.rss_base = 0;
        self.rc_recycled_at = Instant::now();

        // 调试：合成弹幕洪水，肉眼验证密集场景（真热房在游客态常被限流，测不出密度）
        if let Ok(v) = std::env::var("DOUYU_STRESS") {
            if let Ok(rate) = v.trim().parse::<u32>() {
                if rate > 0 {
                    logln!("[stress] 合成弹幕 {rate} 条/秒（关掉 DOUYU_STRESS 即停）");
                    spawn_stress_feeder(
                        rate,
                        self.danmaku_stop.clone(),
                        self.danmaku_queue.clone(),
                        self.danmaku_cap_drops.clone(),
                        ctx.clone(),
                    );
                }
            }
        }
    }

    /// 打开扫码登录：起后台线程生成二维码 + 轮询（绝不阻塞 UI 线程）
    fn open_login(&mut self, ctx: &egui::Context) {
        // 停掉上一个会话
        if let Some(st) = self.login_stop.take() {
            st.store(true, Ordering::Relaxed);
        }
        let stop = Arc::new(AtomicBool::new(false));
        self.login_stop = Some(stop.clone());
        let (tx, rx) = mpsc::channel();
        self.login_rx = Some(rx);
        self.login = Some(LoginUi {
            qr_tex: None,
            qr_px: 0,
            status: "正在获取二维码…".to_string(),
            failed: false,
            done: false,
        });

        std::thread::spawn(move || {
            // 1) 生成二维码
            let sess = match login::generate() {
                Ok(s) => s,
                Err(e) => {
                    let _ = tx.send(LoginMsg::Qr(Err(e)));
                    return;
                }
            };
            if tx
                .send(LoginMsg::Qr(Ok((sess.code.clone(), sess.url.clone()))))
                .is_err()
            {
                return;
            }
            // 2) 轮询循环（2 秒一次，分片 sleep 便于及时停止）
            let mut err_streak = 0;
            loop {
                for _ in 0..20 {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                match login::poll(&sess.code) {
                    Ok(st) => {
                        err_streak = 0;
                        // 终态：成功 / 换票失败 / 过期 / 取消 —— 都别再轮询
                        let done = matches!(
                            st,
                            login::PollState::Success(_)
                                | login::PollState::Failed(_)
                                | login::PollState::Expired
                                | login::PollState::Rejected
                        );
                        let desc = match &st {
                            login::PollState::Waiting => "等待扫码",
                            login::PollState::Scanned => "已扫待确认",
                            login::PollState::Success(_) => "成功",
                            login::PollState::Failed(e) => {
                                logln!("[login] 换票失败: {e}");
                                "换票失败"
                            }
                            login::PollState::Expired => "已过期",
                            login::PollState::Rejected => "已取消",
                        };
                        logln!("[login] 轮询: {desc}");
                        if tx.send(LoginMsg::Poll(Ok(st))).is_err() {
                            return;
                        }
                        if done {
                            return;
                        }
                    }
                    Err(e) => {
                        // 网络抖动重试；连续 3 次失败才报到界面（避免静默死循环）
                        err_streak += 1;
                        logln!("[login] 轮询失败({err_streak}/3): {e}");
                        if err_streak >= 3 {
                            let _ = tx.send(LoginMsg::Poll(Err(e)));
                            return;
                        }
                    }
                }
            }
        });
        // 让 egui 持续重绘以处理回传消息
        ctx.request_repaint();
    }

    /// 处理后台登录线程的消息（非阻塞）
    fn poll_login(&mut self, ctx: &egui::Context) {
        let msgs: Vec<LoginMsg> = match &self.login_rx {
            Some(rx) => {
                let mut v = Vec::new();
                while let Ok(m) = rx.try_recv() {
                    v.push(m);
                }
                v
            }
            None => return,
        };
        for m in msgs {
            match m {
                LoginMsg::Qr(Ok((_code, url))) => {
                    let (tex, px) = make_qr_texture(ctx, &url);
                    if let Some(l) = &mut self.login {
                        l.qr_tex = tex;
                        l.qr_px = px;
                        l.status = "等待扫码…".to_string();
                        l.failed = false;
                    }
                    if self.login.as_ref().map(|l| l.qr_tex.is_none()).unwrap_or(false) {
                        logln!("[login] 二维码纹理未生成");
                        self.notice = Some(Notice::new("二维码渲染失败，请点「刷新」", false));
                    }
                }
                LoginMsg::Qr(Err(e)) => {
                    logln!("[login] 二维码获取失败: {e}");
                    if let Some(l) = &mut self.login {
                        l.status = format!("获取二维码失败: {e}");
                        l.failed = true;
                    }
                    self.notice = Some(Notice::new(format!("获取二维码失败: {e}"), false));
                }
                LoginMsg::Poll(Ok(login::PollState::Success(cookie))) => {
                    let nick = login::nickname(&cookie);
                    self.logged_in_as = Some(nick.clone());
                    self.status = format!("已登录「{nick}」，重新加载房间即可看原画");
                    self.notice = Some(Notice::new(format!("✓ 扫码登录成功：{nick}"), true));
                    if let Some(l) = &mut self.login {
                        l.status = format!("已登录：{nick}");
                        l.failed = false;
                        l.done = true;
                    }
                }
                LoginMsg::Poll(Ok(st)) => {
                    let (txt, bad) = match st {
                        login::PollState::Waiting => ("等待扫码…".to_string(), false),
                        login::PollState::Scanned => ("已扫码，请在手机上点确认".to_string(), false),
                        login::PollState::Expired => ("二维码已过期，点「刷新」重来".to_string(), true),
                        login::PollState::Rejected => ("已取消登录，点「刷新」重试".to_string(), true),
                        login::PollState::Failed(e) => (e, true),
                        login::PollState::Success(_) => (String::new(), false),
                    };
                    if bad {
                        self.status = format!("登录失败: {txt}");
                        self.notice = Some(Notice::new(
                            format!("登录未完成：{}", brief(&txt, 90)),
                            false,
                        ));
                    }
                    if let Some(l) = &mut self.login {
                        l.status = txt;
                        l.failed = bad;
                    }
                }
                LoginMsg::Poll(Err(e)) => {
                    logln!("[login] 轮询连续失败: {e}");
                    if let Some(l) = &mut self.login {
                        l.status = format!("网络错误: {e}");
                        l.failed = true;
                    }
                    self.notice = Some(Notice::new(
                        format!("登录轮询失败：{}", brief(&e, 90)),
                        false,
                    ));
                }
            }
        }
        // 登录弹窗打开期间保持刷新（让后台消息及时处理）；成功后是静态页，不必再刷
        if let Some(l) = &self.login {
            if !l.done {
                ctx.request_repaint_after(Duration::from_millis(200));
            }
        }
    }

    /// 顶部居中漂浮提示：登录成功/失败的关键反馈
    fn draw_notice(&mut self, ctx: &egui::Context) {
        let Some(n) = &self.notice else {
            return;
        };
        if Instant::now() >= n.until {
            self.notice = None;
            return;
        }
        let (text, ok) = (n.text.clone(), n.ok);
        let (bg, fg) = if ok {
            (
                egui::Color32::from_rgb(0x1e, 0x6f, 0x43),
                egui::Color32::from_rgb(0xea, 0xfb, 0xef),
            )
        } else {
            (
                egui::Color32::from_rgb(0x8c, 0x24, 0x24),
                egui::Color32::from_rgb(0xfd, 0xec, 0xec),
            )
        };
        egui::Area::new(egui::Id::new("douyu_notice"))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 14.0))
            .interactable(false)
            .show(ctx, |ui| {
                egui::Frame::none()
                    .fill(bg)
                    .rounding(8.0)
                    .inner_margin(egui::Margin::symmetric(16.0, 10.0))
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new(text).color(fg).size(16.0).strong());
                    });
            });
        ctx.request_repaint_after(Duration::from_millis(200));
    }

    /// 当前密度档位参数 (字号系数, 横穿秒数)
    fn density(&self) -> (f32, f32) {
        DENSITY[(self.dense as usize).min(DENSITY.len() - 1)]
    }

    /// 当前字号：随视频高度缩放；密集时自动缩小（换更多车道 + 更快腾空车道）
    fn danmaku_font_size(&self) -> f32 {
        let base = (self.vh_last / 26.0).clamp(14.0, 34.0);
        (base * self.density().0).max(12.0)
    }

    /// 横穿整屏用几秒（密集时略快，提升每秒可容纳的条数）
    fn danmaku_secs(&self) -> f32 {
        self.density().1
    }

    /// (车道数, 车道高)
    fn lane_metrics(&self) -> (usize, f32) {
        let fs = self.danmaku_font_size();
        let lane_h = (fs * 1.55).max(1.0);
        let lanes = ((self.vh_last / lane_h).floor() as usize).clamp(1, 24);
        (lanes, lane_h)
    }

    fn reset_lanes(&mut self) {
        self.lane_free_at.clear();
    }

    /// 车道数量变化（窗口缩放 / 密度档位切换）时同步「各车道空闲时刻」表
    fn sync_lanes(&mut self, lanes: usize) {
        if self.lane_free_at.len() == lanes {
            return;
        }
        let past = Instant::now()
            .checked_sub(Duration::from_secs(30))
            .unwrap_or_else(Instant::now);
        self.lane_free_at.resize(lanes, past);
        // 缩小窗口后越界的弹幕直接丢掉（跟 retain 里 `d.lane < lanes` 对应）
        let n = self.lane_free_at.len();
        self.danmaku_list.retain(|d| d.lane < n);
    }

    /// 从队列取弹幕上车。每帧有上限，车道上限由「空闲时刻」表判定。
    fn drain_danmaku(&mut self, ctx: &egui::Context) {
        if !self.danmaku_on.load(Ordering::Relaxed) {
            self.danmaku_queue.lock().unwrap().clear();
            // 关闭期间生产者还在往满队列里丢（计数只增不减），必须持续消费掉：
            // 否则重新打开的一瞬间 drops_window 爆表，密度档位直接跳到最高
            let _ = self.danmaku_cap_drops.swap(0, Ordering::Relaxed);
            self.drops_window = 0;
            if !self.danmaku_list.is_empty() {
                self.danmaku_list.clear();
                self.reset_lanes();
            }
            return;
        }
        let now = Instant::now();
        // 队列撑满丢掉的也算「丢弃」：否则队列溢出时密度自适应完全失灵
        // （那些条根本没走到车道判定这一步）
        let capped = self.danmaku_cap_drops.swap(0, Ordering::Relaxed);
        self.drops_window += capped;
        self.drops_total += capped as u64;
        // 一次持锁取一小批：热房一帧来上千条也只处理前 16 条，剩下的下一帧继续
        let mut batch: Vec<Chat> = Vec::with_capacity(INTAKE_PER_FRAME);
        {
            let mut q = self.danmaku_queue.lock().unwrap();
            for _ in 0..INTAKE_PER_FRAME {
                match q.pop_front() {
                    Some(c) => batch.push(c),
                    None => break,
                }
            }
        }
        for (_nn, txt) in batch {
            self.push_danmaku(ctx, &txt, now);
        }
        // 在屏条数截断放在整批之后一次做：逐条 push 就头部删一条是 O(n) memmove，
        // 密集时每帧 16 次 × 200 条元素的搬移；drain 一批只搬一次。
        // 单帧内短暂超过上限十来条，下一帧就截回去，肉眼无感。
        if self.danmaku_list.len() > MAX_ITEMS {
            let excess = self.danmaku_list.len() - MAX_ITEMS;
            self.danmaku_list.drain(..excess);
        }
        self.tune_density(now);
    }

    /// 单条弹幕上车：挑一条当前空闲的车道；全满就丢弃（记数，用于自动升档）
    fn push_danmaku(&mut self, ctx: &egui::Context, raw: &str, now: Instant) {
        let text = sanitize_danmaku(raw);
        if text.is_empty() {
            return;
        }
        let (lanes, _) = self.lane_metrics();
        self.sync_lanes(lanes);

        // 从上往下找第一条空闲车道：既不重叠，视觉上也先填满上部
        let lane = self.lane_free_at.iter().position(|t| *t <= now);
        let Some(lane) = lane else {
            self.drops_window += 1;
            self.drops_total += 1;
            return;
        };

        // 排版：每条只做一次，顺手拿到真实宽度（出屏判定与车道占用时长都用它）
        let fs = self.danmaku_font_size();
        let galley = ctx.fonts(|f| {
            f.layout_no_wrap(text.clone(), egui::FontId::proportional(fs), egui::Color32::WHITE)
        });
        let w = galley.size().x.max(1.0);
        let speed = (self.vw_last.max(320.0) / self.danmaku_secs()).max(1.0);
        let gap = (fs * 1.2).max(12.0);
        // 这条弹幕的尾部完全进场（留出 gap）之前，同车道不能再发车，
        // 否则后车会在屏幕上追尾。两车同速，一旦尾部进场就永不重叠。
        self.lane_free_at[lane] = now + Duration::from_secs_f32(lane_hold_secs(w, gap, speed));

        self.danmaku_list.push(Danmaku {
            text,
            lane,
            spawn: now,
            galley: Some(galley),
            galley_size: fs,
            width: w,
        });
    }

    /// 按丢帧情况自适应密度档位：热房自动缩字提速，人少了再涨回来
    fn tune_density(&mut self, now: Instant) {
        if now.duration_since(self.drops_timer) < Duration::from_secs(1) {
            return;
        }
        self.drops_timer = now;
        if self.drops_window >= 8 && self.dense < DENSITY.len() as u8 - 1 {
            self.dense += 1;
            self.idle_windows = 0;
            logln!(
                "[danmaku] 弹幕过密（1 秒丢 {} 条），密度档位→{}（字号缩至 {:.0}px，横穿 {:.1}s）",
                self.drops_window,
                self.dense,
                self.danmaku_font_size(),
                self.danmaku_secs()
            );
        } else if self.drops_window == 0 {
            self.idle_windows = self.idle_windows.saturating_add(1);
            // 连续 8 秒零丢弃才降档，避免在临界点来回抖
            if self.idle_windows >= 8 && self.dense > 0 {
                self.dense -= 1;
                self.idle_windows = 0;
                logln!(
                    "[danmaku] 弹幕转疏，密度档位→{}（字号 {:.0}px，横穿 {:.1}s）",
                    self.dense,
                    self.danmaku_font_size(),
                    self.danmaku_secs()
                );
            }
        } else {
            self.idle_windows = 0;
        }
        self.drops_window = 0;
    }

    /// 绘制弹幕并移除已出屏的。origin 为视频区域左上角（painter_at 不平移坐标，需手动加）。
    fn draw_danmaku(
        &mut self,
        painter: &egui::Painter,
        origin: egui::Pos2,
        vw: f32,
        vh: f32,
        now: Instant,
    ) {
        // 先把视频区尺寸记下来：正文用不到时也要更新，否则窗口缩放后
        // 第一条弹幕会按旧尺寸算车道
        self.vw_last = vw;
        self.vh_last = vh;
        if self.danmaku_list.is_empty() {
            return;
        }
        let font_size = self.danmaku_font_size();
        let (lanes, lane_h) = self.lane_metrics();
        let font = egui::FontId::proportional(font_size);
        let speed = (vw / self.danmaku_secs()).max(1.0);
        // 兜底寿命：即使出屏判定因字号突变失准，也不会永久堆积
        let max_life = self.danmaku_secs() * 1.6;

        // 一次性 retain 清屏（旧代码在循环里 remove(i) 是 O(n)，密集时自己拖垮自己）
        self.danmaku_list.retain(|d| {
            if d.lane >= lanes {
                return false;
            }
            let elapsed = now.duration_since(d.spawn).as_secs_f32();
            if elapsed > max_life {
                return false;
            }
            let x = vw - elapsed * speed;
            x + d.width >= 0.0
        });

        for d in &mut self.danmaku_list {
            if d.galley.is_none() || (d.galley_size - font_size).abs() > 0.5 {
                let g = painter.layout_no_wrap(
                    d.text.clone(),
                    font.clone(),
                    egui::Color32::WHITE,
                );
                d.width = g.size().x.max(1.0);
                d.galley = Some(g);
                d.galley_size = font_size;
            }
            let elapsed = now.duration_since(d.spawn).as_secs_f32();
            let x = vw - elapsed * speed;
            let pos = origin + egui::vec2(x, d.lane as f32 * lane_h + 4.0);
            let galley = d.galley.clone().unwrap();
            // 描边 2 次（对角偏移）而非 4 次，减半绘制开销
            painter.galley(pos + egui::vec2(1.0, 1.0), galley.clone(), egui::Color32::BLACK);
            painter.galley(pos, galley, egui::Color32::WHITE);
        }
    }

    /// 渲染一帧到 frame_buf（rgb0）。显示侧负责转成 egui 纹理。
    fn render_frame(&mut self, w: usize, h: usize) {
        let player = match &self.player {
            Some(p) => p.clone(),
            None => return,
        };
        let need = w * h * 4;
        if self.frame_buf.len() != need {
            self.frame_buf = vec![0u8; need];
        }
        if let Err(e) = player.render_sw(w, h, &mut self.frame_buf) {
            if !self.render_err_logged {
                self.render_err_logged = true;
                logln!("[video] 渲染失败（只报一次）: {e}");
            }
            return;
        }
        if !self.render_ok_logged {
            self.render_ok_logged = true;
            logln!("[video] 首帧渲染成功 {}x{}", w, h);
        }
        self.frame_w = w;
        self.frame_h = h;
        self.frame_ready = true;
    }

    /// 重建 OpenGL 渲染目标（纹理 + FBO），并注册为 egui 纹理
    fn recreate_gl_target(
        &mut self,
        gl: &glow::Context,
        w: usize,
        h: usize,
        frame: &mut eframe::Frame,
    ) {
        unsafe {
            if let Some(t) = self.gl_video.take() {
                gl.delete_framebuffer(t.fbo);
                gl.delete_texture(t.texture);
            }
            let texture = match gl.create_texture() {
                Ok(t) => t,
                Err(e) => {
                    logln!("[video] 创建纹理失败: {e}");
                    return;
                }
            };
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            // 必须用 sRGB 纹理：egui_glow 的 shader 在 SRGB_TEXTURES 模式下对所有纹理
            // 做 linear→sRGB 转换。普通 RGBA8 存的是 sRGB 值，会被再转一次 → 发白。
            // 用 SRGB8_ALPHA8 后：GPU 采样自动 sRGB→linear，shader 再转回 sRGB = 正确。
            let init = vec![0xFFu8; w * h * 4];
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::SRGB8_ALPHA8 as i32,
                w as i32,
                h as i32,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                Some(&init),
            );
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
            // 采样时强制 alpha=1（不透明），彻底忽略 mpv 可能写入的 alpha 通道
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_SWIZZLE_A,
                glow::ONE as i32,
            );
            gl.bind_texture(glow::TEXTURE_2D, None);

            let fbo = match gl.create_framebuffer() {
                Ok(f) => f,
                Err(e) => {
                    logln!("[video] 创建 FBO 失败: {e}");
                    gl.delete_texture(texture);
                    return;
                }
            };
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(texture),
                0,
            );
            let status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            if status != glow::FRAMEBUFFER_COMPLETE {
                logln!("[video] FBO 不完整: 0x{status:x}");
                gl.delete_framebuffer(fbo);
                gl.delete_texture(texture);
                return;
            }

            let tex_id = frame.register_native_glow_texture(texture);
            self.gl_video = Some(GlVideoTarget {
                texture,
                fbo,
                tex_id,
                w,
                h,
            });
        }
    }

    /// OpenGL 渲染一帧到 FBO 纹理（零拷贝，结果直接是 egui 纹理）
    fn render_video_gl(&mut self, frame: &mut eframe::Frame, w: usize, h: usize) {
        let player = match &self.player {
            Some(p) => p.clone(),
            None => return,
        };
        let gl = match &self.gl {
            Some(g) => g.clone(),
            None => return,
        };

        let mut need_recreate = match &self.gl_video {
            Some(t) => t.w != w || t.h != h,
            None => true,
        };
        if need_recreate {
            // 拖动窗口时目标尺寸逐帧都在变，逐帧重建纹理+FBO（含 w*h*4 的
            // 初始化上传）就是「拖窗口卡成幻灯片」的根因。等尺寸稳定 150ms
            // 再重建，这几帧沿用旧纹理（egui 按显示矩形拉伸，肉眼无感）。
            let now = Instant::now();
            let stable = match (self.pending_size, self.size_stable_at) {
                (Some(sz), Some(at)) => sz == (w, h) && now >= at,
                _ => false,
            };
            if stable || self.gl_video.is_none() {
                self.recreate_gl_target(&gl, w, h, frame);
                self.pending_size = None;
                self.size_stable_at = None;
            } else {
                if self.pending_size != Some((w, h)) {
                    self.pending_size = Some((w, h));
                    self.size_stable_at = Some(now + Duration::from_millis(150));
                }
                // 尺寸未稳定：这帧不重建，也不按新尺寸渲染
                need_recreate = false;
            }
        }

        // 只在 mpv 有新帧时渲染（无新帧复用上一帧纹理，避免 GPU 空转）
        let new_frame = self.need_frame.swap(false, Ordering::Relaxed);
        if !new_frame && !need_recreate {
            return;
        }

        // 渲染尺寸取纹理的实际尺寸：防抖窗口期纹理还是旧的，
        // 按新尺寸渲染进旧纹理会写出 FBO 边界
        let (tw, th, fbo) = match &self.gl_video {
            Some(t) => (t.w, t.h, t.fbo),
            None => return,
        };
        if let Err(e) = player.render_gl(fbo.0.get() as i32, tw, th) {
            if !self.render_err_logged {
                self.render_err_logged = true;
                logln!("[video] GL 渲染失败（只报一次）: {e}");
            }
            return;
        }
        if !self.render_ok_logged {
            self.render_ok_logged = true;
            logln!("[video] GL 首帧渲染成功 {}x{}", tw, th);
        }
        self.frame_w = tw;
        self.frame_h = th;
        self.frame_ready = true;
    }

    fn toggle_fullscreen(&mut self, ctx: &egui::Context) {
        self.fullscreen = !self.fullscreen;
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
    }

    /// 释放 GL 视频目标（FBO + 纹理）。重建渲染上下文前必须走这里：
    /// 直接 `gl_video = None` 会把纹理/FBO 丢在显存里没人删。
    fn drop_gl_target(&mut self) {
        let Some(t) = self.gl_video.take() else {
            return;
        };
        if let Some(gl) = &self.gl {
            unsafe {
                gl.delete_framebuffer(t.fbo);
                gl.delete_texture(t.texture);
            }
        }
    }
}

impl Drop for DouyuApp {
    fn drop(&mut self) {
        self.danmaku_stop.store(true, Ordering::Relaxed);
    }
}

impl eframe::App for DouyuApp {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.drain_danmaku(ctx);

        // 待机抑制自愈：开关开着但进程被系统杀了（挂起唤醒后很常见）→ 重新拉起
        if self.cfg.prevent_sleep {
            self.sleep.ensure(true);
        }

        // 顶部输入区（全屏时隐藏）
        if !self.fullscreen {
            egui::TopBottomPanel::top("input").show(ctx, |ui| {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label("房间号");
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.room_input)
                            .hint_text("输入房间号或链接")
                            .desired_width(170.0),
                    );
                    let enter =
                        resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let clicked = ui.button("确定").clicked();
                    if enter || clicked {
                        self.load_room(ctx);
                    }
                    if let Some(e) = &self.lib_err {
                        ui.colored_label(egui::Color32::RED, e);
                    }
                });
                ui.add_space(6.0);
            });

            // 侧边栏
            egui::SidePanel::left("side")
                .resizable(false)
                .default_width(120.0)
                .show(ctx, |ui| {
                    ui.add_space(8.0);
                    ui.label(egui::RichText::new("画质").strong());
                    let q = self.stream_state.lock().unwrap().1;
                    for (val, name) in QUALITY_NAMES {
                        if ui.selectable_label(q == *val, *name).clicked() {
                            logln!("[quality] 点击切换: {name} (rate={val})，当前 rate={q}");
                            self.stream_state.lock().unwrap().1 = *val;
                            self.status = format!("已切换到 {name}（无缝）");
                            // 记住画质，下次开播沿用
                            if self.cfg.quality != *val {
                                self.cfg.quality = *val;
                                self.cfg.save();
                            }
                        }
                    }
                    ui.separator();
                    let mut on = self.danmaku_on.load(Ordering::Relaxed);
                    if ui.checkbox(&mut on, "显示弹幕").changed() {
                        self.danmaku_on.store(on, Ordering::Relaxed);
                        self.cfg.danmaku_on = on;
                        self.cfg.save();
                        if !on {
                            self.danmaku_list.clear();
                            self.reset_lanes();
                        }
                    }
                    ui.separator();
                    // 阻止系统自动待机（息屏/休眠）：默认开，可在侧边栏随时关掉
                    let mut keep_awake = self.cfg.prevent_sleep;
                    if ui.checkbox(&mut keep_awake, "禁止自动待机").changed() {
                        self.cfg.prevent_sleep = keep_awake;
                        if keep_awake {
                            let ok = self.sleep.start();
                            self.status = if ok {
                                "已阻止系统待机（关闭窗口即恢复）".to_string()
                            } else {
                                self.sleep
                                    .last_err()
                                    .unwrap_or("无法阻止系统待机")
                                    .to_string()
                            };
                        } else {
                            self.sleep.stop();
                            self.status = "已允许系统自动待机".to_string();
                        }
                        self.cfg.save();
                    }
                    // 抑制失败时在侧边栏直接说明原因（比如没有 systemd-inhibit）
                    let inh_err = self.sleep.last_err().map(|s| s.to_string());
                    if let Some(e) = inh_err {
                        if self.cfg.prevent_sleep && !self.sleep.alive() {
                            ui.label(
                                egui::RichText::new(brief(&e, 40))
                                    .small()
                                    .color(egui::Color32::from_rgb(0xd0, 0x3a, 0x3a)),
                            );
                        }
                    }
                    ui.separator();
                    if ui.button("全屏 (f)").clicked() {
                        self.toggle_fullscreen(ctx);
                    }
                    ui.separator();
                    match self.logged_in_as.clone() {
                        Some(n) => {
                            ui.label(egui::RichText::new(format!("👤 {n}")).small());
                        }
                        None => {
                            if ui.button("扫码登录").clicked() {
                                self.open_login(ctx);
                            }
                            if ui.button("手动粘贴 Cookie").clicked() {
                                self.cookie_input = Some(String::new());
                            }
                        }
                    }
                    ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                        ui.label(egui::RichText::new(&self.status).small());
                        if self.last_fps > 0.0 {
                            ui.label(
                                egui::RichText::new(format!(
                                    "渲染 {:.0}fps / 源 {}fps",
                                    self.last_fps,
                                    if self.source_fps > 0.5 {
                                        format!("{:.0}", self.source_fps)
                                    } else {
                                        "?".to_string()
                                    }
                                ))
                                .small()
                                .weak(),
                            );
                        }
                    });
                });
        }

        // 中央：视频（内嵌，16:9 fit 居中，铺满可用区域）
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(egui::Color32::BLACK))
            .show(ctx, |ui| {
                let avail = ui.available_size();
                // 视频按 16:9 fit 到可用区域（保持比例，尽可能大）
                let (vw, vh) = fit_169(avail);
                // 每帧记录视频区逻辑尺寸：换房时「按显示区选档」要用
                //（draw_danmaku 里也写同值，幂等）
                self.vw_last = vw;
                self.vh_last = vh;
                // 渲染纹理尺寸：同比例，上限 1080p（显示时等比放大无失真）
                let scale = (1920.0 / vw).min(1080.0 / vh).min(1.0);
                let rw = ((vw * scale) as usize).clamp(320, 1920);
                let rh = ((vh * scale) as usize).clamp(180, 1080);

                // 渲染（OpenGL 零拷贝优先，软件渲染回退）
                if self.gl.is_some() {
                    self.render_video_gl(frame, rw, rh);
                } else {
                    self.render_frame(rw, rh);
                    if self.frame_ready {
                        let img = egui::ColorImage {
                            size: [rw, rh],
                            pixels: self
                                .frame_buf
                                .as_chunks::<4>()
                                .0
                                .iter()
                                .map(|c| egui::Color32::from_rgb(c[0], c[1], c[2]))
                                .collect(),
                        };
                        match self.video_tex.as_mut() {
                            Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                            None => {
                                self.video_tex = Some(
                                    ctx.load_texture("video", img, egui::TextureOptions::LINEAR),
                                );
                            }
                        }
                    }
                }

                // 取纹理 id（GL 模式用 FBO 纹理，SW 模式用上传纹理）
                let tex_id = if let Some(t) = &self.gl_video {
                    Some(t.tex_id)
                } else {
                    self.video_tex.as_ref().map(|t| t.id())
                };
                if let Some(tid) = tex_id {
                    // 居中铺满 fit 区域
                    let rect = egui::Rect::from_center_size(
                        ui.max_rect().center(),
                        egui::vec2(vw, vh),
                    );
                    let resp = ui.put(
                        rect,
                        egui::Image::new((tid, egui::vec2(vw, vh))),
                    );
                    if resp.double_clicked() {
                        self.toggle_fullscreen(ctx);
                    }
                    // 弹幕（画在视频区上层，clip 到 rect，坐标需加 rect.min 偏移）
                    let painter = ui.painter_at(rect);
                    self.draw_danmaku(&painter, rect.min, vw, vh, Instant::now());
                } else if let Some(e) = &self.lib_err {
                    ui.centered_and_justified(|ui| {
                        ui.colored_label(egui::Color32::RED, e);
                    });
                } else {
                    ui.centered_and_justified(|ui| {
                        ui.label("输入房间号开始播放");
                    });
                }
            });

        // 首帧恢复上次的直播间：放在中央面板之后，让「按显示区选档」
        // 用第一帧布局出来的真实视频区宽度，而不是默认值 1280
        if self.autostart {
            self.autostart = false;
            let rid = self.room_input.trim().to_string();
            if !rid.is_empty() {
                logln!("[room] 自动恢复上次直播间 {rid}");
                self.load_room(ctx);
            }
        }

        // 全局快捷键：f 全屏 / ESC 退出全屏
        let toggle = ctx.input(|i| {
            i.key_pressed(egui::Key::F)
                || (i.key_pressed(egui::Key::Escape) && self.fullscreen)
        });
        if toggle {
            self.toggle_fullscreen(ctx);
        }

        // 手动粘贴 Cookie 兜底
        let mut close_cookie = false;
        let mut save_cookie_now = false;
        if let Some(input) = &mut self.cookie_input {
            egui::Window::new("手动粘贴 Cookie")
                .collapsible(false)
                .resizable(true)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    ui.label("从已登录斗鱼网页的浏览器里复制 Cookie：F12 → Network → 任意 douyu.com 请求 → 复制 Cookie 请求头");
                    ui.add_space(4.0);
                    ui.add(
                        egui::TextEdit::multiline(input)
                            .desired_rows(6)
                            .desired_width(560.0)
                            .hint_text("acf_uid=...; acf_auth=...; ..."),
                    );
                    ui.horizontal(|ui| {
                        if ui.button("保存并登录").clicked() {
                            save_cookie_now = true;
                        }
                        if ui.button("取消").clicked() {
                            close_cookie = true;
                        }
                    });
                });
        }
        if save_cookie_now {
            if let Some(raw) = self.cookie_input.clone() {
                match login::normalize_cookie(&raw) {
                    Ok(c) => {
                        let _ = login::save_cookie(&c);
                        self.logged_in_as = login::load_cookie().map(|c| login::nickname(&c));
                        self.status = "Cookie 已保存，重新加载房间即可看原画".to_string();
                        self.notice = Some(Notice::new("Cookie 已保存", true));
                        self.cookie_input = None;
                    }
                    Err(e) => {
                        self.status = format!("Cookie 无效: {e}");
                        self.notice = Some(Notice::new(format!("Cookie 无效: {e}"), false));
                    }
                }
            }
        }
        if close_cookie {
            self.cookie_input = None;
        }

        // 扫码登录：轮询状态 + 二维码弹窗
        self.poll_login(ctx);
        let mut close_login = false;
        let mut refresh_login = false;
        if let Some(lg) = &self.login {
            egui::Window::new("扫码登录斗鱼")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    ui.vertical_centered(|ui| {
                        if lg.done {
                            // 成功态：明确停住，等用户点「完成」（不再瞬间消失）
                            ui.add_space(6.0);
                            ui.label(
                                egui::RichText::new("✓ 登录成功")
                                    .size(22.0)
                                    .color(egui::Color32::from_rgb(0x2e, 0x9e, 0x5b))
                                    .strong(),
                            );
                            ui.add_space(6.0);
                            ui.label(egui::RichText::new(&lg.status).size(15.0));
                            ui.add_space(4.0);
                            ui.label(
                                egui::RichText::new("原画已解锁，重新点「确定」加载房间即可生效")
                                    .small()
                                    .weak(),
                            );
                            ui.add_space(8.0);
                            if ui.button("完成").clicked() {
                                close_login = true;
                            }
                        } else {
                            ui.label("请用斗鱼 App 扫码登录");
                            ui.add_space(4.0);
                            if let Some(tex) = &lg.qr_tex {
                                ui.image((tex.id(), egui::vec2(lg.qr_px as f32, lg.qr_px as f32)));
                            } else {
                                ui.label("二维码生成失败");
                            }
                            ui.add_space(4.0);
                            if lg.failed {
                                ui.label(
                                    egui::RichText::new(&lg.status)
                                        .color(egui::Color32::from_rgb(0xd0, 0x3a, 0x3a)),
                                );
                                ui.label(
                                    egui::RichText::new("点「刷新二维码」重新开始")
                                        .small()
                                        .weak(),
                                );
                            } else {
                                ui.label(&lg.status);
                            }
                            ui.add_space(4.0);
                            ui.horizontal(|ui| {
                                if ui.button("刷新二维码").clicked() {
                                    refresh_login = true;
                                }
                                if ui.button("关闭").clicked() {
                                    close_login = true;
                                }
                            });
                        }
                    });
                });
        }
        if refresh_login {
            self.open_login(ctx);
        }
        if close_login {
            if let Some(st) = self.login_stop.take() {
                st.store(true, Ordering::Relaxed);
            }
            self.login = None;
            self.login_rx = None;
        }

        // 顶部提示横幅（成功/失败都要一眼能看见；以前只写侧边栏小字，等于没反馈）
        self.draw_notice(ctx);

        // 帧率统计（每秒一次），同时读取 mpv 源帧率和视频参数
        self.frame_count += 1;
        let fe = self.fps_timer.elapsed();
        if fe >= Duration::from_secs(1) {
            self.last_fps = self.frame_count as f32 / fe.as_secs_f32();
            let (mut est, mut cont, mut vw, mut vh) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
            let (mut drops, mut dec_drops, mut mistimed) = (0.0f64, 0.0f64, 0.0f64);
            let mut hwdec = String::new();
            if let Some(p) = &self.player {
                est = p.get_f64("estimated-frame-rate").unwrap_or(0.0);
                cont = p.get_f64("container-fps").unwrap_or(0.0);
                vw = p.get_f64("width").unwrap_or(0.0);
                vh = p.get_f64("height").unwrap_or(0.0);
                // 丢帧统计：区分「源本身 30fps」和「渲染跟不上丢帧」
                drops = p.get_f64("frame-drop-count").unwrap_or(0.0);
                dec_drops = p.get_f64("decoder-frame-drop-count").unwrap_or(0.0);
                mistimed = p.get_f64("mistimed-frame-count").unwrap_or(0.0);
                // 硬解是否真的生效（hwdec=auto 后这是第一个该盯的诊断量：
                // 探测失败会静默回退软解，之前写死 nvdec 时完全无感知）
                hwdec = p.get_string("hwdec-current").unwrap_or_default();
                self.source_fps = if est > 0.5 { est as f32 } else { cont as f32 };
            }
            // 弹幕运行态（一次性取齐，避免在 logln 参数里混合可变/不可变借用）
            let now_l = Instant::now();
            let on_screen = self.danmaku_list.len();
            let lanes_total = self.lane_free_at.len();
            let lanes_free = self.lane_free_at.iter().filter(|t| **t <= now_l).count();
            let awake = match self.sleep.alive() {
                true => "开",
                false => "失效",
            };
            let awake = if self.cfg.prevent_sleep { awake } else { "关" };
            logln!(
                "[fps] 渲染 {:.1} | 源 {} (est={:.1} cont={:.1}) | 视频 {:.0}x{:.0} | 丢帧 输出{:.0}/解码{:.0}/错时{:.0} | 尺寸 {}x{} | rate={} | 弹幕 在屏{} 车道 {}/{} 空闲 密度{} 累计丢弃{} | 待机抑制 {awake} | 硬解 {hwdec}",
                self.last_fps,
                if self.source_fps > 0.5 {
                    format!("{:.1}", self.source_fps)
                } else {
                    "未知".to_string()
                },
                est,
                cont,
                vw,
                vh,
                drops,
                dec_drops,
                mistimed,
                self.frame_w,
                self.frame_h,
                self.stream_state.lock().unwrap().1,
                on_screen,
                lanes_free,
                lanes_total,
                self.dense,
                self.drops_total
            );
            // 内存诊断：Rust 活字节 vs RSS。
            // 活字节涨 = 我们自己漏；活字节平而 RSS 涨 = mpv/GL 或 glibc 碎片。
            let (live_b, live_n) = alloc_stat::live();
            let rss = alloc_stat::rss_kb();
            let (pb, pr) = self.mem_snap;
            self.mem_snap = (live_b, rss);
            let (db, dr) = if pb == 0 {
                (0, 0)
            } else {
                (live_b as i64 - pb as i64, rss as i64 - pr as i64)
            };
            let buckets = alloc_stat::top_buckets(3)
                .iter()
                .map(|(k, v)| format!("{k}={:.1}MB", *v as f64 / 1048576.0))
                .collect::<Vec<_>>()
                .join(" ");
            logln!(
                "[mem] Rust 活 {:.1}MB/{}块 ({:+.0}KB/s) | RSS {:.0}MB ({:+.0}KB/s) | 最大档 {buckets}",
                live_b as f64 / 1048576.0,
                live_n,
                db / 1024,
                rss as f64 / 1024.0,
                dr / 1024
            );

            // 内存看门狗：这套 libmpv 在「渲染到外部 FBO」路径上每渲染一帧漏约 5KB
            // C 侧内存（Rust 活字节完全不涨，见上行的 [mem] 诊断）。桶在渲染上下文里，
            // 定期 free+create 就能收回来，且不影响播放进度。
            // 正常策略：有画面后记基线，涨过 MEM_GROWTH_MB 才重建（默认约 40 分钟一次）；
            // 调试策略：DOUYU_RC_RECYCLE=秒 强制按固定周期重建。
            if self.rss_base == 0 && self.frame_w > 0 {
                self.rss_base = rss;
                logln!("[mem] 内存基线 {}MB，超过 +{}MB 时重建渲染上下文", rss / 1024, MEM_GROWTH_MB);
            }
            let force = std::env::var("DOUYU_RC_RECYCLE")
                .ok()
                .and_then(|v| v.parse::<u64>().ok());
            let limit_mb = std::env::var("DOUYU_MEM_LIMIT_MB")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(MEM_GROWTH_MB);
            let due = match force {
                Some(secs) => self.rc_recycled_at.elapsed() >= Duration::from_secs(secs.max(5)),
                None => {
                    self.rss_base > 0
                        && rss > self.rss_base + limit_mb * 1024
                        && self.rc_recycled_at.elapsed() >= Duration::from_secs(120)
                }
            };
            if due {
                if let Some(p) = &self.player {
                    let before = rss;
                    match p.recycle_render_ctx() {
                        Ok(()) => {
                            self.rc_recycled_at = Instant::now();
                            // 重新连渲染目标：旧纹理要显式删掉，下一帧会重建并重新注册
                            self.drop_gl_target();
                            self.pending_size = None;
                            self.size_stable_at = None;
                            // 基线跟着下移：否则重建完立刻又满足「超阈值」
                            self.rss_base = alloc_stat::rss_kb();
                            logln!(
                                "[mem] 已重建渲染上下文：RSS {}MB → {}MB（阈值 +{}MB，约 {} 分钟一次）",
                                before / 1024,
                                self.rss_base / 1024,
                                limit_mb,
                                // 实测 ~310KB/s，换算成分钟（仅用于日志展示）
                                (limit_mb * 1024 / 310 / 60).max(1)
                            );
                        }
                        Err(e) => {
                            logln!("[mem] 重建渲染上下文失败: {e}（下个周期再试）");
                            self.rc_recycled_at = Instant::now();
                        }
                    }
                }
            }
            self.frame_count = 0;
            self.fps_timer = Instant::now();
        }

        // 播放器存在时持续自驱渲染（60fps，匹配高帧率直播源）
        if self.player.is_some() {
            ctx.request_repaint_after(Duration::from_millis(16));
        }
    }
}

/// 调试：合成弹幕洪水（`DOUYU_STRESS=每秒条数`）。
///
/// 用途：真实热门直播间对未登录游客会限流（实测 300 万人气的房间只有 1 条/秒），
/// 想验证「密集弹幕会不会堆死、会不会重叠」需要自己造压力。
/// 走的是和真实弹幕完全相同的路径（同一个队列、同一个上限、同一个丢弃计数）。
fn spawn_stress_feeder(
    rate: u32,
    stop: Arc<AtomicBool>,
    queue: Arc<Mutex<VecDeque<Chat>>>,
    cap_drops: Arc<std::sync::atomic::AtomicU32>,
    ctx: egui::Context,
) {
    std::thread::spawn(move || {
        const WORDS: [&str; 14] = [
            "666",
            "主播牛啊",
            "这波操作可以",
            "哈哈哈哈哈哈",
            "开饭了开饭了",
            "来啦来吧",
            "牛批牛批牛批",
            "这游戏真好玩",
            "前方高能预警",
            "🎉🎉🎉",
            "一给我里giaogiao",
            "这直播间真热闹",
            "一万个人在说话",
            "弹幕测试弹幕测试弹幕测试",
        ];
        let interval = Duration::from_millis(1000 / rate.max(1) as u64 / 2);
        let mut i: usize = 0;
        while !stop.load(Ordering::Relaxed) {
            {
                let mut q = queue.lock().unwrap();
                // 一次两条：模拟高峰时一帧内到达多条
                for _ in 0..2 {
                    let w = WORDS[i % WORDS.len()];
                    if push_capped(&mut q, (format!("观众{}", i % 997), format!("{w} #{i}"))) {
                        cap_drops.fetch_add(1, Ordering::Relaxed);
                    }
                    i = i.wrapping_add(1);
                }
            }
            ctx.request_repaint();
            std::thread::sleep(interval);
        }
        logln!("[stress] 合成弹幕已停止（共注入 {i} 条）");
    });
}

/// 截断长文本用于横幅提示（完整内容留在弹窗/日志里）
fn brief(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

/// 弹幕文本清洗。三件事都是「密集时才会爆」的坑：
/// 1. 控制字符（尤其 `\n`）必须换掉：`layout_no_wrap` 遇到换行会排成多行，
///    一条弹幕就压到下面几条车道上，画面立刻糊。
/// 2. 合并连续空白，避免「刷屏空格」把一条弹幕撑满整条车道。
/// 3. 超长截断：一条 200 字的弹幕会独占一条车道十几秒，热房里等于永久堵死。
fn sanitize_danmaku(s: &str) -> String {
    const MAX_CHARS: usize = 60;
    let mut out = String::with_capacity(s.len().min(MAX_CHARS * 4));
    let mut n = 0usize;
    let mut last_space = false;
    for c in s.chars() {
        let c = if c.is_control() || c == '\u{200b}' { ' ' } else { c };
        if c == ' ' {
            if last_space {
                continue;
            }
            last_space = true;
        } else {
            last_space = false;
        }
        if n >= MAX_CHARS {
            out.push('…');
            break;
        }
        out.push(c);
        n += 1;
    }
    out.trim().to_string()
}

/// 一条弹幕会占用它的车道多久（秒）。
///
/// 弹幕匀速左移，宽度 w 的弹幕从右边缘进场，到尾部完全进场并留出 gap 所需的
/// 时间是 (w + gap) / speed。这段时间内同车道不能再发车 —— 否则后车会追尾。
/// 两车速度相同，只要发车瞬间不重叠，之后任何时刻都不会重叠。
fn lane_hold_secs(width: f32, gap: f32, speed: f32) -> f32 {
    (width.max(1.0) + gap.max(0.0)) / speed.max(1.0)
}

/// 弹幕队列入队（带上限）。满了丢最旧的：观众看的是"现在"，旧的迟到弹幕没价值，
/// 而无上限的队列在热房会一路涨到吃光内存。返回是否发生了丢弃。
fn push_capped(q: &mut VecDeque<Chat>, chat: Chat) -> bool {
    let dropped = q.len() >= QUEUE_CAP;
    if dropped {
        q.pop_front();
    }
    q.push_back(chat);
    dropped
}

/// 把二维码内容渲染成 egui 纹理。
/// 关键：QR 规范要求四周留 4 模块静区（quiet zone），缺失会导致扫码器识别失败。
fn make_qr_texture(ctx: &egui::Context, url: &str) -> (Option<egui::TextureHandle>, usize) {
    let (w, px) = match login::qr_matrix(url) {
        Ok(v) => v,
        Err(e) => {
            logln!("[login] 二维码矩阵生成失败: {e}");
            return (None, 0);
        }
    };
    const QUIET: usize = 4; // 静区模块数
    const SCALE: usize = 8; // 每模块像素
    let modules = w + QUIET * 2;
    let size = modules * SCALE;
    let mut pixels = vec![egui::Color32::WHITE; size * size];
    let mut dark = 0usize;
    for y in 0..w {
        for x in 0..w {
            if px[y * w + x] {
                dark += 1;
                let ox = (x + QUIET) * SCALE;
                let oy = (y + QUIET) * SCALE;
                for dy in 0..SCALE {
                    for dx in 0..SCALE {
                        pixels[(oy + dy) * size + ox + dx] = egui::Color32::BLACK;
                    }
                }
            }
        }
    }
    logln!(
        "[login] 二维码已渲染：{}x{} 模块（含静区 {}），{}px，黑块 {}，内容 {} 字符",
        modules,
        modules,
        QUIET,
        size,
        dark,
        url.chars().count()
    );
    // 同时导出 PNG：屏幕扫码失败时可用手机相册识别
    let rgba: Vec<u8> = pixels
        .iter()
        .flat_map(|p| [p.r(), p.g(), p.b(), 255])
        .collect();
    match login::save_png(&rgba, size, "douyu_qr.png") {
        Ok(p) => logln!("[login] 二维码图片已保存: {}", p.display()),
        Err(e) => logln!("[login] 二维码图片保存失败: {e}"),
    }

    let img = egui::ColorImage {
        size: [size, size],
        pixels,
    };
    let tex = ctx.load_texture("login_qr", img, egui::TextureOptions::NEAREST);
    (Some(tex), size)
}

/// 把 16:9 视频适配进 avail（保持比例，取最大可能尺寸）
fn fit_169(avail: egui::Vec2) -> (f32, f32) {
    let aw = avail.x.max(1.0);
    let ah = avail.y.max(1.0);
    if aw * 9.0 > ah * 16.0 {
        (ah * 16.0 / 9.0, ah)
    } else {
        (aw, aw * 9.0 / 16.0)
    }
}

fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let candidates = [
        "/usr/share/fonts/google-noto-sans-cjk-vf-fonts/NotoSansCJK-VF.ttc",
        "/usr/share/fonts/google-noto-sans-cjk-fonts/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/wqy-microhei/wqy-microhei.ttc",
    ];
    for path in candidates {
        if let Ok(data) = std::fs::read(path) {
            fonts
                .font_data
                .insert("cjk".into(), egui::FontData::from_owned(data));
            if let Some(fam) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
                fam.insert(0, "cjk".into());
            }
            if let Some(fam) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
                fam.push("cjk".into());
            }
            break;
        }
    }
    ctx.set_fonts(fonts);
}

/// 编译期指纹：确认跑的确实是新构建（改代码后这个值会变）
const BUILD_TAG: &str = concat!(env!("CARGO_PKG_VERSION"), "+", env!("DOUYU_BUILD"));

fn main() {
    log::init();
    logln!("=== douyu v{} 启动（构建 {BUILD_TAG}）===", env!("CARGO_PKG_VERSION"));

    // 调试命令（不影响双击启动）
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 2 {
        match args[1].as_str() {
            "danmaku-test" => return cmd_danmaku_test(&args[2..]),
            "relay-test" => return cmd_relay_test(&args[2..]),
            "login-test" => return cmd_login_test(),
            "login-exchange" => return cmd_login_exchange(&args[2..]),
            // 不是独立命令：设好环境变量后继续走正常 GUI 启动，
            // 用来肉眼验证密集弹幕（默认 60 条/秒）
            "danmaku-stress" => {
                let rate = args.get(2).cloned().unwrap_or_else(|| "60".to_string());
                std::env::set_var("DOUYU_STRESS", &rate);
                logln!("[stress] 进入 GUI 后向弹幕注入 {rate} 条/秒合成弹幕");
            }
            _ => {}
        }
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1024.0, 640.0])
            .with_min_inner_size([640.0, 400.0])
            .with_title("斗鱼直播"),
        ..Default::default()
    };
    // 启动失败（无显示/GPU 初始化失败）必须留下日志：
    // 否则「双击没反应」时既没有窗口也没有任何线索
    if let Err(e) = eframe::run_native(
        "斗鱼直播",
        options,
        Box::new(|cc| Ok(Box::new(DouyuApp::new(cc)))),
    ) {
        logln!("启动 GUI 失败: {e}");
    }
}

/// 调试：连接弹幕并打印收到的弹幕
fn cmd_danmaku_test(rest: &[String]) {
    let rid = rest
        .first()
        .cloned()
        .unwrap_or_else(|| "1126960".to_string());
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let rid2 = rid.clone();
    std::thread::spawn(move || danmaku::run(rid2, stop, tx));
    println!("已连接房间 {rid} 的弹幕，等待消息...");
    let mut n = 0;
    while n < 10 {
        match rx.recv_timeout(Duration::from_secs(40)) {
            Ok((nn, txt)) => {
                println!("[弹幕] {nn}: {txt}");
                n += 1;
            }
            Err(e) => {
                logln!("40 秒内未收到弹幕（{e}）");
                break;
            }
        }
    }
}

/// 调试：起续流中转，读 FLV 验证
fn cmd_relay_test(rest: &[String]) {
    use std::io::Read;
    let rid = rest
        .first()
        .cloned()
        .unwrap_or_else(|| "1126960".to_string());
    // 第二个参数可选：画质档位（默认 4）
    let rate: i32 = rest.get(1).and_then(|s| s.parse().ok()).unwrap_or(4);
    logln!("[test] relay-test rid={rid} rate={rate}");
    let state: relay::StreamState = Arc::new(Mutex::new((rid, rate)));
    let port = relay::Relay::new(state).start().expect("启动 relay 失败");
    let url = format!("http://127.0.0.1:{port}/live.flv");
    println!("relay 已启动: {url}");

    let resp = douyu::agent().get(&url).call().expect("请求 relay 失败");
    let mut reader: Box<dyn Read> = resp.into_reader();
    let mut buf = Vec::new();
    let start = Instant::now();
    let mut tmp = [0u8; 4096];
    while buf.len() < 4096 && start.elapsed() < Duration::from_secs(5) {
        match reader.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(_) => break,
        }
    }
    let is_flv = buf.len() >= 3 && &buf[..3] == b"FLV";
    println!(
        "读得 {} 字节，FLV 头校验: {}",
        buf.len(),
        if is_flv { "OK ✅" } else { "失败 ❌" }
    );
    if is_flv {
        let mut tags = 0;
        let mut i = 13;
        while i + 11 + 4 <= buf.len() {
            let sz =
                ((buf[i + 1] as usize) << 16) | ((buf[i + 2] as usize) << 8) | buf[i + 3] as usize;
            if sz == 0 || i + 11 + sz + 4 > buf.len() {
                break;
            }
            tags += 1;
            i += 11 + sz + 4;
        }
        println!("解析到 {tags} 个 FLV tag");
    }
}

/// 调试：只跑「扫码确认后换登录态」这一段（回调重定向 + Set-Cookie 收集）。
/// 用来确定 HTTPS/TLS 通路是否正常——这是之前整条链路哑掉的地方。
fn cmd_login_exchange(rest: &[String]) {
    let url = match rest.first() {
        Some(u) => u.clone(),
        None => {
            println!("用法: douyu login-exchange '<回调 URL>'");
            println!("  无 code 时可用普通页面探路，例如 https://www.douyu.com/");
            return;
        }
    };
    println!("[test] 目标: {}", url.split('?').next().unwrap_or(&url));
    match login::exchange_cookie(&url) {
        Ok(c) => println!(
            "[test] 换票成功，{} 个字段: {}",
            c.split(';').count(),
            c
        ),
        Err(e) => println!("[test] 换票失败: {e}"),
    }
}

/// 调试：验证二维码登录 API 是否可用
fn cmd_login_test() {
    match login::generate() {
        Ok(s) => {
            // 截断走字符边界（head）：直接按字节切片在非 ASCII 上会 panic
            let code_short = log::head(&s.code, 20);
            let url_short = log::head(&s.url, 90);
            logln!("[test] 二维码生成成功 code={code_short} url={url_short}");
            // 验证：生成二维码 PNG（供解码器校验）
            if let Ok((w, px)) = login::qr_matrix(&s.url) {
                const QUIET: usize = 4;
                const SCALE: usize = 8;
                let modules = w + QUIET * 2;
                let size = modules * SCALE;
                let mut rgba = vec![255u8; size * size * 4];
                for y in 0..w {
                    for x in 0..w {
                        if px[y * w + x] {
                            let ox = (x + QUIET) * SCALE;
                            let oy = (y + QUIET) * SCALE;
                            for dy in 0..SCALE {
                                for dx in 0..SCALE {
                                    let i = ((oy + dy) * size + ox + dx) * 4;
                                    rgba[i] = 0;
                                    rgba[i + 1] = 0;
                                    rgba[i + 2] = 0;
                                    rgba[i + 3] = 255;
                                }
                            }
                        }
                    }
                }
                match login::save_png(&rgba, size, "douyu_qr.png") {
                    Ok(p) => println!("[test] 二维码 PNG: {} ({size}px, {modules} 模块)", p.display()),
                    Err(e) => println!("[test] PNG 保存失败: {e}"),
                }
                println!("[test] 二维码内容: {}", s.url);
            }
            match login::poll(&s.code) {
                Ok(st) => logln!("[test] 首次轮询状态: {st:?}"),
                Err(e) => logln!("[test] 轮询失败: {e}"),
            }
        }
        Err(e) => logln!("[test] 二维码生成失败: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newline_would_break_lanes() {
        // 换行必须被压成空格：layout_no_wrap 会把 \n 排成多行，一条弹幕压好几条车道
        assert_eq!(sanitize_danmaku("上\n下"), "上 下");
        assert_eq!(sanitize_danmaku("a\r\nb"), "a b");
        assert_eq!(sanitize_danmaku("tab\there"), "tab here");
    }

    #[test]
    fn collapse_repeated_spaces() {
        assert_eq!(sanitize_danmaku("刷   屏    空格"), "刷 屏 空格");
        assert_eq!(sanitize_danmaku("   前后留白   "), "前后留白");
        assert_eq!(sanitize_danmaku(""), "");
        assert_eq!(sanitize_danmaku("   "), "");
    }

    #[test]
    fn long_text_is_truncated() {
        let long = "字".repeat(200);
        let s = sanitize_danmaku(&long);
        // 60 字 + 省略号，不会独占车道十几秒
        assert_eq!(s.chars().count(), 61);
        assert!(s.ends_with('…'));
    }

    #[test]
    fn keeps_normal_text() {
        assert_eq!(sanitize_danmaku("主播牛啊 666 🎉"), "主播牛啊 666 🎉");
        assert_eq!(sanitize_danmaku("@S 是转义符"), "@S 是转义符");
    }

    #[test]
    fn queue_is_capped() {
        let mut q: VecDeque<Chat> = VecDeque::new();
        let mut dropped = 0;
        for i in 0..(QUEUE_CAP + 50) {
            if push_capped(&mut q, (String::new(), i.to_string())) {
                dropped += 1;
            }
        }
        assert_eq!(q.len(), QUEUE_CAP);
        // 丢弃数要如实反映，密度自适应靠它判断热不热
        assert_eq!(dropped, 50);
        // 丢的是最旧的，队尾一定是最新那条
        assert_eq!(q.back().unwrap().1, (QUEUE_CAP + 49).to_string());
    }

    /// 车道排程的核心性质：同车道相邻两条，全程不会重叠。
    /// 按 push_danmaku 的真实逻辑跑：车道空闲时刻 = 发车时刻 + lane_hold_secs(本条宽度)。
    #[test]
    fn lane_never_overlaps_even_when_dense() {
        let vw = 1280.0f32;
        let speed = vw / 8.0;
        let gap = 19.0f32; // 16px 字号 × 1.2
        let widths = [60.0f32, 180.0, 320.0, 90.0];
        // free_at 初值取一个足够久远的过去：车道开局就是空的
        let mut free_at = -1.0e6f32;
        let mut prev: Option<(f32, f32)> = None; // (发车时刻, 宽度)
        for &w in widths.iter().cycle().take(60) {
            let spawn = free_at; // 车道一空就发车（最密集的极端情况）
            if let Some((t0, w0)) = prev {
                // 发车瞬间：前一条的右边缘必须已经离开右边界一个 gap
                let right_of_prev = vw - (spawn - t0) * speed + w0;
                assert!(
                    right_of_prev <= vw - gap + 0.01,
                    "spawn={spawn:.2} 追尾：前一条右边缘 {right_of_prev:.1} > 上限 {:.1}",
                    vw - gap
                );
                // 之后任意时刻间距恒定（两车同速），抽查几个时刻
                for dt in [0.0f32, 0.5, 2.0, 5.0, 7.9] {
                    let l_new = vw - dt * speed;
                    let r_prev = vw - ((spawn + dt) - t0) * speed + w0;
                    assert!(l_new >= r_prev - 0.01, "spawn+{dt}s 处发生重叠");
                }
            }
            prev = Some((spawn, w));
            free_at = spawn + lane_hold_secs(w, gap, speed);
        }
    }

    /// 吞吐自检：1280x720 视频区，按真实字号/车道公式算每秒能上多少条。
    #[test]
    fn lane_throughput_grows_with_density() {
        let (vw, vh) = (1280.0f32, 720.0f32);
        let mut last = 0.0f32;
        for (level, (font_factor, secs)) in DENSITY.iter().enumerate() {
            let fs = (vh / 26.0).clamp(14.0, 34.0) * font_factor;
            let lane_h = fs * 1.55;
            let lanes = ((vh / lane_h).floor() as usize).clamp(1, 24) as f32;
            let speed = vw / secs;
            let gap = (fs * 1.2).max(12.0);
            // 按约 8 个汉字（≈8em）的平均宽度估算
            let per_sec = lanes / lane_hold_secs(fs * 8.0, gap, speed);
            assert!(
                per_sec >= last,
                "档位 {level} 吞吐 {per_sec:.1} 反而低于上一档 {last:.1}"
            );
            last = per_sec;
        }
        // 最高档要能扛住热房（约 20 条/秒以上才不至于大面积丢）
        assert!(last > 20.0, "最高密度档吞吐仅 {last:.1} 条/秒");
    }
}
