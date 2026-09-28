//! Razor 子集模板引擎：F014 生成代码（本机 dylib）加载器。
//!
//! 配合 [`crate::razor::codegen`]：把模板编译成 cdylib 后，由本模块在进程内加载并渲染。
//! 零新依赖（手工声明平台 FFI：Windows `LoadLibraryW`/`GetProcAddress`，Unix `dlopen`/`dlsym`）。
//!
//! 使用（伪代码）：
//! ```ignore
//! let src = Template::parse(tpl_text)?.to_rust_lib_source();   // 交 tools/razor-native 编译为 dll
//! let tpl = NativeTemplate::load("template.dll")?;
//! let html = tpl.render(&model)?;                              // 输出与解释器逐字节一致
//! ```
//!
//! 安全说明：加载 dll 意味着执行其代码——只允许加载本工程（dhrust 代码生成器 + rustc）
//! 产出的可信产物；dll 与宿主必须由**同一 dhrust 版本**编译（值布局一致）。

use std::ffi::c_void;
use std::path::Path;

use crate::razor::codegen::RAZOR_CODEGEN_ABI;
use crate::razor::error::RenderError;
use crate::razor::value::Value;
use crate::razor::Options;

/// 生成代码的渲染入口签名（与 `codegen.rs` 生成物一致）。
type RenderFn =
    unsafe extern "C" fn(*const Value, *mut String, bool, *mut *mut RenderError) -> bool;

/// 已加载的原生模板（渲染输出与解释器逐字节一致）。
pub struct NativeTemplate {
    handle: *mut c_void,
    render_fn: RenderFn,
}

/// 加载失败原因（找不到文件、符号缺失、ABI 不匹配等）。
#[derive(Debug)]
pub struct NativeLoadError(pub String);

impl std::fmt::Display for NativeLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for NativeLoadError {}

impl NativeTemplate {
    /// 加载生成代码 dylib（校验 `razor_abi_version` 与入口符号）。
    pub fn load(path: impl AsRef<Path>) -> Result<Self, NativeLoadError> {
        let path = path.as_ref();
        let handle = sys::load(path)?;
        // abi 校验：缺失或版本不符一律拒绝（防"旧代码生成器产物 + 新运行时"的错位）
        let abi_sym = sys::sym(handle, b"razor_abi_version\0");
        let Some(abi_sym) = abi_sym else {
            sys::free(handle);
            return Err(NativeLoadError(format!(
                "缺少 razor_abi_version 符号：{}（不是本引擎生成的产物？）",
                path.display()
            )));
        };
        let abi_fn: extern "C" fn() -> u32 = unsafe { std::mem::transmute(abi_sym) };
        let abi = abi_fn();
        if abi != RAZOR_CODEGEN_ABI {
            sys::free(handle);
            return Err(NativeLoadError(format!(
                "ABI 版本不匹配：产物 {abi}，运行时 {}（请用当前 dhrust 重新生成）",
                RAZOR_CODEGEN_ABI
            )));
        }
        let Some(render_sym) = sys::sym(handle, b"razor_render\0") else {
            sys::free(handle);
            return Err(NativeLoadError(format!(
                "缺少 razor_render 入口符号：{}",
                path.display()
            )));
        };
        let render_fn: RenderFn = unsafe { std::mem::transmute(render_sym) };
        Ok(Self { handle, render_fn })
    }

    /// 渲染模板（默认选项：转义开启）。
    pub fn render(&self, model: &Value) -> Result<String, RenderError> {
        self.render_with(model, &Options::default())
    }

    /// 按指定选项渲染模板（仅 `escape` 生效；嵌套深度由生成代码结构决定、无需运行时保护）。
    pub fn render_with(&self, model: &Value, options: &Options) -> Result<String, RenderError> {
        // 小页面一次分配到位（与解释器一致）
        let mut out = String::with_capacity(4096);
        let mut err: *mut RenderError = std::ptr::null_mut();
        let ok = unsafe {
            (self.render_fn)(
                model as *const Value,
                &mut out as *mut String,
                options.escape,
                &mut err,
            )
        };
        if ok {
            return Ok(out);
        }
        if err.is_null() {
            return Err(RenderError::new("原生渲染", "未知错误（错误指针为空）"));
        }
        // 所有权归还：dll 内 Box::into_raw，宿主 Box::from_raw（同一分配器）
        let boxed = unsafe { Box::from_raw(err) };
        Err(*boxed)
    }
}

impl Drop for NativeTemplate {
    fn drop(&mut self) {
        sys::free(self.handle);
    }
}

// ————— 平台 FFI —————

#[cfg(windows)]
mod sys {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    use crate::razor::native::NativeLoadError;

    // 说明：采用手工 FFI 而非 libloading 依赖（razor 特性保持零新依赖）
    unsafe extern "system" {
        fn LoadLibraryW(name: *const u16) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
        fn FreeLibrary(module: *mut c_void) -> i32;
    }

    pub fn load(path: &Path) -> Result<*mut c_void, NativeLoadError> {
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let handle = unsafe { LoadLibraryW(wide.as_ptr()) };
        if handle.is_null() {
            return Err(NativeLoadError(format!(
                "LoadLibraryW 失败：{}（{}）",
                path.display(),
                std::io::Error::last_os_error()
            )));
        }
        Ok(handle)
    }

    pub fn sym(handle: *mut c_void, name: &[u8]) -> Option<*mut c_void> {
        debug_assert!(name.ends_with(b"\0"));
        let p = unsafe { GetProcAddress(handle, name.as_ptr()) };
        if p.is_null() {
            None
        } else {
            Some(p)
        }
    }

    pub fn free(handle: *mut c_void) {
        unsafe {
            let _ = FreeLibrary(handle);
        }
    }
}

#[cfg(unix)]
mod sys {
    use std::ffi::c_void;
    use std::path::Path;

    use crate::razor::native::NativeLoadError;

    const RTLD_NOW: i32 = 2;

    unsafe extern "C" {
        fn dlopen(file: *const u8, mode: i32) -> *mut c_void;
        fn dlsym(handle: *mut c_void, name: *const u8) -> *mut c_void;
        fn dlclose(handle: *mut c_void) -> i32;
    }

    pub fn load(path: &Path) -> Result<*mut c_void, NativeLoadError> {
        let mut bytes = path.as_os_str().as_encoded_bytes().to_vec();
        bytes.push(0);
        let handle = unsafe { dlopen(bytes.as_ptr(), RTLD_NOW) };
        if handle.is_null() {
            return Err(NativeLoadError(format!(
                "dlopen 失败：{}（{}）",
                path.display(),
                std::io::Error::last_os_error()
            )));
        }
        Ok(handle)
    }

    pub fn sym(handle: *mut c_void, name: &[u8]) -> Option<*mut c_void> {
        debug_assert!(name.ends_with(b"\0"));
        let p = unsafe { dlsym(handle, name.as_ptr()) };
        if p.is_null() {
            None
        } else {
            Some(p)
        }
    }

    pub fn free(handle: *mut c_void) {
        unsafe {
            let _ = dlclose(handle);
        }
    }
}
