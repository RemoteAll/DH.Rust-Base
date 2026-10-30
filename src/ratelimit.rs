//! 固定窗口限流（通用计数引擎；进程内存储，可替换为 Redis 等共享存储）。
//!
//! 语义对齐 C# `DH.RateLimter` 的计数器部分：
//! - **每个键独立窗口**：首次命中时开始计时，窗口结束后计数自动归零；
//! - **首次创建后过期时间不再刷新**：窗口内后续命中不会延长窗口（对齐
//!   `CacheRateLimitStore.IncrementAsync` 只在计数 ≤ 3 时补设过期）；
//! - **正好放行 `limit` 次**：第 `limit + 1` 次判定超限；
//! - **存储可替换**：实现 [`CounterStore`] 即可接入 Redis（Pek.RRedis）等共享计数，
//!   便于多实例部署时做全局接口限流。
//!
//! 本模块不依赖 HTTP，任何进程内周期/任务/接口限流都可复用；HTTP 接口级策略
//! （IP / 用户 / Header / Query / Cookie / Form）见 `pek_rwaf::throttle`。
//!
//! ```no_run
//! use std::time::Duration;
//! use dhrust::ratelimit::WindowLimiter;
//!
//! let limiter = WindowLimiter::new();
//! let d = limiter.hit("api:login:1.2.3.4", 5, Duration::from_secs(60));
//! if !d.allowed {
//!     // 返回 429；d.reset_in 可直接用于 Retry-After
//! }
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 计数存储抽象（原子递增 + 首次创建过期）。
///
/// 共享存储（如 Redis）实现建议：`INCR key`，返回值 ≤ 3 时补 `PEXPIRE key window`
/// （与 C# `CacheRateLimitStore` 的“前 3 次兜底设过期”口径一致），再用 `PTTL` 作为
/// [`CounterHit::reset_in`]；这样多实例部署时也能全局限流。
pub trait CounterStore: Send + Sync {
    /// 原子递增并返回当前窗口计数与剩余时间。
    ///
    /// `window` 只在键首次创建（或上一窗口已过期）时生效；窗口内后续调用不会延长。
    fn increment(&self, key: &str, window: Duration) -> CounterHit;

    /// 移除单个计数（如登录成功/管理员解封后清零）。
    fn remove(&self, key: &str);

    /// 清空全部计数（配置热重载/测试）。
    fn clear(&self);

    /// 当前保存的键数量（诊断用；共享存储可返回估算值）。
    fn len(&self) -> usize;

    /// 是否为空。
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 一次计数结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CounterHit {
    /// 当前窗口内的累计次数（含本次）
    pub count: u64,
    /// 当前窗口剩余时间（用于 `Retry-After`）
    pub reset_in: Duration,
}

/// 限流判定结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Decision {
    /// 本次是否放行
    pub allowed: bool,
    /// 当前窗口计数（含本次）
    pub count: u64,
    /// 配置的上限
    pub limit: u64,
    /// 当前窗口剩余时间（未放行时用于 `Retry-After`）
    pub reset_in: Duration,
}

/// 固定窗口限流器（线程安全；默认使用进程内 [`FixedWindowStore`]）。
pub struct WindowLimiter {
    store: Arc<dyn CounterStore>,
}

impl WindowLimiter {
    /// 使用默认进程内存储。
    pub fn new() -> Self {
        Self::with_store(Arc::new(FixedWindowStore::new()))
    }

    /// 使用自定义存储（如 Redis 实现）。
    pub fn with_store(store: Arc<dyn CounterStore>) -> Self {
        Self { store }
    }

    /// 底层存储。
    pub fn store(&self) -> &Arc<dyn CounterStore> {
        &self.store
    }

    /// 记一次命中并按 `limit` 判定。
    ///
    /// `limit == 0` 或 `window == 0` 视为关闭：直接放行且**不计数**。
    pub fn hit(&self, key: &str, limit: u64, window: Duration) -> Decision {
        if limit == 0 || window.is_zero() {
            return Decision {
                allowed: true,
                count: 0,
                limit,
                reset_in: Duration::ZERO,
            };
        }
        let hit = self.store.increment(key, window);
        Decision {
            allowed: hit.count <= limit,
            count: hit.count,
            limit,
            reset_in: hit.reset_in,
        }
    }

    /// 移除单个键（不存在的键忽略）。
    pub fn remove(&self, key: &str) {
        self.store.remove(key);
    }

    /// 清空全部计数。
    pub fn clear(&self) {
        self.store.clear();
    }
}

impl Default for WindowLimiter {
    fn default() -> Self {
        Self::new()
    }
}

/// 单个键的窗口状态。
struct Entry {
    /// 当前窗口内的累计次数
    count: u64,
    /// 当前窗口开始时间
    start: Instant,
    /// 窗口长度（首次创建时确定）
    window: Duration,
}

/// 进程内固定窗口存储（分片 `Mutex<HashMap>`，降低高并发争用）。
///
/// 每个分片有键数上限（默认 [`Self::DEFAULT_MAX_ENTRIES_PER_SHARD`]）：
/// 达到上限时 **O(1)** 移除任意一个键再插入，保证内存有界且热路径不扫描全表
/// （被移除的键计数归零，属于内存安全优先的取舍；限流键应使用 IP / 哈希短值等低基数输入）。
/// 如需批量回收过期键，可显式调用 [`Self::prune_expired`]。
pub struct FixedWindowStore {
    shards: Vec<Mutex<HashMap<String, Entry>>>,
    max_entries_per_shard: usize,
}

impl FixedWindowStore {
    /// 每分片默认键数上限。
    pub const DEFAULT_MAX_ENTRIES_PER_SHARD: usize = 8192;

    /// 使用默认上限构建（分片数按 CPU 并行度，4~16）。
    pub fn new() -> Self {
        Self::with_max_entries(Self::DEFAULT_MAX_ENTRIES_PER_SHARD)
    }

    /// 指定每分片键数上限（至少 1）。
    pub fn with_max_entries(max_entries_per_shard: usize) -> Self {
        let shard_count = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .clamp(4, 16);
        Self {
            shards: (0..shard_count)
                .map(|_| Mutex::new(HashMap::new()))
                .collect(),
            max_entries_per_shard: max_entries_per_shard.max(1),
        }
    }

    /// 分片数量（诊断用）。
    pub fn shard_count(&self) -> usize {
        self.shards.len()
    }

    /// 当前保存的键数量。
    pub fn len(&self) -> usize {
        self.shards.iter().map(|s| lock(s).len()).sum()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 清理所有已过期键，返回清理数量。
    ///
    /// 正常使用无需手工调用（达到分片上限时自动清理）；关闭限流的进程可借此释放内存。
    pub fn prune_expired(&self) -> usize {
        let now = Instant::now();
        let mut removed = 0;
        for shard in &self.shards {
            let mut map = lock(shard);
            let before = map.len();
            map.retain(|_, e| now.saturating_duration_since(e.start) < e.window);
            removed += before - map.len();
        }
        removed
    }

    fn shard(&self, key: &str) -> &Mutex<HashMap<String, Entry>> {
        // 复用 `sign::fnv1a_64`（稳定、无随机化）；分片只需均匀分布
        let hash = crate::sign::fnv1a_64(key);
        &self.shards[(hash as usize) % self.shards.len()]
    }
}

impl Default for FixedWindowStore {
    fn default() -> Self {
        Self::new()
    }
}

impl CounterStore for FixedWindowStore {
    fn increment(&self, key: &str, window: Duration) -> CounterHit {
        let now = Instant::now();
        let mut map = lock(self.shard(key));

        if let Some(entry) = map.get_mut(key) {
            let elapsed = now.saturating_duration_since(entry.start);
            if elapsed < entry.window {
                entry.count = entry.count.saturating_add(1);
                return CounterHit {
                    count: entry.count,
                    reset_in: entry.window.saturating_sub(elapsed),
                };
            }
            // 上一窗口已过期：就地开启新窗口
            entry.count = 1;
            entry.start = now;
            entry.window = window;
            return CounterHit {
                count: 1,
                reset_in: window,
            };
        }

        // 新键：分片满时 O(1) 淘汰一个键，保证内存有界。
        // 不在插入热路径做 O(n) 全表扫描——唯一键攻击下（大量伪造 IP/头值）会变成 CPU 热点；
        // 过期键由 `prune_expired()` 或后续覆盖逐步清理。被淘汰的键计数归零，
        // 属于「内存安全优先」的取舍（限流键应使用 IP/哈希短值等低基数输入）。
        if map.len() >= self.max_entries_per_shard {
            let victim = map.keys().next().cloned();
            if let Some(victim) = victim {
                map.remove(&victim);
            }
        }

        map.insert(
            key.to_string(),
            Entry {
                count: 1,
                start: now,
                window,
            },
        );
        CounterHit {
            count: 1,
            reset_in: window,
        }
    }

    fn remove(&self, key: &str) {
        lock(self.shard(key)).remove(key);
    }

    fn clear(&self) {
        for shard in &self.shards {
            lock(shard).clear();
        }
    }

    fn len(&self) -> usize {
        FixedWindowStore::len(self)
    }
}

/// 锁中毒容忍：限流计数不是关键数据，panic 后继续使用旧数据比拒绝服务更合理。
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limiter() -> WindowLimiter {
        WindowLimiter::new()
    }

    #[test]
    fn allows_exact_limit_then_denies() {
        let limiter = limiter();
        for i in 1..=3 {
            let d = limiter.hit("k", 3, Duration::from_secs(60));
            assert!(d.allowed, "第 {i} 次应放行");
            assert_eq!(d.count, i);
        }
        let d = limiter.hit("k", 3, Duration::from_secs(60));
        assert!(!d.allowed, "第 4 次应拒绝");
        assert_eq!(d.count, 4);
        assert!(d.reset_in <= Duration::from_secs(60));
    }

    #[test]
    fn keys_are_isolated() {
        let limiter = limiter();
        assert!(limiter.hit("a", 1, Duration::from_secs(60)).allowed);
        assert!(!limiter.hit("a", 1, Duration::from_secs(60)).allowed);
        assert!(limiter.hit("b", 1, Duration::from_secs(60)).allowed);
    }

    #[test]
    fn window_expiry_resets_counter() {
        let limiter = limiter();
        let window = Duration::from_millis(30);
        assert!(limiter.hit("k", 1, window).allowed);
        assert!(!limiter.hit("k", 1, window).allowed);
        std::thread::sleep(Duration::from_millis(60));
        let d = limiter.hit("k", 1, window);
        assert!(d.allowed, "窗口过期后应重新计数");
        assert_eq!(d.count, 1);
    }

    #[test]
    fn zero_limit_or_window_is_unlimited_and_does_not_count() {
        let limiter = limiter();
        for _ in 0..100 {
            assert!(limiter.hit("k", 0, Duration::from_secs(60)).allowed);
            assert!(limiter.hit("k", 10, Duration::ZERO).allowed);
        }
        assert_eq!(limiter.store().len(), 0, "关闭态不应写入计数");
    }

    #[test]
    fn remove_and_clear_work() {
        let limiter = limiter();
        limiter.hit("a", 1, Duration::from_secs(60));
        limiter.hit("b", 1, Duration::from_secs(60));
        limiter.remove("a");
        assert_eq!(limiter.store().len(), 1);
        limiter.clear();
        assert_eq!(limiter.store().len(), 0);
        assert!(limiter.hit("a", 1, Duration::from_secs(60)).allowed);
    }

    #[test]
    fn concurrent_hits_are_atomic() {
        let limiter = Arc::new(limiter());
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let limiter = Arc::clone(&limiter);
                std::thread::spawn(move || {
                    for _ in 0..1000 {
                        limiter.hit("hot", 10_000, Duration::from_secs(60));
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().expect("线程不 panic");
        }
        let extra = limiter.store().increment("hot", Duration::from_secs(60));
        assert_eq!(extra.count, 8001, "并发递增不能丢失");
    }

    #[test]
    fn store_prunes_expired_entries() {
        let store = FixedWindowStore::with_max_entries(4);
        let window = Duration::from_millis(20);
        for i in 0..200 {
            store.increment(&format!("key-{i}"), window);
        }
        // 分片上限保证内存有界（每片最多 4 个键）
        assert!(
            store.len() <= 4 * store.shard_count(),
            "键数应有上限：{} > {}",
            store.len(),
            4 * store.shard_count()
        );
        std::thread::sleep(Duration::from_millis(50));
        let before = store.len();
        assert_eq!(store.prune_expired(), before, "过期键应全部清理");
        assert!(store.is_empty());
    }

    #[test]
    fn custom_store_is_used_by_limiter() {
        struct Fixed(CounterHit);
        impl CounterStore for Fixed {
            fn increment(&self, _key: &str, _window: Duration) -> CounterHit {
                self.0
            }
            fn remove(&self, _key: &str) {}
            fn clear(&self) {}
            fn len(&self) -> usize {
                1
            }
        }

        let limiter = WindowLimiter::with_store(Arc::new(Fixed(CounterHit {
            count: 5,
            reset_in: Duration::from_secs(7),
        })));
        let d = limiter.hit("k", 4, Duration::from_secs(60));
        assert!(!d.allowed);
        assert_eq!(d.count, 5);
        assert_eq!(d.reset_in, Duration::from_secs(7));
    }
}
