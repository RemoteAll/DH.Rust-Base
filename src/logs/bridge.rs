//! 日志门面桥接：把 `log` / `tracing` 宏转到 `dhrust::logs`。
//!
//! ## 为什么需要
//!
//! `dhrust::logs` 自带文件/控制台实现，但**不安装 `log` 门面的全局 logger**。
//! 业务代码里遍布 `log::info!`（以及 rmqtt 等第三方库的 `tracing::info!`），
//! 不桥接的话这些日志会**静默丢弃**（`log::set_logger` 未设置）。
//!
//! 本模块把两种桥接收进库里（原先各项目各写一份，逐字同构）：
//! - [`install_log_bridge`]：`log` crate → dhrust（feature `log-bridge`）；
//! - [`install_tracing_bridge`]：`tracing` → dhrust（feature `tracing-bridge`）；
//! - [`init`]：一步装好「控制台 + 按日文件 + 两种桥接」。
//!
//! ## 与宿主日志共存
//!
//! 已被宿主（例如 rmqtt broker）安装过全局 logger / subscriber 时，
//! `set_logger`/`set_global_default` 返回错误——这里**静默忽略**，继续沿用宿主实现，
//! 不会因为重复安装而 panic。

use std::path::Path;
use std::sync::Arc;
#[cfg(any(feature = "log-bridge", feature = "tracing-bridge"))]
use std::sync::Once;

use crate::logs::{CompositeLog, ConsoleLog, ILog, LogLevel, TextFileLog};

// ————— log crate 桥接 —————

#[cfg(feature = "log-bridge")]
mod log_impl {
    use crate::logs::{write_fmt, LogLevel};

    /// `dhrust::logs::LogLevel` ← `log::Level`（dhrust 无 Trace 档，并入 Debug）。
    ///
    /// # 参数
    /// - `level`：`log` 门面级别。
    ///
    /// # 返回值
    /// dhrust 级别。
    pub fn map_level(level: log::Level) -> LogLevel {
        match level {
            log::Level::Error => LogLevel::Error,
            log::Level::Warn => LogLevel::Warn,
            log::Level::Info => LogLevel::Info,
            log::Level::Debug | log::Level::Trace => LogLevel::Debug,
        }
    }

    /// 只转发不加工的 `log::Log` 实现（级别过滤交给 dhrust，避免两套阈值不一致）。
    pub struct DhrustBridge;

    impl log::Log for DhrustBridge {
        fn enabled(&self, _metadata: &log::Metadata<'_>) -> bool {
            true
        }

        fn log(&self, record: &log::Record<'_>) {
            write_fmt(map_level(record.level()), *record.args());
        }

        fn flush(&self) {}
    }
}

#[cfg(feature = "log-bridge")]
pub use log_impl::DhrustBridge;

/// 安装 `log` 门面桥接（幂等；宿主已装 logger 时静默沿用宿主）。
#[cfg(feature = "log-bridge")]
pub fn install_log_bridge() {
    static BRIDGE: log_impl::DhrustBridge = log_impl::DhrustBridge;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        if log::set_logger(&BRIDGE).is_ok() {
            log::set_max_level(log::LevelFilter::Trace);
        }
    });
}

// ————— tracing 桥接 —————

#[cfg(feature = "tracing-bridge")]
mod tracing_impl {
    use tracing::field::{Field, Visit};
    use tracing::{Event, Level, Subscriber};
    use tracing_subscriber::layer::{Context, Layer};

    use crate::logs::LogLevel;

    /// `tracing::Level` ← dhrust 级别映射（dhrust 无 Trace 档，并入 Debug）。
    ///
    /// # 参数
    /// - `level`：tracing 级别。
    ///
    /// # 返回值
    /// dhrust 级别。
    pub fn map_level(level: Level) -> LogLevel {
        match level {
            Level::TRACE | Level::DEBUG => LogLevel::Debug,
            Level::INFO => LogLevel::Info,
            Level::WARN => LogLevel::Warn,
            Level::ERROR => LogLevel::Error,
        }
    }

    /// 把 tracing 事件拼成一行文本的访问器（`message` 原样，其余 `key=value`）。
    #[derive(Default)]
    pub struct MessageVisitor {
        text: String,
    }

    impl MessageVisitor {
        fn push_field(&mut self, name: &str, value: &str) {
            if !self.text.is_empty() {
                self.text.push(' ');
            }
            if name == "message" {
                self.text.push_str(value);
            } else {
                self.text.push_str(name);
                self.text.push('=');
                self.text.push_str(value);
            }
        }
    }

    impl Visit for MessageVisitor {
        fn record_str(&mut self, field: &Field, value: &str) {
            self.push_field(field.name(), value);
        }

        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.push_field(field.name(), &format!("{value:?}"));
        }
    }

    /// 把 tracing 事件写到 `dhrust::logs` 的图层。
    pub struct DhrustLayer;

    impl<S: Subscriber> Layer<S> for DhrustLayer {
        fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
            let meta = event.metadata();
            let level = map_level(*meta.level());
            let sink = crate::logs::log();
            if !sink.should_log(level) {
                return;
            }
            let mut visitor = MessageVisitor::default();
            event.record(&mut visitor);
            let text = match meta.line() {
                Some(line) => format!("{}:{line}: {}", meta.target(), visitor.text),
                None => format!("{}: {}", meta.target(), visitor.text),
            };
            sink.write(level, &text);
        }
    }
}

#[cfg(feature = "tracing-bridge")]
pub use tracing_impl::{map_level as map_tracing_level, DhrustLayer, MessageVisitor};

/// 安装 `tracing` 全局订阅者（幂等；已装过则静默保留首个）。
///
/// 注意：**不要**再用 `tracing_subscriber::fmt::try_init()`——它会安装 `LogTracer`，
/// 与 [`install_log_bridge`] 的方向冲突（两者只能留一个）。
#[cfg(feature = "tracing-bridge")]
pub fn install_tracing_bridge() {
    use tracing_subscriber::layer::SubscriberExt;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let subscriber = tracing_subscriber::registry().with(tracing_impl::DhrustLayer);
        let _ = tracing::subscriber::set_global_default(subscriber);
    });
}

// ————— 一步初始化 —————

/// 一步初始化：控制台（自动着色）+ 按日文件 + `log`/`tracing` 桥接。
///
/// # 参数
/// - `dir`：日志目录（通常 `{base}/Log`，不存在会自动创建）。
/// - `use_console`：是否同时输出到控制台。
/// - `level`：dhrust 日志级别（推荐 [`LogLevel::Info`]）。
pub fn init(dir: impl AsRef<Path>, use_console: bool, level: LogLevel) {
    #[cfg(feature = "log-bridge")]
    install_log_bridge();

    let mut logs: Vec<Arc<dyn ILog>> = Vec::new();
    if use_console {
        // ConsoleLog::new() = 自动着色（标准输出是终端且未设 NO_COLOR 才加 ANSI）
        logs.push(Arc::new(ConsoleLog::new()) as Arc<dyn ILog>);
    }
    logs.push(TextFileLog::create(dir.as_ref()) as Arc<dyn ILog>);
    crate::logs::set_log(Arc::new(CompositeLog::new(logs)));
    crate::logs::set_level(level);

    #[cfg(feature = "tracing-bridge")]
    install_tracing_bridge();
}

#[cfg(all(test, feature = "log-bridge"))]
mod tests {
    /// 桥接幂等：重复安装不 panic，日志能写出去。
    #[test]
    fn init_is_idempotent() {
        let dir = std::env::temp_dir().join("dhrust-logs-bridge-test");
        super::init(&dir, false, crate::logs::LogLevel::Info);
        super::init(&dir, false, crate::logs::LogLevel::Info);
        log::info!("桥接自检");
        crate::logs::flush();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
