//! 复合日志（对应 DH.NCore `CompositeLog`）：同时向多路日志输出。

use std::sync::Arc;

use super::{ILog, LogLevel, LogOptions};

/// 复合日志：写日志时按顺序转发给每个子日志（子日志各自再做级别过滤）。
pub struct CompositeLog {
    logs: Vec<Arc<dyn ILog>>,
    state: LogOptions,
}

impl CompositeLog {
    /// 新建
    pub fn new(logs: Vec<Arc<dyn ILog>>) -> CompositeLog {
        CompositeLog {
            logs,
            state: LogOptions::new(),
        }
    }

    /// 子日志列表
    pub fn logs(&self) -> &[Arc<dyn ILog>] {
        &self.logs
    }
}

impl ILog for CompositeLog {
    fn write(&self, level: LogLevel, message: &str) {
        if !self.state.should_log(level) {
            return;
        }
        for log in &self.logs {
            log.write(level, message);
        }
    }

    fn enabled(&self) -> bool {
        self.state.enabled()
    }

    fn set_enabled(&self, enabled: bool) {
        self.state.set_enabled(enabled);
        for log in &self.logs {
            log.set_enabled(enabled);
        }
    }

    fn level(&self) -> LogLevel {
        self.state.level()
    }

    fn set_level(&self, level: LogLevel) {
        // 对齐 DH.NCore：外层级别同步下发到子日志
        self.state.set_level(level);
        for log in &self.logs {
            log.set_level(level);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 捕获日志（测试用；写入内存）
    struct CaptureLog {
        options: LogOptions,
        lines: Mutex<Vec<(LogLevel, String)>>,
    }

    impl CaptureLog {
        fn new() -> Arc<CaptureLog> {
            Arc::new(CaptureLog {
                options: LogOptions::new(),
                lines: Mutex::new(Vec::new()),
            })
        }

        fn lines(&self) -> Vec<(LogLevel, String)> {
            self.lines
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        }
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

    #[test]
    fn forwards_to_all_and_propagates_level() {
        let first = CaptureLog::new();
        let second = CaptureLog::new();
        let composite = CompositeLog::new(vec![first.clone(), second.clone()]);

        composite.info("双路输出");
        assert_eq!(first.lines().len(), 1);
        assert_eq!(second.lines().len(), 1);
        assert_eq!(first.lines()[0].1, "双路输出");

        // 外层级别同步下发到子日志：设为 Warn 后 Info 被过滤
        composite.set_level(LogLevel::Warn);
        composite.info("不应输出");
        assert_eq!(first.lines().len(), 1);

        composite.warn("警告输出");
        assert_eq!(second.lines().len(), 2);
    }
}
