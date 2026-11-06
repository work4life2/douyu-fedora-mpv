//! libmpv FFI（dlopen 动态加载 libmpv.so.2），支持软件渲染 + OpenGL 渲染两种模式。
//!
//! 编译不依赖 mpv-libs；运行时需要系统安装 `mpv-libs`（提供 libmpv.so.2）。
//! - SW 模式：视频渲染进 RGB 缓冲，egui 上传纹理（CPU 重）。
//! - OpenGL 模式：视频渲染到 FBO 纹理，egui 直接复用该 GL 纹理（零拷贝）。
//!
//! 硬解：OpenGL 模式用 hwdec=nvdec-copy（GPU 解码拷回，mpv GL 渲染），
//!       渲染结果零拷贝给 egui。

use eframe::glow;
use libloading::Library;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::sync::Arc;

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
    error_string: unsafe extern "C" fn(c_int) -> *const c_char,
    terminate_destroy: unsafe extern "C" fn(*mut MpvHandle),
    render_context_create: unsafe extern "C" fn(
        *mut *mut MpvRenderContext,
        *mut MpvHandle,
        *mut MpvRenderParam,
    ) -> c_int,
    render_context_set_update_callback:
        unsafe extern "C" fn(*mut MpvRenderContext, Option<UpdateFn>, *mut c_void),
    render_context_render: unsafe extern "C" fn(*mut MpvRenderContext, *mut MpvRenderParam) -> c_int,
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
            error_string,
            terminate_destroy,
            render_context_create,
            render_context_set_update_callback,
            render_context_render,
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
    render_ctx: *mut MpvRenderContext,
    cb_raw: *mut Box<dyn Fn() + Send>,
    _gl_loader: Option<Arc<GlLoader>>,
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
                ("hwdec", "nvdec-copy"),
                ("keep-open", "yes"),
                ("osc", "no"),
                ("osd-bar", "no"),
                ("border", "no"),
                ("cache", "no"),
            ];
            for (k, v) in opts {
                let k = CString::new(*k).unwrap();
                let v = CString::new(*v).unwrap();
                (lib.set_option_string)(handle, k.as_ptr(), v.as_ptr());
            }

            let r = (lib.initialize)(handle);
            if r < 0 {
                return Err(format!("mpv_initialize 失败: {}", err(&lib, r)));
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
                return Err(format!("创建渲染上下文失败: {}", err(&lib, r)));
            }

            let cb_raw = Box::into_raw(Box::new(on_frame));
            (lib.render_context_set_update_callback)(ctx, Some(call_update), cb_raw as *mut c_void);

            Ok(MpvPlayer {
                lib,
                handle,
                render_ctx: ctx,
                cb_raw,
                _gl_loader: None,
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
                ("hwdec", "nvdec-copy"),
                ("keep-open", "yes"),
                ("osc", "no"),
                ("osd-bar", "no"),
                ("border", "no"),
                ("cache", "no"),
                // GL 渲染到 FBO 时强制 sRGB + full range 输出（否则可能偏亮/发灰）
                ("target-trc", "srgb"),
                ("video-output-levels", "full"),
            ];
            for (k, v) in opts {
                let k = CString::new(*k).unwrap();
                let v = CString::new(*v).unwrap();
                (lib.set_option_string)(handle, k.as_ptr(), v.as_ptr());
            }

            let r = (lib.initialize)(handle);
            if r < 0 {
                return Err(format!("mpv_initialize 失败: {}", err(&lib, r)));
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
                return Err(format!("创建 GL 渲染上下文失败: {}", err(&lib, r)));
            }

            let cb_raw = Box::into_raw(Box::new(on_frame));
            (lib.render_context_set_update_callback)(ctx, Some(call_update), cb_raw as *mut c_void);

            Ok(MpvPlayer {
                lib,
                handle,
                render_ctx: ctx,
                cb_raw,
                _gl_loader: Some(loader),
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

    /// 软件渲染一帧到 buf（rgb0，4 字节/像素）。
    pub fn render_sw(&self, w: usize, h: usize, buf: &mut [u8]) -> Result<(), String> {
        unsafe {
            let size: [c_int; 2] = [w as c_int, h as c_int];
            let format = CString::new("rgb0").unwrap();
            // SW_STRIDE 是 size_t*（8 字节），必须是像素大小的倍数
            let mut stride: usize = (w * 4) as usize;
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
                    type_: MPV_RENDER_PARAM_INVALID,
                    data: std::ptr::null_mut(),
                },
            ];
            let r = (self.lib.render_context_render)(self.render_ctx, params.as_mut_ptr());
            if r < 0 {
                Err(err(&self.lib, r))
            } else {
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
                    type_: MPV_RENDER_PARAM_INVALID,
                    data: std::ptr::null_mut(),
                },
            ];
            let r = (self.lib.render_context_render)(self.render_ctx, params.as_mut_ptr());
            if r < 0 {
                Err(err(&self.lib, r))
            } else {
                Ok(())
            }
        }
    }
}

impl Drop for MpvPlayer {
    fn drop(&mut self) {
        unsafe {
            (self.lib.render_context_set_update_callback)(self.render_ctx, None, std::ptr::null_mut());
            (self.lib.render_context_free)(self.render_ctx);
            (self.lib.terminate_destroy)(self.handle);
            drop(Box::from_raw(self.cb_raw));
        }
    }
}
