//! 文本文件日志（对应 DH.NCore `TextFileLog`）。
//!
//! - 异步写入：日志先入队，由专用写线程批量落盘（调用方不阻塞在磁盘 IO 上）；
//! - 滚动：按天一个文件（默认 `yyyy_MM_dd.log`），单文件超过 10MB 拆分为 `_2`、`_3`…；
//! - 备份：目录内日志文件超过上限后删除最旧的（默认保留 200 份）；
//! - 日志头：每个进程首次写入时输出进程/环境信息（字段与列序对齐 DH.NCore `GetHead`）；
//! - 行格式：`HH:mm:ss.fff 线程ID 类型 名称 正文`（对齐 DH.NCore 默认 `LogLineFormat`）；
//! - 正文换行：Windows CRLF / 其它 LF（对齐 C# `TextWriter.WriteLine` 的 `Environment.NewLine`）；
//! - 空闲 5 秒自动关闭文件句柄；队列积压超过 1024 条时丢弃新日志（防内存无界）。

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use chrono::Local;

use super::{format_line, ILog, LogLevel, LogOptions, MAX_QUEUE};

/// 空闲多久后关闭日志文件句柄（对齐 DH.NCore「连续 5 秒没日志就关闭」）
const IDLE_CLOSE: Duration = Duration::from_secs(5);
/// 文件创建失败的重试阈值（同一目标文件失败 3 次后不再尝试，换文件后重置；对齐 DH.NCore）
const MAX_OPEN_ERRORS: usize = 3;
/// 正文换行：对齐 C# `TextWriter.WriteLine` 使用的 `Environment.NewLine`（Windows CRLF / 其它 LF）
const EOL: &str = if cfg!(windows) { "\r\n" } else { "\n" };

/// 文件日志选项
#[derive(Debug, Clone)]
pub struct FileLogOptions {
    /// 单文件大小上限（字节；超过后拆分 `_2`、`_3`…；0 不限制）。默认 10MB（对齐 DH.NCore）
    pub max_bytes: u64,
    /// 备份个数上限（目录内日志文件超过后删除最旧的；0 不限制）。默认 200（对齐 DH.NCore）
    pub backups: usize,
    /// 文件名格式：`{0:yyyy_MM_dd}`（C# 风格）与 `{date}` 占位符都展开为 `yyyy_MM_dd`。
    /// 默认 `{0:yyyy_MM_dd}.log`（对齐 DH.NCore `Setting.LogFileFormat`）
    pub file_format: String,
}

impl Default for FileLogOptions {
    fn default() -> Self {
        Self {
            max_bytes: 10 * 1024 * 1024,
            backups: 200,
            file_format: "{0:yyyy_MM_dd}.log".to_owned(),
        }
    }
}

/// 写线程与日志器共享的状态
struct Shared {
    dir: PathBuf,
    options: FileLogOptions,
    state: LogOptions,
    /// 当前打开的文件句柄（写线程持有；`flush` 时短暂借用）
    file: Mutex<Option<File>>,
    /// 已入队未写出的日志条数
    queued: AtomicUsize,
    /// 当前目标文件创建失败计数（换文件后重置；对齐 DH.NCore）
    open_errors: AtomicUsize,
}

/// 写线程消息
enum Msg {
    /// 一行日志
    Line(String),
    /// 刷写同步点（等待已入队日志落盘）
    Flush(Sender<()>),
}

/// 写线程的文件状态
#[derive(Default)]
struct WriterState {
    /// 当前打开的文件路径
    current: Option<PathBuf>,
    /// 是否已写过日志头（每进程一次，对齐 DH.NCore）
    head_written: bool,
    /// 待写入的提示行（备份清理产生；随下一批日志落盘，对齐 DH.NCore 提示入队延迟写入）
    pending_tips: Vec<String>,
}

/// 进程内文件日志实例缓存（对应 DH.NCore `TextFileLog` 静态 cache）
static CACHE: LazyLock<Mutex<HashMap<String, Arc<TextFileLog>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 文本文件日志（对应 DH.NCore `TextFileLog`；线程安全，可 `Arc` 共享）
pub struct TextFileLog {
    shared: Arc<Shared>,
    tx: Sender<Msg>,
}

impl TextFileLog {
    /// 创建（或复用）指定目录的文件日志。
    ///
    /// 相同目录（忽略大小写）与文件名格式返回同一实例（对齐 DH.NCore 静态缓存）。
    pub fn create(dir: impl AsRef<Path>) -> Arc<TextFileLog> {
        TextFileLog::create_with(dir, FileLogOptions::default())
    }

    /// 创建（或复用）指定目录的文件日志（自定义选项）。
    pub fn create_with(dir: impl AsRef<Path>, options: FileLogOptions) -> Arc<TextFileLog> {
        let dir = dir.as_ref();
        // 相同目录只应有一个实例（对齐 DH.NCore 静态缓存；键含文件名格式）
        let key = format!(
            "{}|{}",
            dir.to_string_lossy().to_lowercase(),
            options.file_format
        );
        let mut cache = CACHE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        cache
            .entry(key)
            .or_insert_with(|| Arc::new(TextFileLog::new_inner(dir.to_path_buf(), options)))
            .clone()
    }

    /// 立即刷写已排队的日志（同步等待落盘；对齐 DH.NCore「销毁前把队列日志输出」）。
    pub fn flush(&self) {
        let (ack_tx, ack_rx) = std::sync::mpsc::channel::<()>();
        if self.tx.send(Msg::Flush(ack_tx)).is_ok() {
            // 写线程异常退出时不做无限等待
            let _ = ack_rx.recv_timeout(IDLE_CLOSE);
        }
    }

    fn new_inner(dir: PathBuf, options: FileLogOptions) -> TextFileLog {
        let (tx, rx) = std::sync::mpsc::channel::<Msg>();
        let shared = Arc::new(Shared {
            dir,
            options,
            state: LogOptions::new(),
            file: Mutex::new(None),
            queued: AtomicUsize::new(0),
            open_errors: AtomicUsize::new(0),
        });
        let writer_shared = shared.clone();
        let _ = std::thread::Builder::new()
            .name("dhrust-log".to_owned())
            .spawn(move || writer_loop(&writer_shared, &rx));
        TextFileLog { shared, tx }
    }
}

impl ILog for TextFileLog {
    fn write(&self, level: LogLevel, message: &str) {
        if !self.shared.state.should_log(level) {
            return;
        }
        // 队列积压（磁盘故障等）时丢弃新日志，防内存无界
        if self.shared.queued.load(Ordering::Relaxed) > MAX_QUEUE {
            return;
        }
        if self.tx.send(Msg::Line(format_line(level, message).text)).is_ok() {
            self.shared.queued.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn flush(&self) {
        TextFileLog::flush(self);
    }

    fn enabled(&self) -> bool {
        self.shared.state.enabled()
    }

    fn set_enabled(&self, enabled: bool) {
        self.shared.state.set_enabled(enabled);
    }

    fn level(&self) -> LogLevel {
        self.shared.state.level()
    }

    fn set_level(&self, level: LogLevel) {
        self.shared.state.set_level(level);
    }
}

/// 写线程：批量落盘、空闲关文件、排队刷写同步点
fn writer_loop(shared: &Shared, rx: &Receiver<Msg>) {
    let mut writer = WriterState::default();
    loop {
        match rx.recv_timeout(IDLE_CLOSE) {
            Ok(first) => {
                // 收集同批消息（批量落盘；Flush 同步点与批内已有日志保持顺序）
                let mut batch: Vec<String> = Vec::new();
                let mut acks: Vec<Sender<()>> = Vec::new();
                collect(first, shared, &mut batch, &mut acks);
                while let Ok(msg) = rx.try_recv() {
                    collect(msg, shared, &mut batch, &mut acks);
                }

                if !batch.is_empty() {
                    write_batch(shared, &mut writer, &batch);
                }
                flush_file(shared);
                for ack in acks {
                    let _ = ack.send(());
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                close_file(shared);
                // 空闲时清理超量备份（对齐 DH.NCore 定时器回调：关闭文件后检查目录备份数）
                enqueue_prune_tips(shared, &mut writer);
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    // 发送端全部销毁：消息已按序交付完毕，关闭文件句柄
    close_file(shared);
}

/// 收集一条消息（Line 计数递减；Flush 记录同步点）
fn collect(msg: Msg, shared: &Shared, batch: &mut Vec<String>, acks: &mut Vec<Sender<()>>) {
    match msg {
        Msg::Line(line) => {
            shared.queued.fetch_sub(1, Ordering::Relaxed);
            batch.push(line);
        }
        Msg::Flush(ack) => acks.push(ack),
    }
}

/// 写入一批日志（确定目标文件；必要时切换并清理超量备份）
fn write_batch(shared: &Shared, writer: &mut WriterState, batch: &[String]) {
    // 候选文件全部达到上限时放弃本批（对齐 DH.NCore `GetLogFile` 返回 null）
    let Some(target) = pick_file(shared) else {
        return;
    };

    let mut file_guard = shared
        .file
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if writer.current.as_deref() != Some(target.as_path()) {
        // 切换文件：关闭旧句柄、重置失败计数、清理超量备份
        *file_guard = None;
        writer.current = Some(target.clone());
        shared.open_errors.store(0, Ordering::Relaxed);
        enqueue_prune_tips(shared, writer);
    }

    if file_guard.is_none() {
        // 创建失败重试上限（对齐 DH.NCore：同一目标失败 3 次后放弃，待换文件时重试）
        if shared.open_errors.load(Ordering::Relaxed) >= MAX_OPEN_ERRORS {
            return;
        }
        if std::fs::create_dir_all(&shared.dir).is_err() {
            shared.open_errors.fetch_add(1, Ordering::Relaxed);
            return;
        }
        match OpenOptions::new().create(true).append(true).open(&target) {
            Ok(mut file) => {
                if !writer.head_written {
                    writer.head_written = true;
                    // 追加到已有内容的文件时，先空一行分隔（对齐 DH.NCore；换行随平台）
                    if file.metadata().map(|meta| meta.len() > 10).unwrap_or(false) {
                        let _ = file.write_all(EOL.as_bytes());
                    }
                    let _ = file.write_all(process_head().as_bytes());
                }
                *file_guard = Some(file);
            }
            Err(_) => {
                shared.open_errors.fetch_add(1, Ordering::Relaxed);
                return;
            }
        }
    }

    if let Some(file) = file_guard.as_mut() {
        for line in batch {
            let _ = file.write_all(line.as_bytes());
            let _ = file.write_all(EOL.as_bytes());
        }

        // 清理提示行落盘（对齐 DH.NCore：删除超量文件后补一条 Info 日志）
        if !writer.pending_tips.is_empty() {
            for tip in writer.pending_tips.drain(..) {
                let _ = file.write_all(tip.as_bytes());
                let _ = file.write_all(EOL.as_bytes());
            }
        }
    }
}

/// 清理超量备份，并把删除提示暂存到下一批写入（对齐 DH.NCore：`OnWrite` 提示经队列延迟落盘）
fn enqueue_prune_tips(shared: &Shared, writer: &mut WriterState) {
    for (name, size) in prune_backups(shared) {
        let tip = format!(
            "日志文件达到上限 {}，删除 {}，大小 {}Byte",
            shared.options.backups,
            name,
            format_thousands(size)
        );
        writer
            .pending_tips
            .push(format_line(LogLevel::Info, &tip).text);
    }
}

/// 数字千分位（对齐 C# 格式串 `{2:n0}`，如 `1,234,567`）
fn format_thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// 刷写当前文件（把缓冲数据写入磁盘）
fn flush_file(shared: &Shared) {
    let mut guard = shared
        .file
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(file) = guard.as_mut() {
        let _ = file.flush();
    }
}

/// 关闭当前文件句柄（空闲后调用）
fn close_file(shared: &Shared) {
    let mut guard = shared
        .file
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *guard = None;
}

/// 选择当前应写入的文件。
///
/// `{0:yyyy_MM_dd}`/`{date}` 展开为 `yyyy_MM_dd`；限制大小时向后寻找第一个未达上限的
/// `_2`、`_3`… 文件（对齐 DH.NCore `GetLogFile`）；候选全部达到上限时返回 `None`（放弃本批写入）。
fn pick_file(shared: &Shared) -> Option<PathBuf> {
    let date = Local::now().format("%Y_%m_%d").to_string();
    let name = expand_file_name(&shared.options.file_format, &date);
    let path = shared.dir.join(&name);

    if shared.options.max_bytes == 0 {
        return Some(path);
    }

    // 找到第一个未达到上限的文件（原名、_2、_3…，最多尝试 1023 个；对齐 DH.NCore）
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) => (stem.to_owned(), format!(".{ext}")),
        None => (name.clone(), String::new()),
    };
    for i in 1..1024u32 {
        let candidate = if i == 1 {
            path.clone()
        } else {
            shared.dir.join(format!("{stem}_{i}{ext}"))
        };
        let Ok(meta) = std::fs::metadata(&candidate) else {
            return Some(candidate); // 文件不存在：直接使用
        };
        if meta.len() < shared.options.max_bytes {
            return Some(candidate);
        }
    }
    None
}

/// 展开文件名格式：`{0:yyyy_MM_dd}`（C# `String.Format` 风格）与 `{date}` 均展开为日期。
fn expand_file_name(format: &str, date: &str) -> String {
    format
        .replace("{0:yyyy_MM_dd}", date)
        .replace("{date}", date)
}

/// 清理超量备份。返回被删除的文件 `(名称, 大小)`，供调用方补提示日志（对齐 DH.NCore）。
///
/// 对齐 DH.NCore：先删 `*.del` 残留，再把最旧的日志文件删到上限；
/// 排序优先使用创建时间（`Metadata::created`，不可用时回退修改时间）。
fn prune_backups(shared: &Shared) -> Vec<(String, u64)> {
    let mut removed = Vec::new();
    if shared.options.backups == 0 {
        return removed;
    }
    let Ok(entries) = std::fs::read_dir(&shared.dir) else {
        return removed;
    };

    let mut logs: Vec<(PathBuf, std::time::SystemTime, u64)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // 清理解除删除失败的历史残留（对齐 DH.NCore：被占用文件改名 .del 供下次清理）
        if name.ends_with(".del") {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        if !name.ends_with(".log") || !path.is_file() {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let time = meta
            .created()
            .or_else(|_| meta.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        logs.push((path, time, meta.len()));
    }

    if logs.len() <= shared.options.backups {
        return removed;
    }
    logs.sort_by_key(|(_, time, _)| *time);
    let remove_count = logs.len() - shared.options.backups;
    for (path, _, size) in logs.into_iter().take(remove_count) {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if std::fs::remove_file(&path).is_err() {
            // 删除失败（文件被占用）：改名 .del 供下次清理（对齐 DH.NCore）
            let _ = std::fs::rename(&path, path.with_extension("log.del"));
        }
        removed.push((name, size));
    }

    removed
}

/// 进程/环境信息日志头（对齐 DH.NCore `Logger.GetHead`；每进程首次写入时输出一次）。
///
/// 与 C# 的已知近似：`#Software/#AppDomain` 取程序名（C# 取程序集标题/AppDomain 名）；
/// `#CLR` 固定为 `Rust`（无 CLR 概念，保留字段名）；`#GC/#ThreadPool/#Memory` 等
/// 运行时专属信息不输出（等价 C# 未注册 MachineInfo 的场景）。
fn process_head() -> String {
    let mut head = String::new();

    let exe = std::env::current_exe().ok();
    let exe_path = exe
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_default();
    let software = exe
        .as_ref()
        .and_then(|path| path.file_stem())
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();

    let _ = write!(head, "#Software: {software}\r\n");
    let _ = write!(head, "#ProcessID: {}", std::process::id());
    if cfg!(target_pointer_width = "64") {
        let _ = write!(head, " x64");
    }
    head.push_str("\r\n");
    // Rust 无 AppDomain，用程序名近似（C# 的 FriendlyName 通常即程序集名）
    let _ = write!(head, "#AppDomain: {software}\r\n");
    if !exe_path.is_empty() {
        let _ = write!(head, "#FileName: {exe_path}\r\n");
    }

    let base_dir = exe
        .as_ref()
        .and_then(|path| path.parent())
        .map(with_trailing_sep);
    if let Some(base_dir) = &base_dir {
        let _ = write!(head, "#BaseDirectory: {base_dir}\r\n");
    }
    if let Ok(cwd) = std::env::current_dir() {
        let cwd_text = cwd.display().to_string();
        // 对齐 C#：当前目录与基准目录一致时不输出
        let same = base_dir
            .as_deref()
            .map(|base| normalize_dir(base) == normalize_dir(&cwd_text))
            .unwrap_or(false);
        if !same {
            let _ = write!(head, "#CurrentDirectory: {cwd_text}\r\n");
        }
    }
    let _ = write!(
        head,
        "#TempPath: {}\r\n",
        with_trailing_sep(&std::env::temp_dir())
    );

    let args: Vec<String> = std::env::args().collect();
    if !args.is_empty() {
        // 对齐 C# Environment.CommandLine：程序路径含空格时加引号
        let mut parts = Vec::with_capacity(args.len());
        for (i, arg) in args.iter().enumerate() {
            if i == 0 && arg.contains(' ') {
                parts.push(format!("\"{arg}\""));
            } else {
                parts.push(arg.clone());
            }
        }
        let _ = write!(head, "#CommandLine: {}\r\n", parts.join(" "));
    }

    let _ = write!(head, "#ApplicationType: {}\r\n", application_type());
    let _ = write!(head, "#CLR: Rust\r\n");
    let _ = write!(
        head,
        "#OS: {}, {}/{}\r\n",
        os_description(),
        machine_name(),
        user_name()
    );
    let cpu = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let _ = write!(head, "#CPU: {cpu}\r\n");
    if let Some(started) = system_started() {
        let _ = write!(head, "#SystemStarted: {started}\r\n");
    }
    let _ = write!(head, "#Date: {}\r\n", Local::now().format("%Y-%m-%d"));
    let _ = write!(head, "#详解：https://newlifex.com/core/log\r\n");
    let _ = write!(
        head,
        "#字段: 时间 线程ID 线程池Y/网页W/普通N 线程名/任务ID/定时T/线程池P/长任务L 消息内容\r\n"
    );
    let _ = write!(head, "#Fields: Time ThreadId Kind Name Message\r\n");

    head
}

/// 目录文本 + 平台尾分隔符（对齐 C# `AppDomain.BaseDirectory` / `Path.GetTempPath()` 的形态）。
fn with_trailing_sep(dir: &Path) -> String {
    let mut text = dir.display().to_string();
    let sep = std::path::MAIN_SEPARATOR;
    if !text.ends_with(sep) {
        text.push(sep);
    }
    text
}

/// 目录归一化（忽略大小写与尾分隔符；用于 `#CurrentDirectory` 是否输出）。
fn normalize_dir(text: &str) -> String {
    text.trim_end_matches(['/', '\\']).to_ascii_lowercase()
}

/// 应用类型（对齐 C# `#ApplicationType`）：任一标准流是终端视为控制台，否则按服务处理。
fn application_type() -> &'static str {
    use std::io::IsTerminal;

    if std::io::stdout().is_terminal()
        || std::io::stderr().is_terminal()
        || std::io::stdin().is_terminal()
    {
        "Console"
    } else {
        "Service"
    }
}

/// 操作系统描述（对齐 C# `#OS` 第一段）\uff1aLinux 取发行版名称与版本）。
#[cfg(target_os = "linux")]
fn os_description() -> String {
    if let Ok(text) = std::fs::read_to_string("/etc/os-release") {
        for line in text.lines() {
            if let Some(value) = line.strip_prefix("PRETTY_NAME=") {
                return value.trim().trim_matches('"').to_string();
            }
        }
    }
    "Linux".to_string()
}

/// 操作系统描述（对齐 C# `#OS` 第一段\uff1a`Microsoft Windows NT {主}.{次}.{构建}.0`）。
#[cfg(windows)]
fn os_description() -> String {
    windows_version_description().unwrap_or_else(|| {
        std::env::var("OS")
            .ok()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| "Windows".to_string())
    })
}

/// 操作系统描述（对齐 C# `#OS` 第一段）。
#[cfg(target_os = "macos")]
fn os_description() -> String {
    "macOS".to_string()
}

/// 操作系统描述（其它平台）。
#[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
fn os_description() -> String {
    std::env::consts::OS.to_string()
}

/// Windows 版本描述（对齐 C# `RuntimeInformation.OSDescription`\uff1a`Microsoft Windows NT 10.0.26200.0`）。
///
/// 用 `RtlGetVersion` 取真实版本（不受兼容性清单影响的 API）。
#[cfg(windows)]
fn windows_version_description() -> Option<String> {
    use windows_sys::Win32::System::SystemInformation::OSVERSIONINFOW;

    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn RtlGetVersion(version_info: *mut OSVERSIONINFOW) -> i32;
    }

    let mut info: OSVERSIONINFOW = unsafe { std::mem::zeroed() };
    info.dwOSVersionInfoSize = std::mem::size_of::<OSVERSIONINFOW>() as u32;
    // 返回 0（STATUS_SUCCESS）时版本字段有效
    let status = unsafe { RtlGetVersion(&mut info) };
    if status != 0 {
        return None;
    }
    Some(format!(
        "Microsoft Windows NT {}.{}.{}.0",
        info.dwMajorVersion, info.dwMinorVersion, info.dwBuildNumber
    ))
}

/// `RtlGetVersion` 输出形状校验（本机 Windows 10/11 均应报 `Microsoft Windows NT 10.x.y.0`）。
#[cfg(all(test, windows))]
mod windows_desc_tests {
    use super::*;

    #[test]
    fn windows_version_description_shape() {
        let desc = windows_version_description().expect("RtlGetVersion 应可用");
        assert!(desc.starts_with("Microsoft Windows NT 10."), "{desc}");
        assert_eq!(desc.matches('.').count(), 3, "{desc}");
    }
}

/// 机器名（对齐 C# `#OS` 第二段前半；取值链下沉 `sys::machine`，含内核主机名优先）。
fn machine_name() -> String {
    crate::sys::machine::server_name("")
}

/// 用户名（对齐 C# `#OS` 第二段后半）。
fn user_name() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_default()
}

/// 系统启动至今的时长文本（对齐 C# `#SystemStarted: {TimeSpan}`，如 `4.20:09:04.8750000`）。
///
/// Linux 读 `/proc/uptime`，Windows 调 `GetTickCount64`（对齐 `Environment.TickCount64`）；
/// macOS 暂缺实现（不输出该行）。
fn system_started() -> Option<String> {
    let millis = system_uptime_millis()?;
    let days = millis / 86_400_000;
    let rem = millis % 86_400_000;
    let hh = rem / 3_600_000;
    let mm = (rem % 3_600_000) / 60_000;
    let ss = (rem % 60_000) / 1000;
    let ms = rem % 1000;

    // TimeSpan.ToString()：天数为 0 时省略 `d.` 段；小数秒固定 7 位
    if days > 0 {
        Some(format!("{days}.{hh:02}:{mm:02}:{ss:02}.{ms:03}0000"))
    } else {
        Some(format!("{hh:02}:{mm:02}:{ss:02}.{ms:03}0000"))
    }
}

/// 系统启动至今的毫秒数。
#[cfg(target_os = "linux")]
fn system_uptime_millis() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/uptime").ok()?;
    let secs: f64 = text.split_whitespace().next()?.parse().ok()?;
    Some((secs * 1000.0) as u64)
}

/// 系统启动至今的毫秒数。
#[cfg(windows)]
fn system_uptime_millis() -> Option<u64> {
    // GetTickCount64：系统启动以来的毫秒数（对齐 C# Environment.TickCount64）
    let millis = unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount64() };
    Some(millis)
}

/// 系统启动至今的毫秒数（其它平台暂缺）。
#[cfg(not(any(target_os = "linux", windows)))]
fn system_uptime_millis() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// 独立临时目录（避免用例相互干扰）
    fn temp_dir(tag: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("dhrust-log-{tag}-{stamp}"))
    }

    fn read_file(path: &Path) -> String {
        let mut text = String::new();
        std::fs::File::open(path)
            .expect("日志文件应存在")
            .read_to_string(&mut text)
            .unwrap();
        text
    }

    #[test]
    fn writes_line_with_head_and_filters_level() {
        let dir = temp_dir("basic");
        let log = TextFileLog::create_with(
            &dir,
            FileLogOptions {
                max_bytes: 0,
                backups: 0,
                file_format: "{0:yyyy_MM_dd}.log".to_owned(),
            },
        );

        log.set_level(LogLevel::Info);
        log.debug("调试不应出现");
        log.info("你好 hello");
        log.flush();

        let file = dir.join(format!("{}.log", Local::now().format("%Y_%m_%d")));
        let text = read_file(&file);

        // 日志头字段与列序对齐 DH.NCore GetHead
        for field in [
            "#Software: ",
            "#ProcessID: ",
            "#FileName: ",
            "#TempPath: ",
            "#CommandLine: ",
            "#ApplicationType: ",
            "#CLR: ",
            "#OS: ",
            "#CPU: ",
            "#Date: ",
            "#Fields: Time ThreadId Kind Name Message",
        ] {
            assert!(text.contains(field), "日志头应含 {field}: {text}");
        }

        // 行格式：HH:mm:ss.fff 线程ID 类型 名称 正文（对齐 DH.NCore 默认 LogLineFormat）
        let line = text
            .lines()
            .find(|line| line.ends_with(" 你好 hello"))
            .expect("应含日志行");
        let parts: Vec<&str> = line.split(' ').collect();
        assert!(parts.len() >= 5, "列数不足: {line}");
        assert_eq!(parts[0].len(), 12, "时间列应形如 HH:mm:ss.fff: {line}");
        assert_eq!(parts[1].len(), 2, "线程ID列应为两位: {line}");
        assert_eq!(parts[2], "N", "测试线程应为普通线程: {line}");
        assert!(!text.contains("[INFO]"), "行格式不应包含级别标记: {text}");
        assert!(!text.contains("调试不应出现"), "低于级别的日志不应出现: {text}");
    }

    #[test]
    fn splits_by_size_and_prunes_backups() {
        let dir = temp_dir("split");
        let log = TextFileLog::create_with(
            &dir,
            FileLogOptions {
                max_bytes: 1,
                backups: 1,
                file_format: "{date}.log".to_owned(),
            },
        );

        log.info("第一行");
        log.flush();
        log.info("第二行");
        log.flush();
        log.info("第三行");
        log.flush();

        let date = Local::now().format("%Y_%m_%d").to_string();
        let first = dir.join(format!("{date}.log"));
        let second = dir.join(format!("{date}_2.log"));
        let third = dir.join(format!("{date}_3.log"));
        assert!(
            third.exists(),
            "第三条应拆分到 _3 文件（对齐 DH.NCore：满后 _2、_3…）"
        );
        // backups = 1：除最新文件外应已被清理
        assert!(second.exists(), "_2 文件应保留（最新备份）");
        assert!(!first.exists(), "最旧文件应被清理");
    }

    /// 文件名格式支持 C# 风格 `{0:yyyy_MM_dd}` 与 `{date}` 占位符
    #[test]
    fn file_name_format_placeholders() {
        let date = "2026_10_01";
        assert_eq!(
            expand_file_name("{0:yyyy_MM_dd}.log", date),
            "2026_10_01.log"
        );
        assert_eq!(expand_file_name("{date}.log", date), "2026_10_01.log");
    }

    /// 数字千分位（对齐 C# 格式串 `{2:n0}`）
    #[test]
    fn thousands_separator() {
        assert_eq!(format_thousands(0), "0");
        assert_eq!(format_thousands(999), "999");
        assert_eq!(format_thousands(1_000), "1,000");
        assert_eq!(format_thousands(12_345_678), "12,345,678");
    }

    /// 全部候选文件达到上限时放弃写入（对齐 DH.NCore `GetLogFile` 返回 null）
    #[test]
    fn gives_up_when_all_candidates_full() {
        let dir = temp_dir("full");
        let log = TextFileLog::create_with(
            &dir,
            FileLogOptions {
                max_bytes: 1,
                backups: 0,
                file_format: "{date}.log".to_owned(),
            },
        );

        // 造满 1023 个候选文件（原名 + `_2`..`_1023`），每个均超过 1 字节
        let date = Local::now().format("%Y_%m_%d").to_string();
        std::fs::create_dir_all(&dir).unwrap();
        for i in 1..1024u32 {
            let name = if i == 1 {
                format!("{date}.log")
            } else {
                format!("{date}_{i}.log")
            };
            std::fs::write(dir.join(name), "xx").unwrap();
        }
        assert!(pick_file(&log.shared).is_none(), "候选全满应放弃写入");

        // 腾空一个候选后应能继续写入（首个未满文件）
        std::fs::write(dir.join(format!("{date}_2.log")), "").unwrap();
        assert_eq!(
            pick_file(&log.shared),
            Some(dir.join(format!("{date}_2.log")))
        );
    }
}
