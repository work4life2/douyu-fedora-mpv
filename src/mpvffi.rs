//! libmpv FFI（dlopen 动态加载 libmpv.so.2），软件渲染模式。
//!
//! 编译不依赖 mpv-libs；运行时需要系统安装 `mpv-libs`（提供 libmpv.so.2）。
//! 视频 + OSD（含弹幕）渲染进 RGB 缓冲，由 egui 作为纹理显示 —— 单窗口内嵌。
//!
//! 硬解：hwdec=nvdec-copy（GPU 解码拷回，SW 输出必需）。

use libloading::Library;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::sync::Arc;

// mpv_render_param_type 枚举值（mpv/render.h，mpv 0.36+ 的 SW 渲染）
const MPV_RENDER_PARAM_INVALID: c_int = 0;
const MPV_RENDER_PARAM_API_TYPE: c_int = 1;
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
struct MpvHandle {
    _p: [u8; 0],
}
#[repr(C)]
struct MpvRenderContext {
    _p: [u8; 0],
}

type UpdateFn = unsafe extern "C" fn(*mut c_void);

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

/// 内嵌 mpv 播放器（软件渲染）
pub struct MpvPlayer {
    lib: Arc<MpvLib>,
    handle: *mut MpvHandle,
    render_ctx: *mut MpvRenderContext,
    cb_raw: *mut Box<dyn Fn() + Send>,
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
