//! 键控锁（`KeyedLock`）：**主后端跨进程互斥 + 兜底后端 + 失败自动降级 + 恢复通知**。
//!
//! 典型场景：多实例部署时「同一业务对象同一时刻只让一台机器干活」（取令牌、拉数据、重建索引…）。
//!
//! 分工：
//! - [`LockBackend`]：后端能力接口（**谁有能力谁实现**）——公共库只提供进程内
//!   [`MemoryLock`]；Redis / etcd / 数据库等由使用方实现（公共库不绑具体驱动）。
//! - [`KeyedLock`]：**策略层**（本模块的核心价值）：
//!   - 主后端可用 → 用它，拿不到就返回 `None`（不等待，交给调用方决定等/跳过）；
//!   - 主后端**报错**（连不上/超时）→ 立即降级到兜底后端并在 `retry_after` 后自动重试，
//!     期间**不阻断业务**（最坏结果只是"多干一次"）；
//!   - 降级与恢复各发一次 [`LockEvent`]（调用方可接告警；未接时只记日志），
//!     避免依赖抖动时刷屏。
//!
//! 语义约定：
//! - **非阻塞**：`try_acquire` 立即返回；要等待请自己循环（见「等锁 + 复用结果」的典型用法）。
//! - **按 key 加锁**：不同 key 互不影响（锁粒度最小）。
//! - **TTL 兜底**：持锁进程崩溃时锁会自动过期（进程内后端与实现方后端都要遵守自己的 TTL）。
//! - **释放**：`LockGuard` 一 drop 就释放（实现方的句柄在 drop 时按令牌比对删除）。
//!
//! 用法：
//! ```no_run
//! use dhrust::lock::{KeyedLock, MemoryLock};
//! use std::time::Duration;
//!
//! // 单实例：只给兜底后端
//! let lock = KeyedLock::new(None, Box::new(MemoryLock::new(Duration::from_secs(30))),
//!     Duration::from_secs(30), "进程内", None);
//! if let Some(_guard) = lock.try_acquire("task-1") {
//!     // 干活；离开作用域自动释放
//! }
//! ```

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 锁持有句柄（**空标记 trait**：实现者的 `Drop` 负责释放）。
///
/// 约定：`Drop` 必须是「按令牌比对删除」，不能无条件删除——否则会误删他人的锁。
pub trait LockHold: Send + Sync {}

/// 后端能力接口（Redis / etcd / 数据库等由使用方实现）。
pub trait LockBackend: Send + Sync {
    /// 后端名（日志 / 面板展示用，如 `Redis(127.0.0.1:6379/2)`）。
    fn name(&self) -> String;

    /// 是否跨进程/跨机器生效（进程内后端返回 `false`）。
    fn distributed(&self) -> bool;

    /// 非阻塞加锁：`Ok(Some(hold))` 成功、`Ok(None)` 被他人持有、`Err` 后端故障。
    fn try_acquire(&self, key: &str) -> Result<Option<Box<dyn LockHold>>, String>;
}

/// 降级 / 恢复事件（调用方可接告警；未接时 [`KeyedLock`] 只记日志）。
#[derive(Clone, Debug)]
pub enum LockEvent {
    /// 主后端不可用，已降级到兜底后端。
    Degraded {
        /// 主后端名
        backend: String,
        /// 失败原因
        reason: String,
        /// 多久后自动重试
        retry_in: Duration,
    },
    /// 主后端恢复（重新拿回跨进程互斥）。
    Recovered {
        /// 主后端名
        backend: String,
    },
}

/// 事件接收器。
pub type EventSink = Arc<dyn Fn(LockEvent) + Send + Sync>;

/// 进程内锁后端：`HashMap` + TTL（单实例部署 / 主后端不可用时的兜底）。
///
/// 语义与跨进程后端一致（加锁即写入过期时刻，释放即删除）；进程崩溃后残留条目由
/// 下次加锁时的过期清理吸收。
pub struct MemoryLock {
    held: Arc<Mutex<HashMap<String, Instant>>>,
    ttl: Duration,
}

impl MemoryLock {
    /// 创建（`ttl` 为锁有效期，应大于一次业务耗时）。
    pub fn new(ttl: Duration) -> MemoryLock {
        MemoryLock {
            held: Arc::new(Mutex::new(HashMap::new())),
            ttl,
        }
    }

    /// 有效期。
    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// 当前持有中的键数量（排障/测试用）。
    pub fn len(&self) -> usize {
        lock_plain(&self.held).len()
    }

    /// 是否一把锁都没持有。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl LockBackend for MemoryLock {
    fn name(&self) -> String {
        "进程内（MemoryLock）".to_string()
    }

    fn distributed(&self) -> bool {
        false
    }

    fn try_acquire(&self, key: &str) -> Result<Option<Box<dyn LockHold>>, String> {
        let now = Instant::now();
        let mut held = lock_plain(&self.held);
        // 顺便清掉已超期的条目（键数量 = 业务对象数，开销可忽略）
        held.retain(|_, expire| *expire > now);
        if held.contains_key(key) {
            return Ok(None);
        }
        held.insert(key.to_string(), now + self.ttl);
        Ok(Some(Box::new(MemoryHold {
            held: self.held.clone(),
            key: key.to_string(),
        })))
    }
}

/// 进程内锁句柄（`Drop` 释放）。
struct MemoryHold {
    held: Arc<Mutex<HashMap<String, Instant>>>,
    key: String,
}

impl Drop for MemoryHold {
    fn drop(&mut self) {
        lock_plain(&self.held).remove(&self.key);
    }
}

impl LockHold for MemoryHold {}

/// 锁句柄（`Drop` 即释放，由后端实现决定具体释放方式）。
pub struct LockGuard {
    /// 字段不读：靠实现方的 `Drop` 释放
    #[allow(dead_code)]
    hold: Box<dyn LockHold>,
}

/// 降级状态。
struct DegradeState {
    /// 降级开始时刻（`None` = 未降级）
    since: Option<Instant>,
    /// 降级原因
    reason: String,
    /// 是否已发过「降级」事件（恢复后复位）
    notified: bool,
}

impl DegradeState {
    fn fresh() -> DegradeState {
        DegradeState {
            since: None,
            reason: String::new(),
            notified: false,
        }
    }
}

/// 键控锁策略层（主后端 + 兜底后端 + 自动降级/恢复）。
pub struct KeyedLock {
    /// 主后端（`None` = 只用兜底，即单实例语义）
    primary: Option<Box<dyn LockBackend>>,
    /// 兜底后端（必须是进程内语义）
    fallback: Box<dyn LockBackend>,
    /// 主后端故障后暂停尝试的时长（过后自动重试）
    retry_after: Duration,
    /// 展示名（`describe` 输出）
    label: String,
    state: Mutex<DegradeState>,
    sink: Option<EventSink>,
}

impl KeyedLock {
    /// 创建。
    /// <param name="primary">主后端（`None` = 进程内单实例）</param>
    /// <param name="fallback">兜底后端（通常 `MemoryLock`）</param>
    /// <param name="retry_after">主后端故障后多久自动重试</param>
    /// <param name="label">展示名（日志/面板；建议带上连接与前缀信息）</param>
    /// <param name="sink">事件接收器（`None` = 只记日志）</param>
    pub fn new(
        primary: Option<Box<dyn LockBackend>>,
        fallback: Box<dyn LockBackend>,
        retry_after: Duration,
        label: impl Into<String>,
        sink: Option<EventSink>,
    ) -> Arc<KeyedLock> {
        Arc::new(KeyedLock {
            primary,
            fallback,
            retry_after,
            label: label.into(),
            state: Mutex::new(DegradeState::fresh()),
            sink,
        })
    }

    /// 主动标记为降级（例如启动时就发现主后端连不上——不必等第一次加锁失败）。
    pub fn demote(&self, reason: impl Into<String>) {
        let reason = reason.into();
        let mut state = lock_plain(&self.state);
        if state.since.is_some() {
            state.reason = reason;
            return;
        }
        state.since = Some(Instant::now());
        state.reason = reason.clone();
        drop(state);
        self.emit_degraded(&reason);
    }

    /// 是否跨进程生效（降级期间为 `false`）。
    pub fn is_distributed(&self) -> bool {
        match &self.primary {
            Some(backend) => backend.distributed() && !self.degraded(),
            None => false,
        }
    }

    /// 当前是否处于降级（主后端不可用，走兜底）。
    pub fn degraded(&self) -> bool {
        let state = lock_plain(&self.state);
        state.since.is_some()
    }

    /// 降级原因（未降级为空）。
    pub fn degraded_reason(&self) -> String {
        lock_plain(&self.state).reason.clone()
    }

    /// 展示名（降级时附原因，面板/`-status` 一眼能看出当前是哪种后端）。
    pub fn describe(&self) -> String {
        let state = lock_plain(&self.state);
        match (state.since, self.primary.is_some()) {
            (Some(_), _) => format!("{}；已降级为进程内：{}", self.label, state.reason),
            (None, true) => format!("{}（跨实例）", self.label),
            (None, false) => self.label.clone(),
        }
    }

    /// 非阻塞加锁：拿到返回句柄（`Drop` 释放），被他人持有返回 `None`。
    ///
    /// 主后端报错时**降级到兜底后端继续尝试**（不返回错误，不让业务失败）。
    pub fn try_acquire(&self, key: &str) -> Option<LockGuard> {
        let Some(primary) = &self.primary else {
            return self.acquire_fallback(key);
        };
        if !self.should_try_primary() {
            return self.acquire_fallback(key);
        }
        match primary.try_acquire(key) {
            Ok(Some(hold)) => {
                self.mark_recovered();
                return Some(LockGuard { hold });
            }
            Ok(None) => return None,
            Err(e) => {
                self.demote_runtime(&e);
            }
        }
        self.acquire_fallback(key)
    }

    fn acquire_fallback(&self, key: &str) -> Option<LockGuard> {
        match self.fallback.try_acquire(key) {
            Ok(hold) => hold.map(|hold| LockGuard { hold }),
            Err(e) => {
                // 兜底是进程内实现，正常不会失败；一旦失败只能放行（宁可重复干活也不能卡死业务）
                crate::warn!("进程内兜底锁不可用（{e}），本次不加锁执行：{key}");
                None
            }
        }
    }

    /// 是否该尝试主后端（降级窗口过后自动重试）。
    fn should_try_primary(&self) -> bool {
        let mut state = lock_plain(&self.state);
        let Some(since) = state.since else {
            return true;
        };
        if since.elapsed() >= self.retry_after {
            // 窗口到期：清状态，允许再试（真正成功后才发「恢复」事件）
            state.since = None;
            state.reason.clear();
            return true;
        }
        false
    }

    fn mark_recovered(&self) {
        let mut state = lock_plain(&self.state);
        if state.notified {
            state.notified = false;
            drop(state);
            let backend = self
                .primary
                .as_ref()
                .map(|b| b.name())
                .unwrap_or_default();
            crate::info!("锁后端已恢复（{backend}）");
            if let Some(sink) = &self.sink {
                sink(LockEvent::Recovered { backend });
            }
        }
    }

    fn demote_runtime(&self, reason: &str) {
        let mut state = lock_plain(&self.state);
        state.since = Some(Instant::now());
        state.reason = reason.to_string();
        drop(state);
        self.emit_degraded(reason);
    }

    fn emit_degraded(&self, reason: &str) {
        let first = {
            let mut state = lock_plain(&self.state);
            let first = !state.notified;
            state.notified = true;
            first
        };
        if !first {
            return;
        }
        let backend = self
            .primary
            .as_ref()
            .map(|b| b.name())
            .unwrap_or_else(|| self.label.clone());
        crate::warn!(
            "锁后端不可用，{} 秒内降级为进程内锁（{backend}）：{reason}",
            self.retry_after.as_secs()
        );
        if let Some(sink) = &self.sink {
            sink(LockEvent::Degraded {
                backend,
                reason: reason.to_string(),
                retry_in: self.retry_after,
            });
        }
    }
}

fn lock_plain<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match m.lock() {
        Ok(v) => v,
        Err(e) => e.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// 可控的假后端：`fail=true` 时返回错误，`hold=true` 时返回「被占用」。
    struct FakeBackend {
        fail: AtomicBool,
        busy: AtomicBool,
        acquires: AtomicUsize,
    }

    impl FakeBackend {
        fn new() -> Arc<FakeBackend> {
            Arc::new(FakeBackend {
                fail: AtomicBool::new(false),
                busy: AtomicBool::new(false),
                acquires: AtomicUsize::new(0),
            })
        }
    }

    struct NoopHold;
    impl LockHold for NoopHold {}

    impl LockBackend for Arc<FakeBackend> {
        fn name(&self) -> String {
            "Fake".to_string()
        }
        fn distributed(&self) -> bool {
            true
        }
        fn try_acquire(&self, _key: &str) -> Result<Option<Box<dyn LockHold>>, String> {
            self.acquires.fetch_add(1, Ordering::SeqCst);
            if self.fail.load(Ordering::SeqCst) {
                return Err("连接超时".to_string());
            }
            if self.busy.load(Ordering::SeqCst) {
                return Ok(None);
            }
            Ok(Some(Box::new(NoopHold)))
        }
    }

    fn sink_collector() -> (EventSink, Arc<Mutex<Vec<LockEvent>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let e = events.clone();
        let sink: EventSink = Arc::new(move |ev| lock_plain(&e).push(ev));
        (sink, events)
    }

    #[test]
    fn memory_lock_is_mutually_exclusive_and_releases_on_drop() {
        let lock = KeyedLock::new(
            None,
            Box::new(MemoryLock::new(Duration::from_secs(30))),
            Duration::from_secs(1),
            "进程内",
            None,
        );
        assert!(!lock.is_distributed());

        let first = lock.try_acquire("k1").expect("首次应加锁成功");
        assert!(lock.try_acquire("k1").is_none(), "同一键不可重复加锁");
        assert!(lock.try_acquire("k2").is_some(), "不同键互不影响");
        drop(first);
        assert!(lock.try_acquire("k1").is_some(), "释放后应可再次加锁");
    }

    #[test]
    fn memory_lock_expires_after_ttl() {
        let lock = KeyedLock::new(
            None,
            Box::new(MemoryLock::new(Duration::from_millis(80))),
            Duration::from_secs(1),
            "进程内",
            None,
        );
        let _held = lock.try_acquire("k").expect("首次应成功");
        assert!(lock.try_acquire("k").is_none());
        std::thread::sleep(Duration::from_millis(120));
        assert!(lock.try_acquire("k").is_some(), "超过 TTL 的残留锁应被自动清理");
    }

    #[test]
    fn primary_failure_degrades_once_then_recovers() {
        let fake = FakeBackend::new();
        let (sink, events) = sink_collector();
        let lock = KeyedLock::new(
            Some(Box::new(fake.clone())),
            Box::new(MemoryLock::new(Duration::from_secs(30))),
            Duration::from_millis(60),
            "Redis",
            Some(sink),
        );

        // 主后端故障 → 降级到兜底，依然能加锁（业务不失败）
        fake.fail.store(true, Ordering::SeqCst);
        let guard = lock.try_acquire("k");
        assert!(guard.is_some(), "主后端故障时必须降级继续工作");
        assert!(lock.degraded());
        assert!(!lock.is_distributed(), "降级期间不算跨实例");
        assert!(lock.describe().contains("已降级"), "{}", lock.describe());
        drop(guard);
        assert_eq!(lock_plain(&events).len(), 1, "降级只通知一次");

        // 窗口内不再尝试主后端（避免每个请求都白等一次超时）
        let before = fake.acquires.load(Ordering::SeqCst);
        let _ = lock.try_acquire("k2");
        assert_eq!(fake.acquires.load(Ordering::SeqCst), before, "降级窗口内不应再打主后端");

        // 主后端恢复 + 窗口到期 → 重新用主后端，并发一次「恢复」事件
        fake.fail.store(false, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(90));
        let guard = lock.try_acquire("k3").expect("恢复后应能加锁");
        assert!(!lock.degraded());
        assert!(lock.is_distributed());
        drop(guard);
        let events = lock_plain(&events);
        assert_eq!(events.len(), 2, "{events:?}");
        assert!(matches!(events[1], LockEvent::Recovered { .. }), "{events:?}");
    }

    #[test]
    fn busy_primary_does_not_fall_back() {
        let fake = FakeBackend::new();
        let lock = KeyedLock::new(
            Some(Box::new(fake.clone())),
            Box::new(MemoryLock::new(Duration::from_secs(30))),
            Duration::from_secs(5),
            "Redis",
            None,
        );
        fake.busy.store(true, Ordering::SeqCst);
        assert!(
            lock.try_acquire("k").is_none(),
            "别的实例持锁时返回 None（不能退到本机兜底，否则多实例互斥失效）"
        );
        assert!(!lock.degraded(), "被占用不是故障，不应降级");
    }

    #[test]
    fn demote_fires_single_event() {
        let (sink, events) = sink_collector();
        let lock = KeyedLock::new(
            Some(Box::new(FakeBackend::new())),
            Box::new(MemoryLock::new(Duration::from_secs(30))),
            Duration::from_secs(5),
            "Redis",
            Some(sink),
        );
        lock.demote("启动时 ping 失败");
        lock.demote("再次失败");
        assert_eq!(lock_plain(&events).len(), 1, "同一轮降级只通知一次");
        assert!(lock.degraded_reason().contains("再次失败"), "原因更新为最新");
        let captured = lock_plain(&events);
        match &captured[0] {
            LockEvent::Degraded { reason, .. } => assert!(reason.contains("ping")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn no_primary_never_reports_distributed() {
        let lock = KeyedLock::new(
            None,
            Box::new(MemoryLock::new(Duration::from_secs(5))),
            Duration::from_secs(5),
            "进程内（Lock.Kind=memory）",
            None,
        );
        assert!(!lock.is_distributed());
        assert_eq!(lock.describe(), "进程内（Lock.Kind=memory）");
        assert!(lock.try_acquire("k").is_some());
    }
}
