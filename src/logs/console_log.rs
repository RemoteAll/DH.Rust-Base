//! 控制台日志（对应 DH.NCore `ConsoleLog`）。
//!
//! 输出经队列异步落到标准输出（批量、不阻塞调用方）；级别着色：
//! 调试灰色、信息默认色、警告黄色、错误/严重错误红色。
//! 默认自动着色：标准输出为终端且未设置 `NO_COLOR` 时启用
//! （对齐现代 Rust 生态习惯；DH.NCore 为 Windows 控制台默认固定着色）。

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;

use super::{format_line, ILog, LogLevel, LogOptions, MAX_QUEUE};

/// 控制台日志（对应 DH.NCore `ConsoleLog`；线程安全，可 `Arc` 共享）
pub struct ConsoleLog {
    state: LogOptions,
    tx: Sender<(LogLevel, String)>,
    queued: Arc<AtomicUsize>,
}

impl ConsoleLog {
    /// 新建（自动着色：标准输出为终端且未设置 `NO_COLOR` 时启用）
    pub fn new() -> ConsoleLog {
        let auto = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
        ConsoleLog::with_color(auto)
    }

    /// 新建（指定是否着色）
    pub fn with_color(use_color: bool) -> ConsoleLog {
        // Windows：先切换 UTF-8 输出代码页并启用 ANSI 虚拟终端（否则中文乱码、颜色转义不生效）
        super::console_setup::enable_once();

        let (tx, rx) = std::sync::mpsc::channel::<(LogLevel, String)>();
        let queued = Arc::new(AtomicUsize::new(0));
        let worker_queued = queued.clone();
        let _ = std::thread::Builder::new()
            .name("dhrust-console-log".to_owned())
            .spawn(move || console_loop(&rx, use_color, &worker_queued));
        ConsoleLog {
            state: LogOptions::new(),
            tx,
            queued,
        }
    }
}

impl Default for ConsoleLog {
    fn default() -> Self {
        Self::new()
    }
}

/// 控制台写线程：批量输出（写入错误静默——控制台不可用不应影响进程）
fn console_loop(rx: &Receiver<(LogLevel, String)>, use_color: bool, queued: &AtomicUsize) {
    let mut stdout = std::io::stdout();
    while let Ok(first) = rx.recv() {
        let mut lines = vec![first];
        while let Ok(item) = rx.try_recv() {
            lines.push(item);
        }
        for (level, line) in &lines {
            queued.fetch_sub(1, Ordering::Relaxed);
            write_line(&mut stdout, *level, line, use_color);
        }
        let _ = stdout.flush();
    }
}

/// 输出一行（着色仅对调试/警告/错误启用，信息行不着色）
fn write_line(out: &mut impl Write, level: LogLevel, line: &str, use_color: bool) {
    if !use_color {
        let _ = writeln!(out, "{line}");
        return;
    }
    let color = match level {
        LogLevel::Debug => "\x1b[90m",
        LogLevel::Warn => "\x1b[33m",
        LogLevel::Error | LogLevel::Fatal => "\x1b[31m",
        _ => "",
    };
    if color.is_empty() {
        let _ = writeln!(out, "{line}");
    } else {
        let _ = writeln!(out, "{color}{line}\x1b[0m");
    }
}

impl ILog for ConsoleLog {
    fn write(&self, level: LogLevel, message: &str) {
        if !self.state.should_log(level) {
            return;
        }
        // 队列积压（输出端阻塞等）时丢弃新日志，防内存无界
        if self.queued.load(Ordering::Relaxed) > MAX_QUEUE {
            return;
        }
        if self.tx.send((level, format_line(level, message))).is_ok() {
            self.queued.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn enabled(&self) -> bool {
        self.state.enabled()
    }

    fn set_enabled(&self, enabled: bool) {
        self.state.set_enabled(enabled);
    }

    fn level(&self) -> LogLevel {
        self.state.level()
    }

    fn set_level(&self, level: LogLevel) {
        self.state.set_level(level);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_line_shape() {
        let mut out: Vec<u8> = Vec::new();
        write_line(&mut out, LogLevel::Warn, "警告文本", true);
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("\x1b[33m"), "警告应着黄色: {text:?}");
        assert!(text.trim_end().ends_with("\x1b[0m"), "应复位颜色: {text:?}");
        assert!(text.contains("警告文本"));

        let mut plain: Vec<u8> = Vec::new();
        write_line(&mut plain, LogLevel::Warn, "警告文本", false);
        assert_eq!(String::from_utf8(plain).unwrap().trim_end(), "警告文本");

        let mut info: Vec<u8> = Vec::new();
        write_line(&mut info, LogLevel::Info, "信息文本", true);
        assert!(
            String::from_utf8(info).unwrap().starts_with("信息文本"),
            "信息行不应带颜色"
        );
    }
}
