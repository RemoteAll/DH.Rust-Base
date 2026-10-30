//! 进程内 TTL 缓存（热路径元数据缓存）。
//!
//! 用途：把「极少变化的元数据」（设备、产品配置、令牌等）缓存在进程内，
//! 避免每条消息/每个请求都打一次数据库。对 MySQL/PostgreSQL 这类网络库，
//! **一次往返就是整条链路的主要成本**，缓存命中即 0 次往返。
//!
//! 设计取舍：
//! - 只做 TTL + 容量上限，**不做 LRU**（IoT/设备场景 key 数量有界，超限时整体清理更简单可靠）；
//! - 值按 `Clone` 返回（元数据结构小），避免与锁生命周期纠缠；
//! - `ttl_secs = 0` 时退化为「不缓存」，调用方无需分支。
//!
//! 来源：自 HlktechIoT Rust 接入层（心跳热路径「设备/产品元数据缓存」，
//! 实测把稳定态每跳 1~2 次 SELECT 降到 0 次）下沉为公共件。

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 进程内 TTL 缓存。
///
/// 内部只有一把 `Mutex`，命中路径为「一次加锁 + 一次 HashMap 查」，
/// 适合每秒数万次读取的元数据缓存；**不要**用来缓存大对象或高频写的数据。
pub struct TtlCache<K, V> {
    /// 有效期（`None` = 不缓存）。
    ttl: Option<Duration>,
    /// 容量上限（超过后先清过期、再整体清空）。
    max_entries: usize,
    /// 缓存内容：key → (写入时刻, 值)。
    map: Mutex<HashMap<K, (Instant, V)>>,
}

impl<K, V> TtlCache<K, V>
where
    K: Eq + Hash + Clone,
    V: Clone,
{
    /// 新建缓存。
    ///
    /// # 参数
    /// - `ttl_secs`：有效期秒数（`0` = 关闭缓存，读写都是空操作）。
    /// - `max_entries`：容量上限（建议不小于「热数据条数」；内部最小 16）。
    ///
    /// # 返回值
    /// 缓存实例。
    pub fn new(ttl_secs: u64, max_entries: usize) -> Self {
        Self {
            ttl: (ttl_secs > 0).then(|| Duration::from_secs(ttl_secs)),
            max_entries: max_entries.max(16),
            map: Mutex::new(HashMap::new()),
        }
    }

    /// 是否启用了缓存。
    ///
    /// # 返回值
    /// 启用为 `true`。
    pub fn enabled(&self) -> bool {
        self.ttl.is_some()
    }

    /// 有效期。
    ///
    /// # 返回值
    /// 有效期为 `Some`，关闭为 `None`。
    pub fn ttl(&self) -> Option<Duration> {
        self.ttl
    }

    /// 读取缓存（过期即视为未命中）。
    ///
    /// # 参数
    /// - `key`：缓存键。
    ///
    /// # 返回值
    /// 命中且未过期时返回值副本。
    pub fn get(&self, key: &K) -> Option<V> {
        let ttl = self.ttl?;
        let guard = match self.map.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let (at, value) = guard.get(key)?;
        (at.elapsed() < ttl).then(|| value.clone())
    }

    /// 写入缓存（关闭缓存时为空操作）。
    ///
    /// 容量策略：满了先清过期；仍满则**按写入时间淘汰最旧的 ~10%**（而不是整体清空）——
    /// 整体清空会让下一次请求全部打回数据库，形成周期性延迟尖刺
    /// （来源：HlktechIoT 设备缓存实测教训，2026-10-30 下沉时保留该行为）。
    ///
    /// # 参数
    /// - `key`：缓存键。
    /// - `value`：值。
    pub fn insert(&self, key: K, value: V) {
        if self.ttl.is_none() {
            return;
        }
        let mut guard = match self.map.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if guard.len() >= self.max_entries {
            if let Some(ttl) = self.ttl {
                guard.retain(|_, (at, _)| at.elapsed() < ttl);
            }
            if guard.len() >= self.max_entries {
                // 按最旧淘汰到上限的 90%：一次只让 10% 的键回源，避免全量失效尖刺
                let target = (self.max_entries * 9 / 10).max(1);
                let excess = guard.len().saturating_sub(target);
                let mut ages: Vec<(Instant, K)> = guard
                    .iter()
                    .map(|(k, (at, _))| (*at, k.clone()))
                    .collect();
                ages.sort_by_key(|(at, _)| *at);
                for (_, k) in ages.into_iter().take(excess) {
                    guard.remove(&k);
                }
            }
        }
        guard.insert(key, (Instant::now(), value));
    }

    /// 让某个键立即失效（写库后主动清理）。
    ///
    /// # 参数
    /// - `key`：缓存键。
    pub fn remove(&self, key: &K) {
        if let Ok(mut guard) = self.map.lock() {
            guard.remove(key);
        }
    }

    /// 清空全部缓存（管理员改库后使用）。
    pub fn clear(&self) {
        if let Ok(mut guard) = self.map.lock() {
            guard.clear();
        }
    }

    /// 当前缓存条数（含已过期但尚未清理的条目）。
    ///
    /// # 返回值
    /// 条数（关闭缓存时恒为 0）。
    pub fn len(&self) -> usize {
        self.map.lock().map(|guard| guard.len()).unwrap_or(0)
    }

    /// 缓存是否为空。
    ///
    /// # 返回值
    /// 空为 `true`。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<V> TtlCache<String, V>
where
    V: Clone,
{
    /// 按 `&str` 读取（**零分配**）。
    ///
    /// 热路径上常见的写法是 `get(&key.to_string())` —— 每次查表都要先分配一个 `String`
    /// 才能满足 `&K` 参数（设备上报场景实测每条消息一次多余分配）。
    /// 本方法用 `String: Borrow<str>` 让 `HashMap` 直接按字符串切片查询，不产生分配。
    ///
    /// # 参数
    /// - `key`：缓存键（字符串切片）。
    ///
    /// # 返回值
    /// 命中且未过期时返回值副本。
    pub fn get_str(&self, key: &str) -> Option<V> {
        let ttl = self.ttl?;
        let guard = match self.map.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let (at, value) = guard.get(key)?;
        (at.elapsed() < ttl).then(|| value.clone())
    }

    /// 按 `&str` 让某个键立即失效（**零分配**）。
    ///
    /// # 参数
    /// - `key`：缓存键（字符串切片）。
    pub fn remove_str(&self, key: &str) {
        if let Ok(mut guard) = self.map.lock() {
            guard.remove(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **容量满时只淘汰最旧的 ~10%，不做整体清空**。
    ///
    /// 为什么守这条：整体清空会让下一次请求全部回源（数据库瞬间打满）；
    /// HlktechIoT 设备缓存在 10 万设备规模下就是靠"渐进淘汰"避免周期性尖刺的。
    #[test]
    fn cache_evicts_oldest_tenth_instead_of_clearing() {
        let cache: TtlCache<u32, u32> = TtlCache::new(600, 20);
        for i in 0..20u32 {
            cache.insert(i, i);
            // 让写入时间有先后（Instant 精度在部分平台较粗，这里只保证单调）
            std::thread::yield_now();
        }
        // 第 21 条触发淘汰：应保留大部分旧键（10% = 2 条被淘汰），而不是清空
        cache.insert(100, 100);
        let len = cache.len();
        assert!(len >= 17, "淘汰后应保留大部分条目，实际 {len}");
        assert!(len < 20, "应确实淘汰了最旧的几条，实际 {len}");
        assert_eq!(cache.get(&100), Some(100), "新写入的键必须在");
    }

    /// 命中后返回同一份值；过期后视为未命中。
    #[test]
    fn ttl_cache_hits_then_expires() {
        let cache: TtlCache<String, i32> = TtlCache::new(1, 16);
        assert!(cache.enabled());
        assert!(cache.get(&"a".to_string()).is_none());

        cache.insert("a".to_string(), 7);
        assert_eq!(cache.get(&"a".to_string()), Some(7));

        // 手工把时间戳改到过去，模拟过期（避免 sleep 拖慢测试）
        {
            let mut guard = cache.map.lock().unwrap();
            let old = Instant::now() - Duration::from_secs(5);
            if let Some(entry) = guard.get_mut(&"a".to_string()) {
                entry.0 = old;
            }
        }
        assert!(cache.get(&"a".to_string()).is_none());
    }

    /// `ttl_secs = 0` 时退化为不缓存，读写都是空操作。
    #[test]
    fn zero_ttl_disables_cache() {
        let cache: TtlCache<i32, i32> = TtlCache::new(0, 16);
        assert!(!cache.enabled());
        cache.insert(1, 2);
        assert_eq!(cache.get(&1), None);
        assert_eq!(cache.len(), 0);
    }

    /// `String` 键可用 `&str` 查询/删除（零分配热路径），行为与 `get(&String)` 完全一致。
    #[test]
    fn str_key_can_be_queried_without_allocation() {
        let cache: TtlCache<String, i32> = TtlCache::new(60, 16);
        cache.insert("dev-001".to_string(), 9);
        assert_eq!(cache.get_str("dev-001"), Some(9), "按 &str 命中");
        assert_eq!(cache.get_str("dev-002"), None, "未命中不 panic");
        cache.remove_str("dev-001");
        assert_eq!(cache.get_str("dev-001"), None, "按 &str 失效生效");

        // 关闭缓存时同样是空操作（不 panic）
        let off: TtlCache<String, i32> = TtlCache::new(0, 16);
        off.insert("a".into(), 1);
        assert_eq!(off.get_str("a"), None);
    }

    /// 超过容量上限时先清过期，再整体清空（不会无限增长）。
    #[test]
    fn capacity_is_bounded() {
        let cache: TtlCache<u32, u32> = TtlCache::new(60, 16);
        for i in 0..64 {
            cache.insert(i, i);
        }
        assert!(cache.len() <= 16, "容量应受限于 16，实际 {}", cache.len());
    }

    /// 主动失效与清空。
    #[test]
    fn remove_and_clear() {
        let cache: TtlCache<u32, u32> = TtlCache::new(60, 16);
        cache.insert(1, 1);
        cache.insert(2, 2);
        cache.remove(&1);
        assert!(cache.get(&1).is_none());
        assert_eq!(cache.get(&2), Some(2));
        cache.clear();
        assert!(cache.is_empty());
    }
}
