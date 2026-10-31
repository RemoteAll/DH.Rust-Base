//! 进程内 TTL 缓存（热路径元数据缓存）。
//!
//! 用途：把「极少变化的元数据」（设备、产品配置、令牌等）缓存在进程内，
//! 避免每条消息/每个请求都打一次数据库。对 MySQL/PostgreSQL 这类网络库，
//! **一次往返就是整条链路的主要成本**，缓存命中即 0 次往返。
//!
//! 设计取舍：
//! - 只做 TTL + 容量上限，**不做 LRU**——但满容时**不是整体清空**，而是「按写入时刻淘汰最旧的到上限的 90%」，
//!   一次只让 10% 的键回源（详见 [`TtlCache::insert`]）；
//! - 淘汰时**不克隆 key、不全量排序**：只收集时间戳（`Copy`）求阈值，再限量删除；
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
    /// 容量上限（超过时先清过期，仍超限则**按写入时刻淘汰最旧的到 90%**）。
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
    /// - `max_entries`：容量上限（**`0` = 不限**，按实际负载自然增长，靠 [`TtlCache::purge_expired`]
    ///   主动回收；非 0 时内部最小 16）。
    ///
    /// # 返回值
    /// 缓存实例。
    pub fn new(ttl_secs: u64, max_entries: usize) -> Self {
        Self {
            ttl: (ttl_secs > 0).then(|| Duration::from_secs(ttl_secs)),
            // `0` 保留为「不限」，不能被 `.max(16)` 吃掉（否则「设 0 = 不限」的调用方会静默变成 16 条）
            max_entries: if max_entries == 0 { 0 } else { max_entries.max(16) },
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
        self.make_room(&mut guard);
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

    /// **已占用条数是否已达上限**（只对固定容量有意义；`max_entries = 0` 时恒为 `false`）。
    ///
    /// # 返回值
    /// 已达上限为 `true`。
    pub fn is_saturated(&self) -> bool {
        self.max_entries > 0 && self.len() >= self.max_entries
    }

    /// 主动清掉已过期的条目（后台定时任务调用）。
    ///
    /// `max_entries = 0`（不限容量）时必须靠它回收：否则过期条目永远留在表里，
    /// 内存会随「写入速率 × 时间」无限增长。
    ///
    /// # 返回值
    /// 实际清掉的条数（关闭缓存时恒为 0）。
    pub fn purge_expired(&self) -> usize {
        let Some(ttl) = self.ttl else {
            return 0;
        };
        let mut guard = match self.map.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let before = guard.len();
        guard.retain(|_, (at, _)| at.elapsed() < ttl);
        let kept = guard.len();
        let removed = before - kept;
        if removed > 0 {
            // 收缩容量：过期批次删完后表可能只剩零星几条，不收缩会长期占着大块内存
            guard.shrink_to((kept * 2).max(16));
        }
        removed
    }

    /// 键不存在（或已过期）时插入并返回 `true`；已存在且**未过期**时返回 `false`、不写入。
    ///
    /// 专为「去重 / 幂等」场景设计：`get` + `insert` 要**两次加锁 + 两次哈希**，
    /// 而热路径上常见的写法就是这两个（实测 HlktechIoT 属性上报每点 152.6 ns，
    /// 合并后 132.9 ns）。本方法只加一次锁、只探测一次。
    ///
    /// # 参数
    /// - `key`：缓存键。
    ///
    /// # 返回值
    /// 本次插入返回 `true`；键已存在且未过期返回 `false`。
    pub fn insert_if_absent(&self, key: K) -> bool
    where
        V: Default,
    {
        let Some(ttl) = self.ttl else {
            return false;
        };
        let mut guard = match self.map.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        // 单次探测：命中且未过期 → 视为已存在；命中但已过期 → 覆盖写入并视为新插入
        if let Some((at, _)) = guard.get(&key) {
            if at.elapsed() < ttl {
                return false;
            }
        }
        self.make_room(&mut guard);
        guard.insert(key, (Instant::now(), V::default()));
        true
    }

    /// 容量保护（`insert` / `insert_if_absent` 共用，调用方**必须已持锁**）。
    ///
    /// # 参数
    /// - `guard`：已加锁的映射表。
    fn make_room(&self, guard: &mut HashMap<K, (Instant, V)>) {
        if self.max_entries == 0 || guard.len() < self.max_entries {
            return;
        }
        if let Some(ttl) = self.ttl {
            guard.retain(|_, (at, _)| at.elapsed() < ttl);
        }
        if guard.len() < self.max_entries {
            return;
        }
        // 按最旧淘汰到上限的 90%：一次只让 10% 的键回源，避免全量失效尖刺。
        //
        // **这里在持锁状态下执行**，所以必须廉价：旧实现把「全部 key 克隆进 Vec + 全量排序」
        // （`guard.iter().map(|(k,(at,_))| (*at, k.clone()))` + `sort_by_key`），
        // 5 万容量时单次约 5~6 ms；百万设备场景下每 0.3 秒触发一次，
        // 会周期性制造毫秒级尾延迟尖刺（见审计报告 R6）。
        //
        // 现在：① 只收集时间戳（`Instant` 是 Copy，不克隆 key），
        //       ② `select_nth_unstable` 求第 excess 旧的时间戳（O(n)，不排序），
        //       ③ `retain` 删除旧于该阈值的最多 `excess` 条。
        // 全程零 key 克隆，单次从 O(n log n) 降到 O(n)。
        let target = (self.max_entries * 9 / 10).max(1);
        let excess = guard.len().saturating_sub(target);
        if excess == 0 {
            return;
        }
        let mut ages: Vec<Instant> = guard.values().map(|(at, _)| *at).collect();
        let (_, cutoff, _) = ages.select_nth_unstable(excess - 1);
        let cutoff = *cutoff;
        // 时间戳可能相同：用 `excess` 封顶，避免一次误删过多
        let mut removed = 0usize;
        guard.retain(|_, (at, _)| {
            if removed < excess && *at <= cutoff {
                removed += 1;
                false
            } else {
                true
            }
        });
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
        assert_eq!(off.get_str("dev-001"), None);
        off.remove_str("dev-001");
        off.insert("x".to_string(), 1);
        assert_eq!(off.get_str("x"), None);
    }

    /// **`max_entries = 0` 表示不限**：按实际负载自然增长，不能被 `.max(16)` 吃掉。
    ///
    /// 为什么守这条：口径是「内存管够，按实际量自然占用，只是不要浪费」。
    /// 若 `0` 被静默当成 16，调用方写 `max = 0`（意为不限）会得到一个 16 条的缓存，
    /// 表现为「缓存随机失效、查库量暴涨」，且很难看出来。
    #[test]
    fn zero_max_entries_means_unlimited() {
        let cache: TtlCache<u32, u32> = TtlCache::new(600, 0);
        for i in 0..5_000u32 {
            cache.insert(i, i);
        }
        assert_eq!(cache.len(), 5_000, "0 必须表示不限，不能淘汰到 16 或 90%");
        assert!(!cache.is_saturated(), "不限容量时永远不算饱和");
    }

    /// **`insert_if_absent`：一次加锁完成「判重 + 写入」**，语义等同 `get` + `insert`。
    #[test]
    fn insert_if_absent_dedups_in_one_pass() {
        let cache: TtlCache<u64, ()> = TtlCache::new(600, 0);
        assert!(cache.insert_if_absent(7), "首次应插入");
        assert!(!cache.insert_if_absent(7), "重复应返回 false 且不覆盖");
        assert!(cache.insert_if_absent(8), "不同键应插入");
        assert_eq!(cache.len(), 2);

        // 过期后应重新视为「不存在」
        let short: TtlCache<u64, ()> = TtlCache::new(1, 0);
        assert!(short.insert_if_absent(1));
        {
            let mut guard = short.map.lock().expect("锁");
            if let Some(entry) = guard.get_mut(&1) {
                entry.0 = Instant::now() - Duration::from_secs(5);
            }
        }
        assert!(short.insert_if_absent(1), "过期条目必须视为不存在");

        // 关闭缓存时恒返回 false（不缓存 = 不去重），与 `get`/`insert` 的空操作口径一致
        let off: TtlCache<u64, ()> = TtlCache::new(0, 0);
        assert!(!off.insert_if_absent(1));
    }

    /// **`purge_expired` 主动回收**：`max_entries = 0` 时靠它把内存还回去。
    #[test]
    fn purge_expired_reclaims_and_shrinks() {
        let cache: TtlCache<u32, u32> = TtlCache::new(1, 0);
        for i in 0..1_000u32 {
            cache.insert(i, i);
        }
        assert_eq!(cache.len(), 1_000);
        // 尚未过期 → 一条都不该删
        assert_eq!(cache.purge_expired(), 0);
        // 全部改成 5 秒前写入 → 应全部回收
        {
            let mut guard = cache.map.lock().expect("锁");
            let old = Instant::now() - Duration::from_secs(5);
            for entry in guard.values_mut() {
                entry.0 = old;
            }
        }
        assert_eq!(cache.purge_expired(), 1_000);
        assert_eq!(cache.len(), 0);

        // 关闭缓存时为 0 且不 panic
        let off: TtlCache<u32, u32> = TtlCache::new(0, 0);
        assert_eq!(off.purge_expired(), 0);
    }

    /// 超过容量上限时先清过期，再渐进淘汰（不会无限增长）。
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
