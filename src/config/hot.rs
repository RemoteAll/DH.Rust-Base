//! 配置热重载：后台线程盯文件 → 解析 → 原子替换快照 → 回调通知。
//!
//! 各项目此前各写一遍「定时器 + `reload_if_changed` + 比对 + 通知」，
//! 本模块把它收编成通用能力（**格式无关、无运行时依赖**）：
//!
//! ```ignore
//! // 任意格式：解析函数由调用方提供（TOML / JSON / XML / INI 都行）
//! let watcher = ConfigWatcher::start(&path, cfg, |text| GateConfig::load_from_str(text))
//!     .on_change(|old, new| { /* 应用特有的"要不要重连"判断 */ })
//!     .on_error(|message| log::warn!("{message}"));
//!
//! let cfg = watcher.current(); // 高频路径：只加引用计数，零锁竞争、零分配
//! ```
//!
//! 设计要点：
//! - **无变化零开销**：每周期只做一次 `metadata`（[`FileStamp`](super::FileStamp)），不读盘不解析；
//! - **半成品保护**：解析失败保留旧值并触发 `on_error`，版本戳已推进，不会反复告警；
//! - **推拉双模**：`current()` 拉取当前快照；`on_change` 推送变更前后值；
//! - **可停止**：`Drop` 立即唤醒后台线程并等待退出（`recv_timeout` 而非 `sleep`）；
//! - **不依赖 tokio**：`config` 模块保持"无运行时依赖"，异步项目把回调里做的事
//!   投递给自己的通道即可（`UnboundedSender::send` 是非阻塞的）。

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::{Arc, RwLock};
use std::thread::JoinHandle;
use std::time::Duration;

use super::{FileChange, FileWatcher};

/// 默认轮询间隔：一次 `metadata` 的代价可忽略，3 秒足够"改完即生效"。
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(3);

type ChangeHook<T> = Arc<dyn Fn(&T, &T) + Send + Sync>;
type ErrorHook = Arc<dyn Fn(&str) + Send + Sync>;

/// 变更/失败回调（运行时可替换，故用锁包一层；只在真正变更时读）。
struct Hooks<T> {
    on_change: Option<ChangeHook<T>>,
    on_error: Option<ErrorHook>,
}

/// 配置热重载器：持有当前快照，并在文件变化时自动更新。
pub struct ConfigWatcher<T: Send + Sync + 'static> {
    path: PathBuf,
    current: Arc<RwLock<Arc<T>>>,
    hooks: Arc<RwLock<Hooks<T>>>,
    stop: Option<Sender<()>>,
    join: Option<JoinHandle<()>>,
}

impl<T: Send + Sync + 'static> ConfigWatcher<T> {
    /// 启动后台监视（默认 3 秒轮询）。
    ///
    /// <param name="path">配置文件路径</param>
    /// <param name="initial">初始配置（调用方已加载好的值）</param>
    /// <param name="parse">把文件文本解析为配置；返回 `Err` 表示内容不可用（保留旧值）</param>
    pub fn start<P, F>(path: P, initial: T, parse: F) -> Self
    where
        P: Into<PathBuf>,
        F: Fn(&str) -> Result<T, String> + Send + 'static,
    {
        Self::start_with(path, initial, parse, DEFAULT_POLL_INTERVAL)
    }

    /// 启动后台监视（自定义轮询间隔）。
    ///
    /// <param name="path">配置文件路径</param>
    /// <param name="initial">初始配置</param>
    /// <param name="parse">解析函数</param>
    /// <param name="poll_interval">轮询间隔（下限 100ms）</param>
    pub fn start_with<P, F>(path: P, initial: T, parse: F, poll_interval: Duration) -> Self
    where
        P: Into<PathBuf>,
        F: Fn(&str) -> Result<T, String> + Send + 'static,
    {
        let path = path.into();
        let current = Arc::new(RwLock::new(Arc::new(initial)));
        let hooks: Arc<RwLock<Hooks<T>>> = Arc::new(RwLock::new(Hooks {
            on_change: None,
            on_error: None,
        }));
        let interval = poll_interval.max(Duration::from_millis(100));

        let (stop_tx, stop_rx) = channel::<()>();
        let worker_current = current.clone();
        let worker_hooks = hooks.clone();
        // 基线必须**同步**建立：若放到线程里建，线程调度延迟会把「启动后立即改文件」
        // 那次变更当成基线吞掉（并行测试下必现）。
        let mut file_watcher = FileWatcher::new(&path);
        let worker_path = path.clone();
        let join = std::thread::Builder::new()
            .name("cfg-hot-reload".to_string())
            .spawn(move || {
                loop {
                    // 用 recv_timeout 代替 sleep：停止信号可立即唤醒线程
                    match stop_rx.recv_timeout(interval) {
                        Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                        Err(RecvTimeoutError::Timeout) => {}
                    }
                    let text = match file_watcher.poll() {
                        None => continue,
                        Some(FileChange::Changed(text)) => text,
                        Some(FileChange::Removed) => {
                            report_error(
                                &worker_hooks,
                                &format!(
                                    "配置文件被删除（继续使用内存中的配置）：{}",
                                    worker_path.display()
                                ),
                            );
                            continue;
                        }
                        Some(FileChange::Error(e)) => {
                            report_error(
                                &worker_hooks,
                                &format!(
                                    "配置文件读取失败（{e}），保持当前配置：{}",
                                    worker_path.display()
                                ),
                            );
                            continue;
                        }
                    };
                    let parsed = match parse(&text) {
                        Ok(value) => value,
                        Err(e) => {
                            report_error(
                                &worker_hooks,
                                &format!(
                                    "配置文件解析失败（{e}），保持当前配置：{}",
                                    worker_path.display()
                                ),
                            );
                            continue;
                        }
                    };
                    let new_value = Arc::new(parsed);
                    let old_value = {
                        let mut guard = worker_current
                            .write()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        std::mem::replace(&mut *guard, new_value.clone())
                    };
                    let hook = worker_hooks
                        .read()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .on_change
                        .clone();
                    if let Some(hook) = hook {
                        // 回调异常不应杀死监视线程（否则热重载静默失效）
                        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            hook(&old_value, &new_value);
                        }))
                        .is_err()
                        {
                            report_error(
                                &worker_hooks,
                                "配置变更回调发生 panic（已隔离，监视继续）",
                            );
                        }
                    }
                }
            })
            .ok();

        Self {
            path,
            current,
            hooks,
            stop: Some(stop_tx),
            join,
        }
    }

    /// 设置变更回调（参数为变更前的值、变更后的值；快照已先更新）。
    pub fn on_change<F>(self, hook: F) -> Self
    where
        F: Fn(&T, &T) + Send + Sync + 'static,
    {
        self.hooks
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .on_change = Some(Arc::new(hook));
        self
    }

    /// 设置失败回调（读盘失败 / 解析失败 / 文件被删除；此时快照保持旧值）。
    pub fn on_error<F>(self, hook: F) -> Self
    where
        F: Fn(&str) + Send + Sync + 'static,
    {
        self.hooks
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .on_error = Some(Arc::new(hook));
        self
    }

    /// 当前配置快照（只加引用计数；高频路径可放心调用）。
    pub fn current(&self) -> Arc<T> {
        self.current
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// 主动替换快照（应用内改配置后同步内存；不写盘）。
    ///
    /// 与文件变更走同一套回调，便于"面板保存后立即生效"这类场景。
    pub fn publish(&self, value: T) -> Arc<T> {
        let new_value = Arc::new(value);
        let old_value = {
            let mut guard = self
                .current
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            std::mem::replace(&mut *guard, new_value.clone())
        };
        let hook = self
            .hooks
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .on_change
            .clone();
        if let Some(hook) = hook {
            hook(&old_value, &new_value);
        }
        new_value
    }

    /// 被监视的文件路径。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 停止后台监视（幂等；`Drop` 会自动调用）。
    pub fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl<T: Send + Sync + 'static> Drop for ConfigWatcher<T> {
    fn drop(&mut self) {
        self.stop();
    }
}

fn report_error<T>(hooks: &Arc<RwLock<Hooks<T>>>, message: &str) {
    let hook = hooks
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .on_error
        .clone();
    match hook {
        Some(hook) => {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| hook(message))).is_err() {
                // 错误回调自身 panic：无处可报，只能忽略（监视线程继续工作）
            }
        }
        // 调用方未接 on_error 时静默：本模块不依赖日志设施
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Mutex;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dhrust-hot-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 测试用轮询间隔（默认 3 秒对单测太慢）
    const TEST_POLL: Duration = Duration::from_millis(50);

    /// 等待条件成立（轮询，最多 3 秒）——避免用固定 sleep 造成偶发失败。
    fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_secs(3) {
            if cond() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("等待超时：{what}");
    }

    #[derive(Clone, Debug, PartialEq)]
    struct Demo {
        name: String,
        period: u64,
    }

    fn parse_demo(text: &str) -> Result<Demo, String> {
        let mut name = String::new();
        let mut period = 0u64;
        for line in text.lines() {
            let line = line.trim();
            if let Some(v) = line.strip_prefix("name=") {
                name = v.to_string();
            } else if let Some(v) = line.strip_prefix("period=") {
                period = v.parse().map_err(|e| format!("period 非法：{e}"))?;
            }
        }
        if name.is_empty() {
            return Err("缺少 name".to_string());
        }
        Ok(Demo { name, period })
    }

    #[test]
    fn reloads_and_notifies_on_change() {
        let dir = temp_dir("reload");
        let file = dir.join("demo.conf");
        fs::write(&file, "name=a\nperiod=1\n").unwrap();

        let initial = parse_demo(&fs::read_to_string(&file).unwrap()).unwrap();
        let seen: Arc<Mutex<Vec<(Demo, Demo)>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let watcher = ConfigWatcher::start_with(&file, initial.clone(), parse_demo, TEST_POLL)
            .on_change(move |old, new| {
                sink.lock().unwrap().push((old.clone(), new.clone()));
            });
        assert_eq!(*watcher.current(), initial);

        // 内容长度必须不同：版本戳是「长度 + 修改时间」，同长度时依赖 mtime 分辨率
        fs::write(&file, "name=bbb\nperiod=99\n").unwrap();
        wait_until("快照更新", || watcher.current().name == "bbb");
        assert_eq!(watcher.current().period, 99);

        let events = seen.lock().unwrap().clone();
        assert_eq!(events.len(), 1, "应恰好通知一次");
        assert_eq!(events[0].0.name, "a");
        assert_eq!(events[0].1.name, "bbb");

        let _ = fs::remove_dir_all(&dir);
    }

    /// 解析失败必须保留旧值并报错，且不反复刷（版本戳已推进）。
    #[test]
    fn parse_failure_keeps_previous_value() {
        let dir = temp_dir("bad");
        let file = dir.join("demo.conf");
        fs::write(&file, "name=good\nperiod=3\n").unwrap();

        let errors: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = errors.clone();
        let watcher = ConfigWatcher::start_with(
            &file,
            parse_demo(&fs::read_to_string(&file).unwrap()).unwrap(),
            parse_demo,
            TEST_POLL,
        )
        .on_error(move |message| sink.lock().unwrap().push(message.to_string()));

        fs::write(&file, "name=\nperiod=0\n").unwrap(); // 缺 name → 解析失败
        wait_until("错误回调", || !errors.lock().unwrap().is_empty());
        assert_eq!(watcher.current().name, "good", "坏内容不得覆盖旧值");

        // 再等两轮：同一坏文件不应重复告警
        std::thread::sleep(Duration::from_millis(300));
        let count = errors.lock().unwrap().len();
        assert!(count <= 1, "同一坏内容只应报一次，实际 {count} 次");

        let _ = fs::remove_dir_all(&dir);
    }

    /// `publish` 主动替换也应触发回调（面板保存后立即生效）。
    #[test]
    fn publish_replaces_snapshot_and_notifies() {
        let dir = temp_dir("publish");
        let file = dir.join("demo.conf");
        fs::write(&file, "name=a\nperiod=1\n").unwrap();

        let hits = Arc::new(Mutex::new(0usize));
        let sink = hits.clone();
        let watcher = ConfigWatcher::start(
            &file,
            parse_demo(&fs::read_to_string(&file).unwrap()).unwrap(),
            parse_demo,
        )
        .on_change(move |_, _| *sink.lock().unwrap() += 1);

        watcher.publish(Demo {
            name: "mem".to_string(),
            period: 7,
        });
        assert_eq!(watcher.current().name, "mem");
        assert_eq!(*hits.lock().unwrap(), 1);

        let _ = fs::remove_dir_all(&dir);
    }

    /// 停止后线程应退出（不再响应文件变更），且 drop 不 panic。
    #[test]
    fn stop_terminates_worker() {
        let dir = temp_dir("stop");
        let file = dir.join("demo.conf");
        fs::write(&file, "name=a\nperiod=1\n").unwrap();

        let mut watcher = ConfigWatcher::start(
            &file,
            parse_demo(&fs::read_to_string(&file).unwrap()).unwrap(),
            parse_demo,
        );
        watcher.stop();
        fs::write(&file, "name=zzz\nperiod=2\n").unwrap();
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(watcher.current().name, "a", "停止后不应再跟进文件变更");
        watcher.stop(); // 幂等

        let _ = fs::remove_dir_all(&dir);
    }
}
