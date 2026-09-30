//! 日志模块（对应 DH.NCore `NewLife.Log`）：分级、多路输出的轻量日志。
//!
//! 设计对齐 DH.NCore 日志体系（`XTrace`/`ILog`/`Logger`/`TextFileLog`/`ConsoleLog`/`CompositeLog`）：
//!
//! - [`LogLevel`]：All/Debug/Info/Warn/Error/Fatal/Off——只输出大于等于配置级别的日志，默认 Info；
//! - [`ILog`]：日志接口；实现方只需提供 write/enabled/level 五件套，其余方法有默认实现；
//! - [`TextFileLog`]：文本文件日志——异步队列落盘、按天与按大小滚动、备份清理、进程日志头；
//! - [`ConsoleLog`]：控制台日志——队列异步输出、级别着色（警告黄/错误红/调试灰）；
//! - [`enable_windows_console`]：Windows 控制台初始化——UTF-8 输出代码页 + ANSI 虚拟终端（`ConsoleLog` 创建时自动调用）；
//! - [`CompositeLog`]：复合日志——多路同时输出（如控制台 + 文件）；
//! - 全局门面：`log()`/`set_log()`/`use_console()`/`use_file()` 及 `info!` 等宏（对应 `XTrace`）。
//!
//! # 快速上手
//!
//! ```no_run
//! // 控制台输出；RUST_LOG=debug 可调级别（默认 Info）
//! dhrust::logs::use_console();
//! dhrust::logs::set_level(dhrust::logs::level_from_env());
//!
//! dhrust::logs::info!("服务已启动：端口 {port}", port = 8080);
//! dhrust::logs::warn!("磁盘空间不足");
//! ```
//!
//! 文件日志（默认目录内按天一个文件 `yyyy_MM_dd.log`；单文件 10MB 后拆分；最多保留 100 份）：
//!
//! ```no_run
//! dhrust::logs::use_file("Log");
//! dhrust::logs::info!("写入 Log 目录");
//! ```

mod composite_log;
mod console_log;
mod console_setup;
mod text_file_log;
mod xtrace;

pub mod style;

pub use composite_log::CompositeLog;
pub use console_log::ConsoleLog;
pub use console_setup::enable_windows_console;
pub use text_file_log::{FileLogOptions, TextFileLog};
pub use xtrace::{
    level_from_env, log, null, set_level, set_log, use_console, use_console_options, use_file,
    write_exception, write_fmt, write_line,
};

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

/// 日志队列积压上限：超过后丢弃新日志（对齐 DH.NCore——磁盘故障/输出阻塞时防内存无界增长）
const MAX_QUEUE: usize = 1024;

// ————— 日志宏（`#[macro_export]` 导出到 crate 根；文件末尾再导出为 `dhrust::logs::info!` 等路径） —————

/// 写调试日志（延迟格式化：未达级别时不格式化、不分配）
#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => {
        $crate::logs::write_fmt($crate::logs::LogLevel::Debug, ::std::format_args!($($arg)*))
    };
}

/// 写信息日志（延迟格式化：未达级别时不格式化、不分配）
#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => {
        $crate::logs::write_fmt($crate::logs::LogLevel::Info, ::std::format_args!($($arg)*))
    };
}

/// 写警告日志（延迟格式化：未达级别时不格式化、不分配）
#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => {
        $crate::logs::write_fmt($crate::logs::LogLevel::Warn, ::std::format_args!($($arg)*))
    };
}

/// 写错误日志（延迟格式化：未达级别时不格式化、不分配）
#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => {
        $crate::logs::write_fmt($crate::logs::LogLevel::Error, ::std::format_args!($($arg)*))
    };
}

/// 写严重错误日志（延迟格式化：未达级别时不格式化、不分配）
#[macro_export]
macro_rules! fatal {
    ($($arg:tt)*) => {
        $crate::logs::write_fmt($crate::logs::LogLevel::Fatal, ::std::format_args!($($arg)*))
    };
}

// ————— 日志等级 —————

/// 日志等级（对应 DH.NCore `LogLevel`）。
///
/// 只输出大于等于配置级别的日志（`All` 打开全部、`Off` 关闭全部），默认 `Info`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    /// 打开所有日志记录
    All = 0,
    /// 最低调试。细粒度信息事件对调试应用程序非常有帮助
    Debug = 1,
    /// 普通消息。在粗粒度级别上突出强调应用程序的运行过程
    Info = 2,
    /// 警告
    Warn = 3,
    /// 错误
    Error = 4,
    /// 严重错误
    Fatal = 5,
    /// 关闭所有日志记录
    Off = 0xFF,
}

impl LogLevel {
    /// 等级名称（大写；用于日志行文本）
    pub fn as_str(self) -> &'static str {
        match self {
            LogLevel::All => "ALL",
            LogLevel::Debug => "DEBUG",
            LogLevel::Info => "INFO",
            LogLevel::Warn => "WARN",
            LogLevel::Error => "ERROR",
            LogLevel::Fatal => "FATAL",
            LogLevel::Off => "OFF",
        }
    }

    /// 解析等级名称（不区分大小写；支持 all/debug/info/warn(warning)/error/fatal/off）
    pub fn parse(name: &str) -> Option<LogLevel> {
        match name.trim().to_ascii_lowercase().as_str() {
            "all" => Some(LogLevel::All),
            "debug" => Some(LogLevel::Debug),
            "info" => Some(LogLevel::Info),
            "warn" | "warning" => Some(LogLevel::Warn),
            "error" => Some(LogLevel::Error),
            "fatal" => Some(LogLevel::Fatal),
            "off" => Some(LogLevel::Off),
            _ => None,
        }
    }

    /// 由字节还原（越界回退 Off）
    fn from_u8(value: u8) -> LogLevel {
        match value {
            0 => LogLevel::All,
            1 => LogLevel::Debug,
            2 => LogLevel::Info,
            3 => LogLevel::Warn,
            4 => LogLevel::Error,
            5 => LogLevel::Fatal,
            _ => LogLevel::Off,
        }
    }
}

impl std::fmt::Display for LogLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ————— 开关与级别（过滤状态） —————

/// 日志开关与级别（线程安全；各日志器共用的过滤状态，对应 DH.NCore `Logger.Enable/Level`）。
#[derive(Debug)]
pub struct LogOptions {
    enabled: AtomicBool,
    level: AtomicU8,
}

impl LogOptions {
    /// 新建（默认启用、级别 Info）
    pub fn new() -> LogOptions {
        LogOptions {
            enabled: AtomicBool::new(true),
            level: AtomicU8::new(LogLevel::Info as u8),
        }
    }

    /// 是否启用日志。为 false 时不输出任何日志
    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// 设置是否启用
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    /// 日志等级。只输出大于等于该级别的日志
    pub fn level(&self) -> LogLevel {
        LogLevel::from_u8(self.level.load(Ordering::Relaxed))
    }

    /// 设置日志等级
    pub fn set_level(&self, level: LogLevel) {
        self.level.store(level as u8, Ordering::Relaxed);
    }

    /// 过滤判定：启用 且 级别达标（`level >= 阈值`；`All`/`Off` 作为记录级别时不输出）
    pub fn should_log(&self, level: LogLevel) -> bool {
        self.enabled()
            && level >= self.level()
            && level != LogLevel::All
            && level != LogLevel::Off
    }
}

impl Default for LogOptions {
    fn default() -> Self {
        Self::new()
    }
}

// ————— 日志接口 —————

/// 日志接口（对应 DH.NCore `ILog`）。
///
/// 实现方只需提供 `write/enabled/set_enabled/level/set_level`，其余便捷方法有默认实现。
pub trait ILog: Send + Sync {
    /// 写日志（输出落点；实现方首行按 [`LogOptions::should_log`] 判定后再输出）
    fn write(&self, level: LogLevel, message: &str);

    /// 是否启用日志。为 false 时不输出任何日志
    fn enabled(&self) -> bool;

    /// 设置是否启用
    fn set_enabled(&self, enabled: bool);

    /// 日志等级。只输出大于等于该级别的日志
    fn level(&self) -> LogLevel;

    /// 设置日志等级
    fn set_level(&self, level: LogLevel);

    /// 过滤判定：启用 且 级别达标
    fn should_log(&self, level: LogLevel) -> bool {
        self.enabled() && level >= self.level() && level != LogLevel::All && level != LogLevel::Off
    }

    /// 写日志（延迟格式化入口；`info!` 等宏最终走这里，未达标时不进行格式化）
    fn write_fmt(&self, level: LogLevel, args: std::fmt::Arguments<'_>) {
        if self.should_log(level) {
            self.write(level, &args.to_string());
        }
    }

    /// 调试日志
    fn debug(&self, message: &str) {
        self.write(LogLevel::Debug, message);
    }

    /// 信息日志
    fn info(&self, message: &str) {
        self.write(LogLevel::Info, message);
    }

    /// 警告日志
    fn warn(&self, message: &str) {
        self.write(LogLevel::Warn, message);
    }

    /// 错误日志
    fn error(&self, message: &str) {
        self.write(LogLevel::Error, message);
    }

    /// 严重错误日志
    fn fatal(&self, message: &str) {
        self.write(LogLevel::Fatal, message);
    }
}

/// 空日志实现（对应 DH.NCore `Logger.Null`）：不输出任何日志。
pub struct NullLog;

impl ILog for NullLog {
    fn write(&self, _level: LogLevel, _message: &str) {}

    fn enabled(&self) -> bool {
        false
    }

    fn set_enabled(&self, _enabled: bool) {}

    fn level(&self) -> LogLevel {
        LogLevel::Off
    }

    fn set_level(&self, _level: LogLevel) {}
}

// ————— 行格式 —————

/// 构建标准日志行：`yyyy-MM-dd HH:mm:ss.fff [级别] 正文`。
///
/// 文件与控制台共用（对应 DH.NCore `WriteLogEventArgs.ToString` 的默认行格式；
/// Rust 版在行内带日期——控制台场景没有按天文件名可依赖）。
pub(crate) fn format_line(level: LogLevel, message: &str) -> String {
    let now = crate::times::format_datetime_ms(&chrono::Local::now().naive_local());
    format!("{now} [{}] {message}", level.as_str())
}

// ————— 宏路径导出（`dhrust::logs::info!` 等） —————

pub use crate::{debug, error, fatal, info, warn};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_parse_and_order() {
        assert_eq!(LogLevel::parse("info"), Some(LogLevel::Info));
        assert_eq!(LogLevel::parse("WARNING"), Some(LogLevel::Warn));
        assert_eq!(LogLevel::parse("乱写"), None);
        assert!(LogLevel::Debug < LogLevel::Info);
        assert!(LogLevel::Fatal < LogLevel::Off);
        assert_eq!(LogLevel::Info.as_str(), "INFO");
        assert_eq!(LogLevel::Info.to_string(), "INFO");
    }

    #[test]
    fn options_filter() {
        let options = LogOptions::new();
        assert!(options.should_log(LogLevel::Info));
        assert!(options.should_log(LogLevel::Error));
        assert!(!options.should_log(LogLevel::Debug));

        options.set_level(LogLevel::All);
        assert!(options.should_log(LogLevel::Debug));

        options.set_enabled(false);
        assert!(!options.should_log(LogLevel::Fatal));
    }

    #[test]
    fn line_format_shape() {
        let line = format_line(LogLevel::Warn, "磁盘空间不足");
        assert!(line.contains(" [WARN] 磁盘空间不足"), "行格式不符: {line}");
        // 形如 2026-09-29 12:34:56.789 [WARN] ...
        assert_eq!(line.as_bytes().get(4), Some(&b'-'));
    }
}
