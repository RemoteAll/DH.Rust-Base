//! 全局日志门面（对应 DH.NCore `XTrace`）：`log()`/`set_log()`、`use_console()`、`use_file()`。
//!
//! 未显式设置时，首次写入懒初始化为控制台日志（自动着色）；
//! 如需文件日志：`use_file("Log")` 或 `use_console_options(true, true)`。

use std::sync::{Arc, RwLock};

use super::{CompositeLog, ConsoleLog, ILog, LogLevel, NullLog, TextFileLog};

/// 全局日志（对应 DH.NCore `XTrace.Log`）
static GLOBAL: RwLock<Option<Arc<dyn ILog>>> = RwLock::new(None);

/// 空日志（不输出任何日志；对应 DH.NCore `Logger.Null`）
pub fn null() -> Arc<dyn ILog> {
    Arc::new(NullLog)
}

/// 获取全局日志接口。未设置时懒初始化为控制台日志（自动着色）。
pub fn log() -> Arc<dyn ILog> {
    if let Some(log) = GLOBAL
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
    {
        return log.clone();
    }

    let mut global = GLOBAL
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    global
        .get_or_insert_with(|| Arc::new(ConsoleLog::new()) as Arc<dyn ILog>)
        .clone()
}

/// 设置全局日志接口（对应 `XTrace.Log` 赋值）
pub fn set_log(log: Arc<dyn ILog>) {
    *GLOBAL
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(log);
}

/// 设置全局日志等级（对应 `XTrace.Log.Level`）
pub fn set_level(level: LogLevel) {
    log().set_level(level);
}

/// 写信息日志（对应 `XTrace.WriteLine`）
pub fn write_line(message: &str) {
    log().info(message);
}

/// 刷写已排队日志（进程退出/升级重启前调用；异步文件日志依赖此同步落盘）。
pub fn flush() {
    log().flush();
}

/// 写异常日志（对应 `XTrace.WriteException`；Error 级别）
pub fn write_exception(err: &dyn std::fmt::Display) {
    log().error(&err.to_string());
}

/// 写日志（`info!` 等宏的入口；过滤在前、格式化在后）
pub fn write_fmt(level: LogLevel, args: std::fmt::Arguments<'_>) {
    log().write_fmt(level, args);
}

/// 使用控制台输出日志（自动着色：标准输出为终端且未设置 `NO_COLOR` 时启用）
pub fn use_console() {
    set_log(Arc::new(ConsoleLog::new()));
}

/// 使用控制台输出日志（对应 `XTrace.UseConsole(useColor, useFileLog)`）。
///
/// `use_file_log` 为 true 时同时启用文件日志（`Log` 目录）。
pub fn use_console_options(use_color: bool, use_file_log: bool) {
    let console: Arc<dyn ILog> = Arc::new(ConsoleLog::with_color(use_color));
    if use_file_log {
        let file: Arc<dyn ILog> = TextFileLog::create("Log");
        set_log(Arc::new(CompositeLog::new(vec![console, file])));
    } else {
        set_log(console);
    }
}

/// 使用文件日志（对应 `XTrace.LogPath` + `TextFileLog`；目录内按天一个文件 `yyyy_MM_dd.log`）
pub fn use_file(dir: impl AsRef<std::path::Path>) {
    set_log(TextFileLog::create(dir));
}

/// 从环境变量 `RUST_LOG` 解析日志等级（默认 Info）。
///
/// 支持 `all/debug/info/warn/error/fatal/off`（不区分大小写）；
/// 形如 `info,my_crate=debug` 的目标级语法只取第一个无条件段。
pub fn level_from_env() -> LogLevel {
    let Ok(text) = std::env::var("RUST_LOG") else {
        return LogLevel::Info;
    };
    for part in text.split(',') {
        let part = part.trim();
        if part.contains('=') {
            continue;
        }
        if let Some(level) = LogLevel::parse(part) {
            return level;
        }
    }
    LogLevel::Info
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::{LogLevel, LogOptions};
    use std::sync::Mutex;

    /// 捕获日志（测试用；写入内存）
    struct CaptureLog {
        options: LogOptions,
        lines: Mutex<Vec<(LogLevel, String)>>,
    }

    impl ILog for CaptureLog {
        fn write(&self, level: LogLevel, message: &str) {
            self.lines
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push((level, message.to_owned()));
        }

        fn enabled(&self) -> bool {
            self.options.enabled()
        }

        fn set_enabled(&self, enabled: bool) {
            self.options.set_enabled(enabled);
        }

        fn level(&self) -> LogLevel {
            self.options.level()
        }

        fn set_level(&self, level: LogLevel) {
            self.options.set_level(level);
        }
    }

    /// 门面宏写入 + 级别过滤（单测试内串行，避免共享全局状态被并行用例干扰）
    #[test]
    fn facade_write_filter_and_macros() {
        let capture = Arc::new(CaptureLog {
            options: LogOptions::new(),
            lines: Mutex::new(Vec::new()),
        });
        set_log(capture.clone());

        // 宏经 `dhrust::logs::` 路径调用（验证宏的模块路径导出）
        crate::logs::info!("成员加入: room={room}", room = 8848);
        crate::logs::debug!("不应输出（级别不足）");

        let lines = capture.lines.lock().unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].0, LogLevel::Info);
        assert_eq!(lines[0].1, "成员加入: room=8848");
        drop(lines);

        set_level(LogLevel::Debug);
        crate::logs::debug!("现在应输出");
        assert_eq!(capture.lines.lock().unwrap().len(), 2);
    }

    /// RUST_LOG 解析（与门面测试无共享状态冲突：仅本测试读写该环境变量）
    #[test]
    fn env_level_parse() {
        let prev = std::env::var("RUST_LOG").ok();

        std::env::set_var("RUST_LOG", "debug");
        assert_eq!(level_from_env(), LogLevel::Debug);

        std::env::set_var("RUST_LOG", "info,my_crate=debug");
        assert_eq!(level_from_env(), LogLevel::Info);

        std::env::set_var("RUST_LOG", "乱写");
        assert_eq!(level_from_env(), LogLevel::Info);

        match prev {
            Some(value) => std::env::set_var("RUST_LOG", value),
            None => std::env::remove_var("RUST_LOG"),
        }
    }
}
