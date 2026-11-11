//! 全局「在途请求体」预算（字节）——HTTP 服务的 OOM 保险。
//!
//! **为什么需要**：很多服务要把请求体整段收进内存（改写、解析、校验），
//! 单请求上限配好了（如 8MB），但并发连接上限是几百 —— 理论上限就是「几百 × 8MB ≈ GB 级」。
//! 命中「被刷 / 大量慢速大上传」时，这是最现实的 OOM 路径（不是泄漏，是瞬时峰值）。
//!
//! 语义：超过预算的请求**立刻拒绝**（调用方回 503 + Retry-After），不排队等内存，
//! 也不影响已在处理中的请求。预算按「声明长度」预留；拿不到长度（chunked）时
//! 按单请求上限预留 —— 宁可保守，避免被 chunked 绕过。
//!
//! 下沉自 Pek.Proxy `limits::BodyBudget`（任何收 body 的 HTTP 服务都适用）。
//!
//! ```no_run
//! use std::sync::Arc;
//! use dhrust::net::body_budget::BodyBudget;
//!
//! let budget = Arc::new(BodyBudget::new(256 * 1024 * 1024)); // 256MB 总预算
//! match budget.try_acquire(8 * 1024 * 1024) {
//!     Some(lease) => { /* 读 body；lease 离开作用域自动归还 */ let _ = lease.bytes(); }
//!     None => { /* 回 503 + Retry-After */ }
//! }
//! ```

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// 在途请求体预算（线程安全，`Arc` 共享）。
#[derive(Debug)]
pub struct BodyBudget {
    /// 预算上限（字节）；`0` = 不限制（仅测试/极端配置）
    limit: usize,
    /// 已预留字节数
    used: AtomicUsize,
}

impl BodyBudget {
    /// 新建（`limit = 0` 表示不限制）。
    pub fn new(limit: usize) -> Self {
        Self {
            limit,
            used: AtomicUsize::new(0),
        }
    }

    /// 预算上限（字节；0 = 不限）。
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// 当前已预留字节数。
    pub fn used(&self) -> usize {
        self.used.load(Ordering::Relaxed)
    }

    /// 剩余可用字节数（不限时返回 `usize::MAX`）。
    pub fn remaining(&self) -> usize {
        if self.limit == 0 {
            return usize::MAX;
        }
        self.limit.saturating_sub(self.used())
    }

    /// 尝试预留 `bytes`；超预算返回 `None`（调用方应快速失败，如 503）。
    ///
    /// - 单请求声明长度超过预算时**按预算封顶**（不因单个超大请求永久失败，
    ///   它会独占预算直到结束，其余请求被拒）；
    /// - 用 CAS 循环做「先看再加」，避免两个并发请求各自看到「还够」而双双超额。
    pub fn try_acquire(self: &Arc<Self>, bytes: usize) -> Option<BodyLease> {
        if self.limit == 0 {
            return Some(BodyLease {
                budget: self.clone(),
                bytes: 0,
            });
        }
        let bytes = bytes.min(self.limit);
        let mut current = self.used.load(Ordering::Relaxed);
        loop {
            let next = current.saturating_add(bytes);
            if next > self.limit {
                return None;
            }
            match self
                .used
                .compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Relaxed)
            {
                Ok(_) => {
                    return Some(BodyLease {
                        budget: self.clone(),
                        bytes,
                    })
                }
                Err(actual) => current = actual,
            }
        }
    }
}

/// 预算租约（`Drop` 时自动归还，异常分支也不会漏）。
#[derive(Debug)]
pub struct BodyLease {
    /// 所属预算
    budget: Arc<BodyBudget>,
    /// 本次预留的字节数
    bytes: usize,
}

impl BodyLease {
    /// 本次预留的字节数（`0` = 未限制或无需记账）。
    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

impl Drop for BodyLease {
    fn drop(&mut self) {
        if self.bytes > 0 {
            self.budget.used.fetch_sub(self.bytes, Ordering::AcqRel);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admits_within_limit_and_rejects_over() {
        let budget = Arc::new(BodyBudget::new(100));
        let a = budget.try_acquire(60).expect("60 ≤ 100");
        assert_eq!(budget.used(), 60);
        let b = budget.try_acquire(40).expect("40 恰好用满");
        assert_eq!(budget.used(), 100);
        assert!(budget.try_acquire(1).is_none(), "已满应拒绝");
        drop(a);
        assert_eq!(budget.used(), 40);
        drop(b);
        assert_eq!(budget.used(), 0);
        assert_eq!(budget.remaining(), 100);
    }

    #[test]
    fn single_request_is_clamped_to_limit() {
        let budget = Arc::new(BodyBudget::new(10));
        let lease = budget.try_acquire(usize::MAX).expect("封顶后仍可准入");
        assert_eq!(lease.bytes(), 10, "单请求最多占满预算");
        assert_eq!(budget.used(), 10);
        assert!(budget.try_acquire(1).is_none());
    }

    #[test]
    fn zero_means_unlimited() {
        let budget = Arc::new(BodyBudget::new(0));
        let lease = budget.try_acquire(usize::MAX).expect("0 = 不限制");
        assert_eq!(lease.bytes(), 0);
        assert_eq!(budget.used(), 0, "不限制时不做记账");
        assert_eq!(budget.remaining(), usize::MAX);
    }
}
