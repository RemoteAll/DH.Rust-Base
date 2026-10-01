//! 控制台日志（对应 DH.NCore `ConsoleLog`）。
//!
//! 输出经队列异步落到标准输出（批量、不阻塞调用方）；着色规则对齐 DH.NCore：
//! 警告黄色、错误/严重错误红色，其余级别（含调试/信息）按线程号取自 10 色调色板，
//! 1 号线程灰色。默认自动着色：标准输出为终端且未设置 `NO_COLOR` 时启用
//! （对齐现代 Rust 生态习惯；DH.NCore 为 Windows 控制台默认固定着色）。

use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;

use super::{format_line, ILog, LogLevel, LogOptions, MAX_QUEUE};

/// 控制台日志（对应 DH.NCore `ConsoleLog`；线程安全，可 `Arc` 共享）
pub struct ConsoleLog {
    state: LogOptions,
    tx: Sender<(LogLevel, String, i32)>,
    queued: Arc<AtomicUsize>,
}

impl ConsoleLog {
    /// 新建（自动着色：与 [`style`](super::style) 同源——标准输出为终端且未设置 `NO_COLOR` 时启用）
    pub fn new() -> ConsoleLog {
        ConsoleLog::with_color(super::style::color_enabled())
    }

    /// 新建（指定是否着色）
    pub fn with_color(use_color: bool) -> ConsoleLog {
        // Windows：先切换 UTF-8 输出代码页并启用 ANSI 虚拟终端（否则中文乱码、颜色转义不生效）
        super::console_setup::enable_once();

        let (tx, rx) = std::sync::mpsc::channel::<(LogLevel, String, i32)>();
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
fn console_loop(rx: &Receiver<(LogLevel, String, i32)>, use_color: bool, queued: &AtomicUsize) {
    let mut stdout = std::io::stdout();
    while let Ok(first) = rx.recv() {
        let mut lines = vec![first];
        while let Ok(item) = rx.try_recv() {
            lines.push(item);
        }
        for (level, line, thread_id) in &lines {
            queued.fetch_sub(1, Ordering::Relaxed);
            write_line(&mut stdout, *level, *thread_id, line, use_color);
        }
        let _ = stdout.flush();
    }
}

/// 调色板（按线程号取模索引）。颜色顺序对齐 DH.NCore `ConsoleLog.colors`，
/// 以 ANSI 近似表达：亮色 9x / 标准色 3x。
static PALETTE: [&str; 10] = ["92", "96", "95", "97", "93", "32", "36", "35", "31", "33"];

/// 取日志行颜色码（对齐 DH.NCore：警告黄、错误红，其余按线程号；1 号线程灰色）。
fn color_code(level: LogLevel, thread_id: i32) -> &'static str {
    match level {
        LogLevel::Warn => "33",
        LogLevel::Error | LogLevel::Fatal => "31",
        _ => {
            if thread_id == 1 {
                "37"
            } else {
                PALETTE[(thread_id % PALETTE.len() as i32) as usize]
            }
        }
    }
}

/// 输出一行（着色规则对齐 DH.NCore；颜色关闭时纯文本）
fn write_line(out: &mut impl Write, level: LogLevel, thread_id: i32, line: &str, use_color: bool) {
    if use_color {
        let code = color_code(level, thread_id);
        let _ = writeln!(out, "\x1b[{code}m{line}\x1b[0m");
    } else {
        let _ = writeln!(out, "{line}");
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
        let line = format_line(level, message);
        if self.tx.send((level, line.text, line.thread_id)).is_ok() {
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
        // 警告黄色（对齐 DH.NCore：Warn → Yellow）
        let mut out: Vec<u8> = Vec::new();
        write_line(&mut out, LogLevel::Warn, 2, "警告文本", true);
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("\x1b[33m"), "警告应着黄色: {text:?}");
        assert!(text.trim_end().ends_with("\x1b[0m"), "应复位颜色: {text:?}");
        assert!(text.contains("警告文本"));

        // 1 号线程灰色（对齐 DH.NCore：threadid == 1 → Gray）
        let mut out: Vec<u8> = Vec::new();
        write_line(&mut out, LogLevel::Info, 1, "信息文本", true);
        assert!(
            String::from_utf8(out).unwrap().starts_with("\x1b[37m"),
            "1 号线程应着灰色"
        );

        // 其它线程按调色板取色：2 → Magenta（对齐 C# colors[2]）
        let mut out: Vec<u8> = Vec::new();
        write_line(&mut out, LogLevel::Info, 2, "信息文本", true);
        assert!(
            String::from_utf8(out).unwrap().starts_with("\x1b[95m"),
            "2 号线程应着 Magenta"
        );

        // 颜色关闭时纯文本
        let mut plain: Vec<u8> = Vec::new();
        write_line(&mut plain, LogLevel::Warn, 2, "警告文本", false);
        assert_eq!(String::from_utf8(plain).unwrap().trim_end(), "警告文本");
    }
}
