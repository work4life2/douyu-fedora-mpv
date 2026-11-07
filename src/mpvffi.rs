//! libmpv FFI（dlopen 动态加载 libmpv.so.2），支持软件渲染 + OpenGL 渲染两种模式。
//!
//! 编译不依赖 mpv-libs；运行时需要系统安装 `mpv-libs`（提供 libmpv.so.2）。
//! - SW 模式：视频渲染进 RGB 缓冲，egui 上传纹理（CPU 重）。
//! - OpenGL 模式：视频渲染到 FBO 纹理，egui 直接复用该 GL 纹理（零拷贝）。
//!
//! 硬解：hwdec=auto-copy-safe（vaapi/nvdec/vdpau 依次探测，失败回退软解），
//!       实际生效值在 [fps] 日志的「硬解」字段里。

use eframe::glow;
use libloading::Library;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::Arc;
use crate::logln;

// mpv_render_param_type 枚举值（mpv/render.h）
const MPV_RENDER_PARAM_INVALID: c_int = 0;
const MPV_RENDER_PARAM_API_TYPE: c_int = 1;
const MPV_RENDER_PARAM_OPENGL_INIT_PARAMS: c_int = 2;
const MPV_RENDER_PARAM_OPENGL_FBO: c_int = 3;
const MPV_RENDER_PARAM_FLIP_Y: c_int = 4;
const MPV_RENDER_PARAM_SW_SIZE: c_int = 17;
const MPV_RENDER_PARAM_SW_FORMAT: c_int = 18;
const MPV_RENDER_PARAM_SW_STRIDE: c_int = 19;
const MPV_RENDER_PARAM_SW_POINTER: c_int = 20;
/// 关掉「渲染到目标时间」的阻塞：GUI 自己有 16ms 重绘节奏，让 mpv 在里面再阻塞
/// 一次显示时钟只会互相打架（而且阻塞式渲染会把 UI 线程拖住）
const MPV_RENDER_PARAM_BLOCK_FOR_TARGET_TIME: c_int = 12;

#[repr(C)]
struct MpvRenderParam {
    type_: c_int,
    data: *mut c_void,
}

#[repr(C)]
struct MpvOpenglInitParams {
    get_proc_address: unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void,
    get_proc_address_ctx: *mut c_void,
}

#[repr(C)]
struct MpvOpenglFbo {
    fbo: c_int,
    w: c_int,
    h: c_int,
    internal_format: c_int,
}

#[repr(C)]
struct MpvHandle {
    _p: [u8; 0],
}
#[repr(C)]
struct MpvRenderContext {
    _p: [u8; 0],
}

type UpdateFn = unsafe extern "C" fn(*mut c_void);

/// GL 函数指针获取：优先 eglGetProcAddress（Linux 桌面 GL via EGL），
/// 失败回退 glXGetProcAddressARB（X11）。供 mpv render API 解析 GL 函数。
struct GlLoader {
    _egl: Option<Library>,
    _glx: Option<Library>,
    egl_get_proc: Option<unsafe extern "C" fn(*const c_char) -> *mut c_void>,
    glx_get_proc: Option<unsafe extern "C" fn(*const c_char) -> *mut c_void>,
}

impl GlLoader {
    fn load() -> Self {
        let mut loader = GlLoader {
            _egl: None,
            _glx: None,
            egl_get_proc: None,
            glx_get_proc: None,
        };
        unsafe {
            if let Ok(lib) = Library::new("libEGL.so.1") {
                loader.egl_get_proc = lib
                    .get::<unsafe extern "C" fn(*const c_char) -> *mut c_void>(b"eglGetProcAddress\0")
                    .ok()
                    .map(|s| *s);
                loader._egl = Some(lib);
            }
            if let Ok(lib) = Library::new("libGL.so.1") {
                loader.glx_get_proc = lib
                    .get::<unsafe extern "C" fn(*const c_char) -> *mut c_void>(b"glXGetProcAddressARB\0")
                    .ok()
                    .map(|s| *s);
                loader._glx = Some(lib);
            }
        }
        loader
    }

    fn get_proc(&self, name: &CStr) -> *mut c_void {
        let n = name.as_ptr();
        if let Some(f) = self.egl_get_proc {
            let p = unsafe { f(n) };
            if !p.is_null() {
                return p;
            }
        }
        if let Some(f) = self.glx_get_proc {
            let p = unsafe { f(n) };
            if !p.is_null() {
                return p;
            }
        }
        std::ptr::null_mut()
    }
}

/// libmpv.so 解析出的函数指针集
struct MpvLib {
    _lib: Library,
    create: unsafe extern "C" fn() -> *mut MpvHandle,
    initialize: unsafe extern "C" fn(*mut MpvHandle) -> c_int,
    #[allow(dead_code)]
    set_option_string: unsafe extern "C" fn(*mut MpvHandle, *const c_char, *const c_char) -> c_int,
    #[allow(dead_code)]
    set_property_string: unsafe extern "C" fn(*mut MpvHandle, *const c_char, *const c_char) -> c_int,
    command: unsafe extern "C" fn(*mut MpvHandle, *const *const c_char) -> c_int,
    get_property: unsafe extern "C" fn(*mut MpvHandle, *const c_char, c_int, *mut c_void) -> c_int,
    get_property_string: unsafe extern "C" fn(*mut MpvHandle, *const c_char) -> *mut c_char,
    error_string: unsafe extern "C" fn(c_int) -> *const c_char,
    terminate_destroy: unsafe extern "C" fn(*mut MpvHandle),
    mpv_free: unsafe extern "C" fn(*mut c_void),
    render_context_create: unsafe extern "C" fn(
        *mut *mut MpvRenderContext,
        *mut MpvHandle,
        *mut MpvRenderParam,
    ) -> c_int,
    render_context_set_update_callback:
        unsafe extern "C" fn(*mut MpvRenderContext, Option<UpdateFn>, *mut c_void),
    render_context_render: unsafe extern "C" fn(*mut MpvRenderContext, *mut MpvRenderParam) -> c_int,
    /// 告诉 mpv「这一帧已经上屏了」。嵌入式渲染里这步不能省：
    /// 不报告就没有呈现时刻，mpv 的帧计时/引用会一路攒着（实测每帧 ~5KB，一小时 1GB）。
    render_context_report_swap: unsafe extern "C" fn(*mut MpvRenderContext),
    render_context_free: unsafe extern "C" fn(*mut MpvRenderContext),
}

impl MpvLib {
    unsafe fn load() -> Result<Self, String> {
        let lib = match Library::new("libmpv.so.2") {
            Ok(l) => l,
            Err(_) => Library::new("libmpv.so").map_err(|e| {
                format!("加载 libmpv 失败（请安装: sudo dnf install mpv-libs）: {e}")
            })?,
        };

        macro_rules! get {
            ($name:literal, $ty:ty) => {
                *lib.get::<$ty>(concat!($name, "\0").as_bytes())
                    .map_err(|e| format!("libmpv 符号 {} 缺失: {e}", $name))?
            };
        }

        // 先取全部符号（借用 lib），最后再把 lib 收进 struct
        let create = get!("mpv_create", unsafe extern "C" fn() -> *mut MpvHandle);
        let initialize =
            get!("mpv_initialize", unsafe extern "C" fn(*mut MpvHandle) -> c_int);
        let set_option_string = get!(
            "mpv_set_option_string",
            unsafe extern "C" fn(*mut MpvHandle, *const c_char, *const c_char) -> c_int
        );
        let set_property_string = get!(
            "mpv_set_property_string",
            unsafe extern "C" fn(*mut MpvHandle, *const c_char, *const c_char) -> c_int
        );
        let command = get!(
            "mpv_command",
            unsafe extern "C" fn(*mut MpvHandle, *const *const c_char) -> c_int
        );
        let get_property = get!(
            "mpv_get_property",
            unsafe extern "C" fn(*mut MpvHandle, *const c_char, c_int, *mut c_void) -> c_int
        );
        let get_property_string = get!(
            "mpv_get_property_string",
            unsafe extern "C" fn(*mut MpvHandle, *const c_char) -> *mut c_char
        );
        let mpv_free = get!("mpv_free", unsafe extern "C" fn(*mut c_void));
        let error_string =
            get!("mpv_error_string", unsafe extern "C" fn(c_int) -> *const c_char);
        let terminate_destroy =
            get!("mpv_terminate_destroy", unsafe extern "C" fn(*mut MpvHandle));
        let render_context_create = get!(
            "mpv_render_context_create",
            unsafe extern "C" fn(
                *mut *mut MpvRenderContext,
                *mut MpvHandle,
                *mut MpvRenderParam,
            ) -> c_int
        );
        let render_context_set_update_callback = get!(
            "mpv_render_context_set_update_callback",
            unsafe extern "C" fn(*mut MpvRenderContext, Option<UpdateFn>, *mut c_void)
        );
        let render_context_render = get!(
            "mpv_render_context_render",
            unsafe extern "C" fn(*mut MpvRenderContext, *mut MpvRenderParam) -> c_int
        );
        let render_context_report_swap = get!(
            "mpv_render_context_report_swap",
            unsafe extern "C" fn(*mut MpvRenderContext)
        );
        let render_context_free =
            get!("mpv_render_context_free", unsafe extern "C" fn(*mut MpvRenderContext));

        Ok(MpvLib {
            _lib: lib,
            create,
            initialize,
            set_option_string,
            set_property_string,
            command,
            get_property,
            get_property_string,
            error_string,
            terminate_destroy,
            mpv_free,
            render_context_create,
            render_context_set_update_callback,
            render_context_render,
            render_context_report_swap,
            render_context_free,
        })
    }
}

unsafe extern "C" fn call_update(data: *mut c_void) {
    if data.is_null() {
        return;
    }
    let cb = &*(data as *mut Box<dyn Fn() + Send>);
    cb();
}

unsafe extern "C" fn gl_get_proc(ctx: *mut c_void, name: *const c_char) -> *mut c_void {
    if ctx.is_null() || name.is_null() {
        return std::ptr::null_mut();
    }
    let loader = &*(ctx as *const GlLoader);
    loader.get_proc(CStr::from_ptr(name))
}

/// 内嵌 mpv 播放器（软件渲染 / OpenGL 渲染）
pub struct MpvPlayer {
    lib: Arc<MpvLib>,
    handle: *mut MpvHandle,
    /// 渲染上下文。用原子指针而不是裸字段，是为了能不重建整个播放器就把它
    /// 换掉（见 `recycle_render_ctx`）——所有渲染调用都在 UI 线程，
    /// 换的时候不会有人在用。
    render_ctx: AtomicPtr<MpvRenderContext>,
    cb_raw: *mut Box<dyn Fn() + Send>,
    gl_loader: Option<Arc<GlLoader>>,
}

impl MpvPlayer {
    fn ctx(&self) -> Result<*mut MpvRenderContext, String> {
        let p = self.render_ctx.load(Ordering::Relaxed);
        if p.is_null() {
            Err("渲染上下文已释放".to_string())
        } else {
            Ok(p)
        }
    }

    /// 重建渲染上下文（GL 与 SW 两种模式都支持）。
    ///
    /// 为什么要这个：这套 libmpv（0.41 + NVIDIA）在 **渲染到外部 FBO** 的路径上，
    /// 每渲染一帧会漏约 5KB 的 C 侧元数据（Rust 侧分配计数完全不涨，见 [mem] 行）。
    /// 一帧 5KB × 30fps ≈ 155KB/s ≈ 1GB/小时，看两三个小时就上 GB。
    /// 实测暂停/无视频时完全不漏，独立 `mpv --vo=gpu` 也不漏，
    /// 说明是 render-API→FBO 这条路径上的问题（report_swap / block-for-target-time
    /// / hwdec / cache / 缩放色彩选项都不是元凶，逐项排除过）。
    ///
    /// 既然桶在渲染上下文里，周期性 free+create 就能把这一段全收回来，
    /// 而且**不影响播放进度**（mpv 的解码/解复用状态在 handle 上，不动）。
    pub fn recycle_render_ctx(&self) -> Result<(), String> {
        unsafe {
            // 1) 摘掉回调再释放，避免释放过程中回调线程还在往里写
            let old = self.render_ctx.swap(std::ptr::null_mut(), Ordering::AcqRel);
            if !old.is_null() {
                (self.lib.render_context_set_update_callback)(old, None, std::ptr::null_mut());
                (self.lib.render_context_free)(old);
            }
            // 2) 按原模式重建
            let mut ctx: *mut MpvRenderContext = std::ptr::null_mut();
            if let Some(loader) = &self.gl_loader {
                let mut gl_init = MpvOpenglInitParams {
                    get_proc_address: gl_get_proc,
                    get_proc_address_ctx: Arc::as_ptr(loader) as *mut c_void,
                };
                let api = CString::new("opengl").unwrap();
                let mut params = [
                    MpvRenderParam {
                        type_: MPV_RENDER_PARAM_API_TYPE,
                        data: api.as_ptr() as *mut c_void,
                    },
                    MpvRenderParam {
                        type_: MPV_RENDER_PARAM_OPENGL_INIT_PARAMS,
                        data: &mut gl_init as *mut MpvOpenglInitParams as *mut c_void,
                    },
                    MpvRenderParam {
                        type_: MPV_RENDER_PARAM_INVALID,
                        data: std::ptr::null_mut(),
                    },
                ];
                let r = (self.lib.render_context_create)(&mut ctx, self.handle, params.as_mut_ptr());
                if r < 0 {
                    return Err(format!("重建 GL 渲染上下文失败: {}", err(&self.lib, r)));
                }
            } else {
                let api = CString::new("sw").unwrap();
                let mut params = [
                    MpvRenderParam {
                        type_: MPV_RENDER_PARAM_API_TYPE,
                        data: api.as_ptr() as *mut c_void,
                    },
                    MpvRenderParam {
                        type_: MPV_RENDER_PARAM_INVALID,
                        data: std::ptr::null_mut(),
                    },
                ];
                let r = (self.lib.render_context_create)(&mut ctx, self.handle, params.as_mut_ptr());
                if r < 0 {
                    return Err(format!("重建 SW 渲染上下文失败: {}", err(&self.lib, r)));
                }
            }
            (self.lib.render_context_set_update_callback)(
                ctx,
                Some(call_update),
                self.cb_raw as *mut c_void,
            );
            self.render_ctx.store(ctx, Ordering::Release);

            // 关键：渲染上下文一释放，挂在它上面的硬解设备（vulkan/vaapi interop）就没了，
            // 解码链会跟着断（实测现象：重建后 `width/height` 变 0、`hwdec-current` 变空，
            // 画面定格）。把视频轨摘掉再挂回去，强制解码器用新上下文的设备重新初始化。
            let _ = self.command(&["set", "vid", "no"]);
            let _ = self.command(&["set", "vid", "auto"]);
            logln!("[mpv] 渲染上下文已重建，视频轨已重新挂载");
            Ok(())
        }
    }
}

unsafe impl Send for MpvPlayer {}
unsafe impl Sync for MpvPlayer {}

fn err(lib: &MpvLib, code: c_int) -> String {
    unsafe {
        let p = (lib.error_string)(code);
        if p.is_null() {
            format!("mpv 错误 {code}")
        } else {
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }
}

/// 额外 mpv 选项：`DOUYU_MPV_OPTS="hwdec=no,cache=no"`。
///
/// 用途是**诊断/救急**：换硬解方式、关缓存、改缩放算法这些，不该为了对比改代码再编译。
/// 逗号分隔 k=v（只写 k 视为 =yes），在下面那批默认选项之后应用，可覆盖它们。
fn env_opts() -> Vec<(String, String)> {
    std::env::var("DOUYU_MPV_OPTS")
        .ok()
        .map(|s| {
            s.split(',')
                .filter_map(|kv| {
                    let mut it = kv.splitn(2, '=');
                    let k = it.next()?.trim().to_string();
                    if k.is_empty() {
                        return None;
                    }
                    let v = it.next().unwrap_or("yes").trim().to_string();
                    Some((k, v))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 把 (k,v) 列表设到 handle 上，失败必须报出来（静默失败的选项排查起来最费劲）
unsafe fn apply_opts(lib: &MpvLib, handle: *mut MpvHandle, list: &[(String, String)]) {
    for (k, v) in list {
        let (Ok(kc), Ok(vc)) = (CString::new(k.as_str()), CString::new(v.as_str())) else {
            logln!("[mpv] 选项 {k} 含非法字符，跳过");
            continue;
        };
        let r = (lib.set_option_string)(handle, kc.as_ptr(), vc.as_ptr());
        if r < 0 {
            logln!("[mpv] 选项 {k}={v} 设置失败 ret={r}");
        } else {
            logln!("[mpv] 选项 {k}={v}（来自 DOUYU_MPV_OPTS）");
        }
    }
}

impl MpvPlayer {
    /// 创建播放器。`on_frame` 在 mpv 有新帧时被调用（任意线程）。
    pub fn new(on_frame: Box<dyn Fn() + Send>) -> Result<Self, String> {
        unsafe {
            let lib = Arc::new(MpvLib::load()?);
            let handle = (lib.create)();
            if handle.is_null() {
                return Err("mpv_create 失败".into());
            }

            // initialize 前的选项
            let opts: &[(&str, &str)] = &[
                ("vo", "libmpv"),
                // auto-copy-safe：依次探测 vaapi/nvdec/vdpau 等并自动回退软解。
                // 之前写死 nvdec-copy，在没有 N 卡的机器上静默变成纯软解且无任何日志。
                ("hwdec", "auto-copy-safe"),
                ("keep-open", "yes"),
                ("osc", "no"),
                ("osd-bar", "no"),
                ("border", "no"),
                // relay 到 CDN 是公网链路，留一点缓冲抗抖动
                // （原来的 cache=no 会把任何抖动直接顶到解码器）
                ("cache", "auto"),
                // 但 cache=auto 的默认上限是 150MiB 解复用缓存 + 75MiB 回看，
                // 直播流用不到、而且这部分内存**涨上去就不再还**（实测基线因此
                // 凭空高 120~150MB，看起来很像泄漏）。32MiB ≈ 30~60 秒，够抗抖动。
                ("demuxer-max-bytes", "32MiB"),
                ("demuxer-max-back-bytes", "8MiB"),
                // 软解减负：直播画面跳过去块滤波肉眼几乎无损，省 10~20% 解码。
                // 硬解成功时该项不生效，无副作用。
                ("vd-lavc-skiploopfilter", "all"),
            ];
            for (k, v) in opts {
                let kc = CString::new(*k).unwrap();
                let vc = CString::new(*v).unwrap();
                let r = (lib.set_option_string)(handle, kc.as_ptr(), vc.as_ptr());
                if r < 0 {
                    // 设置失败必须报出来：display-fps 的 ret=-5 就是这么被静默漏掉的
                    logln!("[mpv] 选项 {k}={v} 设置失败 ret={r}");
                }
            }
            // 环境变量覆盖（诊断/救急：换硬解、关缓存等，不用改代码重编）
            apply_opts(&lib, handle, &env_opts());

            let r = (lib.initialize)(handle);
            if r < 0 {
                let msg = format!("mpv_initialize 失败: {}", err(&lib, r));
                (lib.terminate_destroy)(handle);
                return Err(msg);
            }

            // 软件渲染上下文
            let api = CString::new("sw").unwrap();
            let mut ctx: *mut MpvRenderContext = std::ptr::null_mut();
            let mut params = [
                MpvRenderParam {
                    type_: MPV_RENDER_PARAM_API_TYPE,
                    data: api.as_ptr() as *mut c_void,
                },
                MpvRenderParam {
                    type_: MPV_RENDER_PARAM_INVALID,
                    data: std::ptr::null_mut(),
                },
            ];
            let r = (lib.render_context_create)(&mut ctx, handle, params.as_mut_ptr());
            if r < 0 {
                let msg = format!("创建渲染上下文失败: {}", err(&lib, r));
                (lib.terminate_destroy)(handle);
                return Err(msg);
            }

            let cb_raw = Box::into_raw(Box::new(on_frame));
            (lib.render_context_set_update_callback)(ctx, Some(call_update), cb_raw as *mut c_void);

            Ok(MpvPlayer {
                lib,
                handle,
                render_ctx: AtomicPtr::new(ctx),
                cb_raw,
                gl_loader: None,
            })
        }
    }

    /// 创建 OpenGL 渲染模式的播放器（渲染到 FBO，纹理零拷贝给 egui）。
    pub fn new_opengl(on_frame: Box<dyn Fn() + Send>) -> Result<Self, String> {
        unsafe {
            let lib = Arc::new(MpvLib::load()?);
            let handle = (lib.create)();
            if handle.is_null() {
                return Err("mpv_create 失败".into());
            }

            let opts: &[(&str, &str)] = &[
                ("vo", "libmpv"),
                // auto-copy-safe：依次探测 vaapi/nvdec/vdpau 等并自动回退软解。
                // 之前写死 nvdec-copy，在没有 N 卡的机器上静默变成纯软解且无任何日志。
                ("hwdec", "auto-copy-safe"),
                ("keep-open", "yes"),
                ("osc", "no"),
                ("osd-bar", "no"),
                ("border", "no"),
                // relay 到 CDN 是公网链路，留一点缓冲抗抖动
                // （原来的 cache=no 会把任何抖动直接顶到解码器）
                ("cache", "auto"),
                // 但 cache=auto 的默认上限是 150MiB 解复用缓存 + 75MiB 回看，
                // 直播流用不到、而且这部分内存**涨上去就不再还**（实测基线因此
                // 凭空高 120~150MB，看起来很像泄漏）。32MiB ≈ 30~60 秒，够抗抖动。
                ("demuxer-max-bytes", "32MiB"),
                ("demuxer-max-back-bytes", "8MiB"),
                // GL 渲染到 FBO 时强制 sRGB + full range 输出（否则可能偏亮/发灰）
                ("target-trc", "srgb"),
                ("video-output-levels", "full"),
                // 软解减负：直播画面跳过去块滤波肉眼几乎无损，省 10~20% 解码。
                // 硬解成功时该项不生效，无副作用。
                ("vd-lavc-skiploopfilter", "all"),
                // 缩放/降采样用最便宜的滤镜。2K 原画缩到窗口大小是每帧
                // 百万像素级的着色工作，bilinear 对直播画面足够
                // （默认的 correct-downscaling 在核显/llvmpipe 上明显更贵）
                ("scale", "bilinear"),
                ("dscale", "bilinear"),
                ("correct-downscaling", "no"),
                ("linear-downscaling", "no"),
                ("sigmoid-upscaling", "no"),
            ];
            for (k, v) in opts {
                let kc = CString::new(*k).unwrap();
                let vc = CString::new(*v).unwrap();
                let r = (lib.set_option_string)(handle, kc.as_ptr(), vc.as_ptr());
                if r < 0 {
                    // 设置失败必须报出来：display-fps 的 ret=-5 就是这么被静默漏掉的
                    logln!("[mpv] 选项 {k}={v} 设置失败 ret={r}");
                }
            }
            // 环境变量覆盖（诊断/救急：换硬解算法、关缓存等，不用改代码重编）
            apply_opts(&lib, handle, &env_opts());

            // render API 模式下 mpv 感知不到显示器刷新率，调度会保守。
            // 实测（日志 ret=-5 = MPV_ERROR_OPTION_NOT_FOUND）：这套 libmpv 里
            // display-fps 已不是可写选项（新版本它是自动检测的只读属性，
            // 可写的覆盖叫 override-display-fps），逐个尝试；都失败也不影响播放
            // —— 渲染节奏由 request_repaint_after(16ms) 驱动，不依赖 mpv 的显示时钟。
            let fps = std::env::var("DOUYU_DISPLAY_FPS").unwrap_or_else(|_| "60".to_string());
            let mut fps_set = false;
            for name in ["display-fps", "override-display-fps"] {
                let k = CString::new(name).unwrap();
                let v = CString::new(fps.clone()).unwrap();
                let r = (lib.set_option_string)(handle, k.as_ptr(), v.as_ptr());
                logln!("[mpv] {name}={fps}（set_option ret={r}）");
                if r >= 0 {
                    fps_set = true;
                    break;
                }
            }
            if !fps_set {
                logln!("[mpv] 当前 libmpv 不支持手动设置显示刷新率，忽略（不影响渲染节奏）");
            }

            let r = (lib.initialize)(handle);
            if r < 0 {
                let msg = format!("mpv_initialize 失败: {}", err(&lib, r));
                (lib.terminate_destroy)(handle);
                return Err(msg);
            }

            // OpenGL 渲染上下文：API_TYPE="opengl" + OPENGL_INIT_PARAMS(get_proc_address)
            let loader = Arc::new(GlLoader::load());
            let mut gl_init = MpvOpenglInitParams {
                get_proc_address: gl_get_proc,
                get_proc_address_ctx: Arc::as_ptr(&loader) as *mut c_void,
            };
            let api = CString::new("opengl").unwrap();
            let mut ctx: *mut MpvRenderContext = std::ptr::null_mut();
            let mut params = [
                MpvRenderParam {
                    type_: MPV_RENDER_PARAM_API_TYPE,
                    data: api.as_ptr() as *mut c_void,
                },
                MpvRenderParam {
                    type_: MPV_RENDER_PARAM_OPENGL_INIT_PARAMS,
                    data: &mut gl_init as *mut MpvOpenglInitParams as *mut c_void,
                },
                MpvRenderParam {
                    type_: MPV_RENDER_PARAM_INVALID,
                    data: std::ptr::null_mut(),
                },
            ];
            let r = (lib.render_context_create)(&mut ctx, handle, params.as_mut_ptr());
            if r < 0 {
                let msg = format!("创建 GL 渲染上下文失败: {}", err(&lib, r));
                (lib.terminate_destroy)(handle);
                return Err(msg);
            }

            let cb_raw = Box::into_raw(Box::new(on_frame));
            (lib.render_context_set_update_callback)(ctx, Some(call_update), cb_raw as *mut c_void);

            Ok(MpvPlayer {
                lib,
                handle,
                render_ctx: AtomicPtr::new(ctx),
                cb_raw,
                gl_loader: Some(loader),
            })
        }
    }

    /// 发送命令（变长字符串数组）
    pub fn command(&self, args: &[&str]) -> Result<(), String> {
        unsafe {
            let cargs: Vec<CString> = args
                .iter()
                .map(|s| CString::new(*s).map_err(|_| "命令含非法字符".to_string()))
                .collect::<Result<_, _>>()?;
            let mut ptrs: Vec<*const c_char> = cargs.iter().map(|s| s.as_ptr()).collect();
            ptrs.push(std::ptr::null());
            let r = (self.lib.command)(self.handle, ptrs.as_ptr());
            if r < 0 {
                Err(err(&self.lib, r))
            } else {
                Ok(())
            }
        }
    }

    /// 加载播放地址
    pub fn loadfile(&self, url: &str) -> Result<(), String> {
        self.command(&["loadfile", url, "replace"])
    }

    /// 设置字符串属性
    #[allow(dead_code)]
    pub fn set_property(&self, name: &str, value: &str) -> Result<(), String> {
        unsafe {
            let n = CString::new(name).unwrap();
            let v = CString::new(value).unwrap();
            let r = (self.lib.set_property_string)(self.handle, n.as_ptr(), v.as_ptr());
            if r < 0 {
                Err(err(&self.lib, r))
            } else {
                Ok(())
            }
        }
    }

    /// 读取 double 类型属性（如 estimated-frame-rate / container-fps）
    pub fn get_f64(&self, name: &str) -> Option<f64> {
        unsafe {
            let n = CString::new(name).ok()?;
            let mut v: f64 = 0.0;
            // MPV_FORMAT_DOUBLE = 5
            let r = (self.lib.get_property)(
                self.handle,
                n.as_ptr(),
                5,
                &mut v as *mut f64 as *mut c_void,
            );
            if r >= 0 {
                Some(v)
            } else {
                None
            }
        }
    }

    /// 读字符串属性（如 hwdec-current，判断硬解到底有没有生效）。
    /// mpv 分配的返回内存读完立即释放。
    pub fn get_string(&self, name: &str) -> Option<String> {
        unsafe {
            let n = CString::new(name).ok()?;
            let p = (self.lib.get_property_string)(self.handle, n.as_ptr());
            if p.is_null() {
                return None;
            }
            let s = CStr::from_ptr(p).to_string_lossy().into_owned();
            (self.lib.mpv_free)(p as *mut c_void);
            Some(s)
        }
    }

    /// 软件渲染一帧到 buf（rgb0，4 字节/像素）。
    pub fn render_sw(&self, w: usize, h: usize, buf: &mut [u8]) -> Result<(), String> {
        unsafe {
            let size: [c_int; 2] = [w as c_int, h as c_int];
            let format = CString::new("rgb0").unwrap();
            // SW_STRIDE 是 size_t*（8 字节），必须是像素大小的倍数
            let mut stride: usize = w * 4;
            let mut block: c_int = 0;
            let mut params = [
                MpvRenderParam {
                    type_: MPV_RENDER_PARAM_SW_SIZE,
                    data: size.as_ptr() as *mut c_void,
                },
                MpvRenderParam {
                    type_: MPV_RENDER_PARAM_SW_FORMAT,
                    data: format.as_ptr() as *mut c_void,
                },
                MpvRenderParam {
                    type_: MPV_RENDER_PARAM_SW_STRIDE,
                    data: &mut stride as *mut usize as *mut c_void,
                },
                MpvRenderParam {
                    type_: MPV_RENDER_PARAM_SW_POINTER,
                    data: buf.as_mut_ptr() as *mut c_void,
                },
                MpvRenderParam {
                    type_: MPV_RENDER_PARAM_BLOCK_FOR_TARGET_TIME,
                    data: &mut block as *mut c_int as *mut c_void,
                },
                MpvRenderParam {
                    type_: MPV_RENDER_PARAM_INVALID,
                    data: std::ptr::null_mut(),
                },
            ];
            let ctx = self.ctx()?;
            let r = (self.lib.render_context_render)(ctx, params.as_mut_ptr());
            if r < 0 {
                Err(err(&self.lib, r))
            } else {
                (self.lib.render_context_report_swap)(ctx);
                Ok(())
            }
        }
    }

    /// OpenGL 渲染一帧到指定 FBO（FBO 绑定了一个 RGBA 纹理）。
    pub fn render_gl(&self, fbo: c_int, w: usize, h: usize) -> Result<(), String> {
        unsafe {
            let fbo_param = MpvOpenglFbo {
                fbo,
                w: w as c_int,
                h: h as c_int,
                // 必须是 sRGB 纹理格式：egui_glow 的 shader 编译期按 SRGB_TEXTURES 处理所有纹理，
                // 普通 RGBA8 会被再转一次 linear→sRGB 导致画面发白。
                internal_format: glow::SRGB8_ALPHA8 as i32,
            };
            // FBO（非默认 framebuffer）渲染不需要翻转
            let mut flip: c_int = 0;
            let mut block: c_int = 0;
            let mut params = [
                MpvRenderParam {
                    type_: MPV_RENDER_PARAM_OPENGL_FBO,
                    data: &fbo_param as *const MpvOpenglFbo as *mut c_void,
                },
                MpvRenderParam {
                    type_: MPV_RENDER_PARAM_FLIP_Y,
                    data: &mut flip as *mut c_int as *mut c_void,
                },
                MpvRenderParam {
                    type_: MPV_RENDER_PARAM_BLOCK_FOR_TARGET_TIME,
                    data: &mut block as *mut c_int as *mut c_void,
                },
                MpvRenderParam {
                    type_: MPV_RENDER_PARAM_INVALID,
                    data: std::ptr::null_mut(),
                },
            ];
            let ctx = self.ctx()?;
            let r = (self.lib.render_context_render)(ctx, params.as_mut_ptr());
            if r < 0 {
                Err(err(&self.lib, r))
            } else {
                // 报告「已上屏」：正规嵌入式用法（虽然实测它并不是这个泄漏的元凶，
                // 但不报告会让 mpv 的帧计时一直等不到呈现时刻）
                (self.lib.render_context_report_swap)(ctx);
                Ok(())
            }
        }
    }
}

impl Drop for MpvPlayer {
    fn drop(&mut self) {
        unsafe {
            let ctx = self.render_ctx.swap(std::ptr::null_mut(), Ordering::AcqRel);
            if !ctx.is_null() {
                (self.lib.render_context_set_update_callback)(ctx, None, std::ptr::null_mut());
                (self.lib.render_context_free)(ctx);
            }
            (self.lib.terminate_destroy)(self.handle);
            drop(Box::from_raw(self.cb_raw));
        }
    }
}
