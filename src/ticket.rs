//! 短时效票据库（平台签发 → 节点内存核销）。
//!
//! 典型场景：平台把「一次性 / 限额」的能力票据经实时通道下发到节点内存，
//! 客户端拿着票据直连节点（`/deploy/apply`、`/db/apply`），节点按票据放行。
//! 统一在这里实现，避免各模块各写一套 `Mutex<HashMap>` + TTL 清理 + 配额扣减。
//!
//! 语义：
//! - **有效期**：超过 `ttl` 的票据在每次操作时顺带清理（惰性，无定时器）；
//! - **容量**：表满（`max`）时拒绝新票据，防止平台异常刷爆内存；
//! - **配额**：每张票据最多消费 `quota` 次，用尽即删（一次性票据就传 `1`）；
//! - **只读校验**：[`TicketStore::peek`] 不扣配额（例如「读目录指纹」可反复读）；
//! - **票据 id**：仅允许 1~64 位十六进制（防注入/超长键）。
//!
//! 线程安全：内部单个 `Mutex`，操作都是 O(1) 级（外加过期项的 retain）。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 票据记录（对调用方不可见）。
struct Entry<T> {
    payload: T,
    issued: Instant,
    remaining: u32,
}

/// 短时效票据库（`T` = 票据内容）。
pub struct TicketStore<T> {
    ttl: Duration,
    max: usize,
    table: Mutex<HashMap<String, Entry<T>>>,
}

impl<T: Clone> TicketStore<T> {
    /// 构建（`ttl` 有效期、`max` 同时存在的票据上限）。
    ///
    /// 注：`HashMap::new` 在当前 MSRV 不是 `const fn`，因此配合 `OnceLock` 做静态初始化：
    /// `static S: OnceLock<TicketStore<T>> = OnceLock::new(); S.get_or_init(|| TicketStore::new(..))`。
    pub fn new(ttl: Duration, max: usize) -> Self {
        Self {
            ttl,
            max,
            table: Mutex::new(HashMap::new()),
        }
    }

    /// 票据 id 是否合法（1~64 位十六进制）。
    pub fn valid_id(id: &str) -> bool {
        !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_hexdigit())
    }

    /// 登记票据；返回 `false` = id 非法 / 表已满（调用方据此回 400 或记日志）。
    pub fn issue(&self, id: &str, payload: T, quota: u32) -> bool {
        if !Self::valid_id(id) {
            return false;
        }
        let mut m = self.lock();
        m.retain(|_, t| t.issued.elapsed() < self.ttl);
        if m.len() >= self.max {
            return false;
        }
        m.insert(
            id.to_ascii_lowercase(),
            Entry {
                payload,
                issued: Instant::now(),
                remaining: quota,
            },
        );
        true
    }

    /// 读取票据但**不扣配额**（只读校验场景）；过期或不存在返回 `None`。
    pub fn peek(&self, id: &str) -> Option<T> {
        if !Self::valid_id(id) {
            return None;
        }
        let mut m = self.lock();
        m.retain(|_, t| t.issued.elapsed() < self.ttl);
        m.get(&id.to_ascii_lowercase()).map(|t| t.payload.clone())
    }

    /// 核销一次：先跑 `check`（业务校验，如连接名/语句级别），通过后扣一次配额。
    ///
    /// - 票据不存在/过期 → `Err("票据无效或已过期…")`
    /// - 配额用尽 → `Err("…请求次数已用尽…")`
    /// - `check` 拒绝 → 原样返回其错误（**不扣配额**，便于同一票据多次试错）
    /// - 扣到 0 时立即删除，不可重放。
    pub fn take(
        &self,
        id: &str,
        check: impl FnOnce(&T) -> Result<(), String>,
    ) -> Result<T, String> {
        if !Self::valid_id(id) {
            return Err("票据格式无效".to_string());
        }
        let key = id.to_ascii_lowercase();
        let mut m = self.lock();
        m.retain(|_, t| t.issued.elapsed() < self.ttl);
        let Some(t) = m.get_mut(&key) else {
            return Err("票据无效或已过期（请重新申请）".to_string());
        };
        if t.issued.elapsed() >= self.ttl {
            m.remove(&key);
            return Err("票据已过期（请重新申请）".to_string());
        }
        if t.remaining == 0 {
            m.remove(&key);
            return Err("票据请求次数已用尽（请重新申请）".to_string());
        }
        check(&t.payload)?;
        t.remaining -= 1;
        let out = t.payload.clone();
        if t.remaining == 0 {
            m.remove(&key);
        }
        Ok(out)
    }

    /// 当前有效票据数（面板/诊断用）。
    pub fn len(&self) -> usize {
        let mut m = self.lock();
        m.retain(|_, t| t.issued.elapsed() < self.ttl);
        m.len()
    }

    /// 是否没有有效票据。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 锁中毒也恢复（一次 panic 不该让票据功能整体不可用）。
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry<T>>> {
        self.table.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> TicketStore<String> {
        TicketStore::new(Duration::from_secs(60), 4)
    }

    #[test]
    fn issue_peek_take_and_quota() {
        let s = store();
        assert!(s.issue("0123456789abcdef", "payload".into(), 2));
        // 非法 id
        assert!(!s.issue("not-hex", "x".into(), 1));
        // peek 不扣配额
        assert_eq!(s.peek("0123456789ABCDEF").as_deref(), Some("payload"));
        assert_eq!(s.peek("0123456789abcdef").as_deref(), Some("payload"));
        // 业务校验失败不扣配额
        let err = s.take("0123456789abcdef", |_| Err("拒绝".into())).unwrap_err();
        assert!(err.contains("拒绝"), "{err}");
        // 两次消费后删除
        assert!(s.take("0123456789abcdef", |_| Ok(())).is_ok());
        assert!(s.take("0123456789abcdef", |_| Ok(())).is_ok());
        assert!(s.take("0123456789abcdef", |_| Ok(())).is_err());
        assert!(s.peek("0123456789abcdef").is_none());
    }

    #[test]
    fn capacity_is_bounded() {
        let s = store();
        for i in 0..4u8 {
            let id = format!("{:016x}", i);
            assert!(s.issue(&id, "x".into(), 1), "第 {i} 张应登记成功");
        }
        assert!(!s.issue("ffffffffffffffff", "x".into(), 1), "表满应拒绝");
        assert_eq!(s.len(), 4);
    }

    #[test]
    fn expired_entries_are_cleaned() {
        let s: TicketStore<String> = TicketStore::new(Duration::from_millis(10), 4);
        assert!(s.issue("0011223344556677", "x".into(), 1));
        std::thread::sleep(Duration::from_millis(30));
        assert!(s.peek("0011223344556677").is_none());
        assert!(s.is_empty());
    }
}
