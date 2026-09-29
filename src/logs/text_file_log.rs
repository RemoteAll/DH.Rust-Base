//! 文本文件日志（对应 DH.NCore `TextFileLog`）。
//!
//! - 异步写入：日志先入队，由专用写线程批量落盘（调用方不阻塞在磁盘 IO 上）；
//! - 滚动：按天一个文件（默认 `yyyy_MM_dd.log`），单文件超过 10MB 拆分为 `_1`、`_2`…；
//! - 备份：目录内日志文件超过上限后删除最旧的（默认保留 100 份）；
//! - 日志头：每个进程首次写入时输出进程/环境信息（对齐 DH.NCore `GetHead`）；
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

/// 文件日志选项
#[derive(Debug, Clone)]
pub struct FileLogOptions {
    /// 单文件大小上限（字节；超过后拆分 `_1`、`_2`…；0 不限制）。默认 10MB（对齐 DH.NCore）
    pub max_bytes: u64,
    /// 备份个数上限（目录内日志文件超过后删除最旧的；0 不限制）。默认 100
    pub backups: usize,
    /// 文件名格式（`{date}` 占位符 = `yyyy_MM_dd`）。默认 `{date}.log`
    pub file_format: String,
}

impl Default for FileLogOptions {
    fn default() -> Self {
        Self {
            max_bytes: 10 * 1024 * 1024,
            backups: 100,
            file_format: "{date}.log".to_owned(),
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
        if self.tx.send(Msg::Line(format_line(level, message))).is_ok() {
            self.shared.queued.fetch_add(1, Ordering::Relaxed);
        }
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
            Err(RecvTimeoutError::Timeout) => close_file(shared),
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

/// 写入一批日志（确定目标文件；切换文件时清理超量备份）
fn write_batch(shared: &Shared, writer: &mut WriterState, batch: &[String]) {
    let target = pick_file(shared);

    let mut file_guard = shared
        .file
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if writer.current.as_deref() != Some(target.as_path()) {
        // 切换文件：关闭旧句柄、重置失败计数、清理超量备份
        *file_guard = None;
        writer.current = Some(target.clone());
        shared.open_errors.store(0, Ordering::Relaxed);
        prune_backups(shared);
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
                    // 追加到已有内容的文件时，先空一行分隔（对齐 DH.NCore）
                    if file.metadata().map(|meta| meta.len() > 10).unwrap_or(false) {
                        let _ = file.write_all(b"\r\n");
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
            let _ = file.write_all(b"\n");
        }
    }
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
/// `{date}` 展开为 `yyyy_MM_dd`；限制大小时向后寻找第一个未达上限的 `_N` 文件
/// （对齐 DH.NCore `GetLogFile`）。
fn pick_file(shared: &Shared) -> PathBuf {
    let date = Local::now().format("%Y_%m_%d").to_string();
    let name = shared.options.file_format.replace("{date}", &date);
    let path = shared.dir.join(&name);

    if shared.options.max_bytes == 0 {
        return path;
    }

    // 找到第一个未达到大小上限的文件（原名、_1、_2…，最多尝试 1024 个）
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) => (stem.to_owned(), format!(".{ext}")),
        None => (name.clone(), String::new()),
    };
    for i in 0..1024u32 {
        let candidate = if i == 0 {
            path.clone()
        } else {
            shared.dir.join(format!("{stem}_{i}{ext}"))
        };
        let Ok(meta) = std::fs::metadata(&candidate) else {
            return candidate; // 文件不存在：直接使用
        };
        if meta.len() < shared.options.max_bytes {
            return candidate;
        }
    }
    path
}

/// 清理超量备份（对齐 DH.NCore：先删 `*.del` 残留，再把最旧的日志文件删到上限）
fn prune_backups(shared: &Shared) {
    if shared.options.backups == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(&shared.dir) else {
        return;
    };

    let mut logs: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
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
        let modified = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        logs.push((path, modified));
    }

    if logs.len() <= shared.options.backups {
        return;
    }
    logs.sort_by_key(|(_, time)| *time);
    let remove_count = logs.len() - shared.options.backups;
    for (path, _) in logs.into_iter().take(remove_count) {
        if std::fs::remove_file(&path).is_err() {
            // 删除失败（文件被占用）：改名 .del 供下次清理（对齐 DH.NCore）
            let _ = std::fs::rename(&path, path.with_extension("log.del"));
        }
    }
}

/// 进程/环境信息日志头（对齐 DH.NCore `GetHead`；每进程首次写入时输出一次）
fn process_head() -> String {
    let mut head = String::new();

    let exe = std::env::current_exe().ok();
    let name = exe
        .as_ref()
        .and_then(|path| path.file_stem())
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    let _ = write!(head, "#Software: {name}\r\n");
    let _ = write!(head, "#ProcessID: {}", std::process::id());
    if cfg!(target_pointer_width = "64") {
        let _ = write!(head, " x64");
    }
    head.push_str("\r\n");
    if let Some(exe) = &exe {
        let _ = write!(head, "#FileName: {}\r\n", exe.display());
        if let Some(dir) = exe.parent() {
            let _ = write!(head, "#BaseDirectory: {}\r\n", dir.display());
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        let _ = write!(head, "#CurrentDirectory: {}\r\n", cwd.display());
    }
    let _ = write!(head, "#TempPath: {}\r\n", std::env::temp_dir().display());
    let args: Vec<String> = std::env::args().collect();
    if !args.is_empty() {
        let _ = write!(head, "#CommandLine: {}\r\n", args.join(" "));
    }
    let _ = write!(
        head,
        "#OS: {}, {}\r\n",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    let cpu = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let _ = write!(head, "#CPU: {cpu}\r\n");
    let _ = write!(
        head,
        "#Time: {}\r\n",
        crate::times::format_datetime_ms(&Local::now().naive_local())
    );
    head
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
                file_format: "{date}.log".to_owned(),
            },
        );

        log.set_level(LogLevel::Info);
        log.debug("调试不应出现");
        log.info("你好 hello");
        log.flush();

        let file = dir.join(format!("{}.log", Local::now().format("%Y_%m_%d")));
        let text = read_file(&file);
        assert!(text.contains("#Software: "), "应写日志头: {text}");
        assert!(text.contains("#ProcessID: "), "日志头应含进程号: {text}");
        assert!(text.contains("[INFO] 你好 hello"), "应含日志行: {text}");
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
        let second = dir.join(format!("{date}_1.log"));
        let third = dir.join(format!("{date}_2.log"));
        assert!(third.exists(), "第三条应拆分到 _2 文件");
        // backups = 1：除最新文件外应已被清理
        assert!(second.exists(), "_1 文件应保留（最新备份）");
        assert!(!first.exists(), "最旧文件应被清理");
    }
}
