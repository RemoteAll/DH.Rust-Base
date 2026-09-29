//! Windows 控制台初始化（收编自 tcp-scanner-server / Rust.DDNS / Rust.FixHosts 等工程重复实现）。
//!
//! 控制台彩色与中文输出的两件套：
//! - 输出代码页切到 UTF-8(65001)，否则中文日志乱码；
//! - 启用 `ENABLE_VIRTUAL_TERMINAL_PROCESSING`，让 ANSI 颜色转义生效（Win10 1511+）。
//!
//! 其他平台为空操作；`ConsoleLog` 创建时自动调用，独立程序（工具/服务）也可直接调用。

use std::sync::Once;

/// 进程内初始化标记（控制台设置只需生效一次）
static INIT: Once = Once::new();

/// 进程内触发一次控制台初始化（`ConsoleLog` 创建时自动调用）。
pub(crate) fn enable_once() {
    INIT.call_once(enable_windows_console);
}

/// 初始化控制台：Windows 下切换 UTF-8 输出代码页并启用 ANSI 虚拟终端；其他平台空操作。
///
/// 可重复调用（无副作用）；控制台不可用（输出被重定向、服务方式运行）时静默忽略。
pub fn enable_windows_console() {
    #[cfg(windows)]
    {
        // SAFETY: 仅调用控制台 API；句柄来自 GetStdHandle 且已判空/判无效；所有失败路径均忽略返回值。
        unsafe {
            win::SetConsoleOutputCP(65001);

            let handle = win::GetStdHandle(win::STD_OUTPUT_HANDLE);
            if handle != 0 && handle != win::INVALID_HANDLE_VALUE {
                let mut mode: u32 = 0;
                if win::GetConsoleMode(handle, &mut mode) != 0 {
                    win::SetConsoleMode(handle, mode | win::ENABLE_VIRTUAL_TERMINAL_PROCESSING);
                }
            }
        }
    }
}

/// Windows API 最小绑定（裸 FFI，零新增依赖；与组织内 Rust.FixHosts 等工具同做法）。
#[cfg(windows)]
mod win {
    // 设置控制台输出代码页
    unsafe extern "system" {
        pub fn SetConsoleOutputCP(w_code_page_id: u32) -> i32;
        pub fn GetStdHandle(n_std_handle: u32) -> isize;
        pub fn GetConsoleMode(h_console_handle: isize, lp_mode: *mut u32) -> i32;
        pub fn SetConsoleMode(h_console_handle: isize, dw_mode: u32) -> i32;
    }

    /// `(DWORD)-11`：标准输出句柄
    pub const STD_OUTPUT_HANDLE: u32 = 0xFFFF_FFF5;
    /// 无效句柄值 `(HANDLE)-1`
    pub const INVALID_HANDLE_VALUE: isize = -1;
    /// 启用虚拟终端转义序列处理
    pub const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enable_windows_console_never_panics() {
        // 结果依赖运行环境（可能有控制台也可能没有），这里只验证不崩溃且可重复调用
        enable_windows_console();
        enable_windows_console();
    }
}
