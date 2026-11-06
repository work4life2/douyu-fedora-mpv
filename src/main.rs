//! 斗鱼直播客户端 —— 原生 GUI、低开销、视频内嵌单窗口。
//!
//! 双击启动 → GUI 输入房间号 → 点确定/回车 → 视频内嵌在窗口中央播放。
//! 侧边栏画质按钮无缝切换，弹幕可开关。
//!
//! 视频：libmpv 软件渲染（dlopen libmpv.so.2）→ egui 纹理显示（单窗口内嵌）。
//! 硬解：hwdec=nvdec-copy。弹幕：mpv osd-overlay(libass) 直接渲染进视频帧。
//! 续流：本地 FLV 中转解决 300s 断流。

mod danmaku;
mod douyu;
mod mpvffi;
mod relay;
mod sign;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui;
use eframe::glow::{self, HasContext};

const QUALITY_NAMES: &[(i32, &str)] = &[(0, "原画"), (4, "蓝光4M"), (3, "超清"), (2, "高清")];

type Chat = (String, String);

/// OpenGL 渲染目标：FBO + 绑定的纹理 + 注册给 egui 的 TextureId
struct GlVideoTarget {
    texture: glow::Texture,
    fbo: glow::Framebuffer,
    tex_id: egui::TextureId,
    w: usize,
    h: usize,
}

/// 一条待显示的弹幕（位置由绘制时按出生时间计算）
struct Danmaku {
    text: String,
    row: usize,
    spawn: Instant,
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

    // 视频纹理（GL 模式用 gl_video.tex_id，SW 模式用 video_tex）
    video_tex: Option<egui::TextureHandle>,
    frame_buf: Vec<u8>,
    frame_w: usize,
    frame_h: usize,
    frame_ready: bool,
    #[allow(dead_code)] // mpv callback 仍写此标志，渲染由持续循环驱动
    need_frame: Arc<AtomicBool>,
    render_err_logged: bool,
    render_ok_logged: bool,

    // 弹幕
    danmaku_on: Arc<AtomicBool>,
    danmaku_stop: Arc<AtomicBool>,
    danmaku_queue: Arc<Mutex<VecDeque<Chat>>>,
    danmaku_list: Vec<Danmaku>,
    next_danmaku_row: u64,
}

impl DouyuApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        setup_fonts(&cc.egui_ctx);

        // 中转（App 生命周期内只启动一次）
        let stream_state: relay::StreamState = Arc::new(Mutex::new((String::new(), 0)));
        let relay_url = match relay::Relay::new(stream_state.clone()).start() {
            Ok(p) => format!("http://127.0.0.1:{p}/live.flv"),
            Err(e) => {
                eprintln!("启动中转失败: {e}");
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
                    eprintln!("[video] mpv 帧回调已激活");
                }
                nf.store(true, Ordering::Relaxed);
                ctx2.request_repaint();
            })
        };

        let (player, gl) = if let Some(g) = cc.gl.clone() {
            match mpvffi::MpvPlayer::new_opengl(make_cb()) {
                Ok(p) => {
                    eprintln!("[video] 使用 OpenGL 零拷贝渲染");
                    (Some(Arc::new(p)), Some(g))
                }
                Err(e) => {
                    eprintln!("[video] OpenGL 渲染初始化失败，回退软件渲染: {e}");
                    match mpvffi::MpvPlayer::new(make_cb()) {
                        Ok(p) => (Some(Arc::new(p)), None),
                        Err(e2) => {
                            eprintln!("{e2}");
                            (None, None)
                        }
                    }
                }
            }
        } else {
            match mpvffi::MpvPlayer::new(make_cb()) {
                Ok(p) => (Some(Arc::new(p)), None),
                Err(e) => {
                    eprintln!("{e}");
                    (None, None)
                }
            }
        };

        let lib_err = if player.is_none() {
            Some("libmpv 加载失败，请安装: sudo dnf install mpv-libs".to_string())
        } else {
            None
        };

        DouyuApp {
            room_input: String::new(),
            status: "请输入房间号，点「确定」或回车加载".to_string(),
            fullscreen: false,
            stream_state,
            relay_url,
            player,
            lib_err,
            gl,
            gl_video: None,
            video_tex: None,
            frame_buf: Vec::new(),
            frame_w: 0,
            frame_h: 0,
            frame_ready: false,
            need_frame,
            render_err_logged: false,
            render_ok_logged: false,
            danmaku_on: Arc::new(AtomicBool::new(true)),
            danmaku_stop: Arc::new(AtomicBool::new(false)),
            danmaku_queue: Arc::new(Mutex::new(VecDeque::new())),
            danmaku_list: Vec::new(),
            next_danmaku_row: 0,
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

        // 换流（relay 检测状态变化无缝切流）+ 播放
        *self.stream_state.lock().unwrap() = (rid.clone(), 0);
        let url = self.relay_url.clone();
        let player = self.player.as_ref().unwrap().clone();
        let r = std::thread::spawn(move || player.loadfile(&url))
            .join()
            .unwrap_or_else(|_| Err("loadfile 线程失败".into()));
        if let Err(e) = r {
            self.status = format!("加载失败: {e}");
            return;
        }

        // 新弹幕线程
        let stop = Arc::new(AtomicBool::new(false));
        self.danmaku_stop = stop.clone();
        let (tx, rx) = mpsc::channel();
        let rid2 = rid.clone();
        std::thread::spawn(move || danmaku::run(rid2, stop, tx));
        let queue = self.danmaku_queue.clone();
        let ctx2 = ctx.clone();
        std::thread::spawn(move || {
            for chat in rx {
                queue.lock().unwrap().push_back(chat);
                ctx2.request_repaint();
            }
        });

        self.status = format!("正在播放房间 {rid}");
    }

    fn drain_danmaku(&mut self) {
        if !self.danmaku_on.load(Ordering::Relaxed) {
            self.danmaku_queue.lock().unwrap().clear();
            return;
        }
        let mut q = self.danmaku_queue.lock().unwrap();
        while let Some((_nn, txt)) = q.pop_front() {
            if txt.trim().is_empty() {
                continue;
            }
            // 限制数量，防止弹幕海量堆积
            if self.danmaku_list.len() > 300 {
                self.danmaku_list.remove(0);
            }
            let row = (self.next_danmaku_row % 13) as usize;
            self.next_danmaku_row += 1;
            self.danmaku_list.push(Danmaku {
                text: txt,
                row,
                spawn: Instant::now(),
            });
        }
    }

    /// 绘制弹幕并移除已出屏的。返回后再统一 retain。
    fn draw_danmaku(
        &mut self,
        painter: &egui::Painter,
        vw: f32,
        vh: f32,
        now: Instant,
    ) {
        if self.danmaku_list.is_empty() {
            return;
        }
        let font_size = (vh / 26.0).clamp(14.0, 34.0);
        let font = egui::FontId::proportional(font_size);
        let row_h = vh / 13.0;
        // 8 秒横穿整屏
        let speed = vw / 8.0;

        let mut i = 0;
        while i < self.danmaku_list.len() {
            let d = &self.danmaku_list[i];
            let elapsed = now.duration_since(d.spawn).as_secs_f32();
            // 文本左上角 x：从右边缘进入，向左移动
            let x = vw - elapsed * speed;
            // 估算文本宽度（用于判断出屏）
            let text_w = d.text.chars().count() as f32 * font_size * 0.92;
            if x + text_w < 0.0 {
                self.danmaku_list.remove(i);
                continue;
            }
            let pos = egui::pos2(x, d.row as f32 * row_h + 4.0);
            let galley = painter.layout_no_wrap(d.text.clone(), font.clone(), egui::Color32::WHITE);
            // 黑描边（四方向）
            for (dx, dy) in [(-1.0, 0.0), (1.0, 0.0), (0.0, -1.0), (0.0, 1.0)] {
                painter.galley(
                    pos + egui::vec2(dx, dy),
                    galley.clone(),
                    egui::Color32::BLACK,
                );
            }
            painter.galley(pos, galley, egui::Color32::WHITE);
            i += 1;
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
                eprintln!("[video] 渲染失败（只报一次）: {e}");
            }
            return;
        }
        if !self.render_ok_logged {
            self.render_ok_logged = true;
            eprintln!("[video] 首帧渲染成功 {}x{}", w, h);
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
                    eprintln!("[video] 创建纹理失败: {e}");
                    return;
                }
            };
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            // 用不透明数据初始化（alpha=255），避免未初始化显存里的脏 alpha
            let init = vec![0xFFu8; w * h * 4];
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA8 as i32,
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
                    eprintln!("[video] 创建 FBO 失败: {e}");
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
                eprintln!("[video] FBO 不完整: 0x{status:x}");
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

        let need_recreate = match &self.gl_video {
            Some(t) => t.w != w || t.h != h,
            None => true,
        };
        if need_recreate {
            self.recreate_gl_target(&gl, w, h, frame);
        }

        let fbo = match &self.gl_video {
            Some(t) => t.fbo,
            None => return,
        };
        if let Err(e) = player.render_gl(fbo.0.get() as i32, w, h) {
            if !self.render_err_logged {
                self.render_err_logged = true;
                eprintln!("[video] GL 渲染失败（只报一次）: {e}");
            }
            return;
        }
        if !self.render_ok_logged {
            self.render_ok_logged = true;
            eprintln!("[video] GL 首帧渲染成功 {}x{}", w, h);
        }
        self.frame_w = w;
        self.frame_h = h;
        self.frame_ready = true;
    }

    fn toggle_fullscreen(&mut self, ctx: &egui::Context) {
        self.fullscreen = !self.fullscreen;
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
    }
}

impl Drop for DouyuApp {
    fn drop(&mut self) {
        self.danmaku_stop.store(true, Ordering::Relaxed);
    }
}

impl eframe::App for DouyuApp {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.drain_danmaku();

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
                            self.stream_state.lock().unwrap().1 = *val;
                            self.status = format!("已切换到 {name}（无缝）");
                        }
                    }
                    ui.separator();
                    let mut on = self.danmaku_on.load(Ordering::Relaxed);
                    if ui.checkbox(&mut on, "显示弹幕").changed() {
                        self.danmaku_on.store(on, Ordering::Relaxed);
                    }
                    ui.separator();
                    if ui.button("全屏 (f)").clicked() {
                        self.toggle_fullscreen(ctx);
                    }
                    ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                        ui.label(egui::RichText::new(&self.status).small());
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
                                .chunks_exact(4)
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
                    // 弹幕（画在视频区上层，clip 到 rect，坐标相对 rect 左上）
                    let painter = ui.painter_at(rect);
                    self.draw_danmaku(&painter, vw, vh, Instant::now());
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

        // 全局快捷键：f 全屏 / ESC 退出全屏
        let toggle = ctx.input(|i| {
            i.key_pressed(egui::Key::F)
                || (i.key_pressed(egui::Key::Escape) && self.fullscreen)
        });
        if toggle {
            self.toggle_fullscreen(ctx);
        }

        // 播放器存在时持续自驱渲染（60fps，匹配高帧率直播源）
        if self.player.is_some() {
            ctx.request_repaint_after(Duration::from_millis(16));
        }
    }
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
                .insert("cjk".into(), egui::FontData::from_owned(data).into());
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

fn main() {
    // 调试命令（不影响双击启动）
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 2 {
        match args[1].as_str() {
            "danmaku-test" => return cmd_danmaku_test(&args[2..]),
            "relay-test" => return cmd_relay_test(&args[2..]),
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
    let _ = eframe::run_native(
        "斗鱼直播",
        options,
        Box::new(|cc| Ok(Box::new(DouyuApp::new(cc)))),
    );
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
                eprintln!("40 秒内未收到弹幕（{e}）");
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
    let state: relay::StreamState = Arc::new(Mutex::new((rid, 4)));
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
