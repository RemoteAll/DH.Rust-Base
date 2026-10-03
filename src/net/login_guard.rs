//! 登录限流（按来源的失败计数/封禁）。
//!
//! 默认策略：同一 key（一般为客户端 IP）在 **15 分钟**窗口内失败 **5 次** →
//! 封禁 **5 分钟**；窗口过期自动重置计数；登录成功清除记录。
//! 策略可经 [`LoginGuard::with_policy`] 定制。
//!
//! 本模块为 Pek 系服务面板（Pek.RAgent / HlkProductTool）同款实现的收编：
//! 两项目原各自内嵌一模一样的计数/封禁逻辑，现统一由本模块提供。
//!
//! ```no_run
//! use dhrust::net::login_guard::LoginGuard;
//!
//! let guard = LoginGuard::new();
//! # let password_ok = true;
//! if guard.is_blocked("10.0.0.8") {
//!     // 返回 429
//! } else if password_ok {
//!     guard.record_success("10.0.0.8");
//! } else {
//!     guard.record_failure("10.0.0.8");
//! }
//! ```

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 登录尝试记录。
struct Attempt {
    /// 窗口内失败次数
    count: u32,
    /// 首次失败时间
    first: Instant,
    /// 封禁截止时间（None = 未封禁）
    blocked_until: Option<Instant>,
}

/// 登录限流器（线程安全；状态为进程内内存）。
pub struct LoginGuard {
    /// 来源 → 尝试记录
    inner: Mutex<HashMap<String, Attempt>>,
    /// 封禁阈值（窗口内失败次数）
    max_attempts: u32,
    /// 失败统计窗口
    window: Duration,
    /// 封禁时长
    block: Duration,
}

impl Default for LoginGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl LoginGuard {
    /// 默认策略：15 分钟窗口内失败 5 次 → 封禁 5 分钟。
    pub fn new() -> Self {
        Self::with_policy(
            5,
            Duration::from_secs(15 * 60),
            Duration::from_secs(5 * 60),
        )
    }

    /// 自定义策略（窗口内失败 `max_attempts` 次封禁 `block` 时长）。
    pub fn with_policy(max_attempts: u32, window: Duration, block: Duration) -> Self {
        LoginGuard {
            inner: Mutex::new(HashMap::new()),
            max_attempts,
            window,
            block,
        }
    }

    /// 指定 key 是否处于封禁中（封禁到期自动解除并清除记录）。
    pub fn is_blocked(&self, key: &str) -> bool {
        if key.is_empty() {
            return false;
        }

        let now = Instant::now();
        let mut inner = self.inner.lock().unwrap();
        let blocked = inner.get(key).and_then(|info| info.blocked_until);
        match blocked {
            Some(until) if until > now => true,
            Some(_) => {
                // 封禁已到期：清除记录
                inner.remove(key);
                false
            }
            None => false,
        }
    }

    /// 记录一次失败（窗口过期重置计数；达到阈值封禁）。
    pub fn record_failure(&self, key: &str) {
        if key.is_empty() {
            return;
        }

        let now = Instant::now();
        let mut inner = self.inner.lock().unwrap();
        let info = inner.entry(key.to_string()).or_insert(Attempt {
            count: 0,
            first: now,
            blocked_until: None,
        });

        // 窗口过期：重置计数
        if now.duration_since(info.first) > self.window {
            info.count = 0;
            info.first = now;
            info.blocked_until = None;
        }

        info.count += 1;
        if info.count >= self.max_attempts {
            info.blocked_until = Some(now + self.block);
        }
    }

    /// 记录成功登录（清除该 key 记录）。
    pub fn record_success(&self, key: &str) {
        if !key.is_empty() {
            self.inner.lock().unwrap().remove(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_after_threshold() {
        let guard = LoginGuard::with_policy(3, Duration::from_secs(10), Duration::from_secs(5));
        guard.record_failure("1.1.1.1");
        guard.record_failure("1.1.1.1");
        assert!(!guard.is_blocked("1.1.1.1"), "未达阈值不应封禁");
        guard.record_failure("1.1.1.1");
        assert!(guard.is_blocked("1.1.1.1"), "达到阈值应封禁");
        // 其他来源不受影响；空 key 忽略
        assert!(!guard.is_blocked("2.2.2.2"));
        assert!(!guard.is_blocked(""));
    }

    #[test]
    fn success_clears_record() {
        let guard = LoginGuard::with_policy(3, Duration::from_secs(10), Duration::from_secs(5));
        guard.record_failure("3.3.3.3");
        guard.record_failure("3.3.3.3");
        guard.record_success("3.3.3.3");
        guard.record_failure("3.3.3.3");
        guard.record_failure("3.3.3.3");
        assert!(!guard.is_blocked("3.3.3.3"), "成功后计数应清零");
    }

    #[test]
    fn window_expiry_resets_counter() {
        // 窗口很短：两次失败后等待窗口过期，计数应重置（不至于第三次就封禁）
        let guard = LoginGuard::with_policy(3, Duration::from_millis(30), Duration::from_secs(60));
        guard.record_failure("4.4.4.4");
        guard.record_failure("4.4.4.4");
        std::thread::sleep(Duration::from_millis(60));
        guard.record_failure("4.4.4.4");
        assert!(!guard.is_blocked("4.4.4.4"), "窗口过期后计数应重置");
    }
}
