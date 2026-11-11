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
        Self::with_policy(5, Duration::from_secs(15 * 60), Duration::from_secs(5 * 60))
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

/// 按**账号**的失败计数 + 递增延时（不硬锁定）。
///
/// 与 [`LoginGuard`] 互补：`LoginGuard` 按来源（IP）封禁，但**换 IP 就能继续爆破**同一条口令；
/// 这里按账号另计一份，用「第 n 次失败 → 等 n × base，封顶 cap」的**递增延时**惩罚。
///
/// **只用延时、不用硬锁定**：硬锁定会变成「知道用户名就能把管理员锁在门外」的 DoS 手段；
/// 递增延时对暴力破解是线性惩罚，对正常用户只是「输错几次后稍等」。
///
/// 账号表有上限（默认 4096），超限先清过期再整体清空 —— 只影响延时强度，不影响安全边界。
/// 下沉自 Pek.Proxy `limits::AbuseGuard` 的账号维度实现。
///
/// ```no_run
/// use dhrust::net::login_guard::UserDelayGuard;
///
/// let guard = UserDelayGuard::new();
/// if let Some(wait) = guard.delay("admin").is_zero().then_some(0) {
///     // 0 = 不延时；否则先 sleep 再校验口令
/// }
/// ```
pub struct UserDelayGuard {
    /// 账号（小写）→ 窗口内失败次数与首次失败时刻
    inner: Mutex<HashMap<String, (u32, Instant)>>,
    /// 失败统计窗口
    window: Duration,
    /// 每次失败的基准延时（毫秒）
    base_ms: u64,
    /// 延时封顶（毫秒）
    cap_ms: u64,
    /// 账号表上限
    max_keys: usize,
}

impl Default for UserDelayGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl UserDelayGuard {
    /// 默认策略：15 分钟窗口、200ms × 次数、封顶 3s、最多 4096 个账号。
    pub fn new() -> Self {
        Self::with_policy(
            15 * 60,
            200,
            3_000,
            4096,
        )
    }

    /// 自定义策略。`window_secs`：失败统计窗口（秒）；`base_ms`：每次失败的基准延时；
    /// `cap_ms`：延时封顶；`max_keys`：账号表上限（至少 16）。
    pub fn with_policy(
        window_secs: u64,
        base_ms: u64,
        cap_ms: u64,
        max_keys: usize,
    ) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            window: Duration::from_secs(window_secs.max(1)),
            base_ms,
            cap_ms: cap_ms.max(base_ms),
            max_keys: max_keys.max(16),
        }
    }

    /// 纯函数：第 `failures` 次失败后应延时多少毫秒（`min(cap, base × 次数)`）。
    pub fn delay_ms(base_ms: u64, cap_ms: u64, failures: u32) -> u64 {
        (failures as u64)
            .saturating_mul(base_ms)
            .min(cap_ms)
    }

    /// 该账号当前窗口内的失败次数。
    pub fn failures(&self, user: &str) -> u32 {
        let key = user.trim().to_ascii_lowercase();
        if key.is_empty() {
            return 0;
        }
        let Ok(map) = self.inner.lock() else {
            return 0;
        };
        map.get(&key)
            .filter(|(_, first)| first.elapsed() <= self.window)
            .map(|(count, _)| *count)
            .unwrap_or(0)
    }

    /// 本次登录前应等待的时长（`Duration::ZERO` = 不延时）。
    pub fn delay(&self, user: &str) -> Duration {
        Duration::from_millis(Self::delay_ms(
            self.base_ms,
            self.cap_ms,
            self.failures(user),
        ))
    }

    /// 记一次失败，返回窗口内累计次数。
    pub fn record_failure(&self, user: &str) -> u32 {
        let key = user.trim().to_ascii_lowercase();
        if key.is_empty() {
            return 0;
        }
        let Ok(mut map) = self.inner.lock() else {
            return 0;
        };
        // 有界：超上限先清过期，再满就整体清空（只影响延时强度，不影响安全边界）
        if map.len() >= self.max_keys {
            let window = self.window;
            map.retain(|_, (_, first)| first.elapsed() <= window);
            if map.len() >= self.max_keys {
                map.clear();
            }
        }
        let window = self.window;
        let entry = map.entry(key).or_insert((0, Instant::now()));
        if entry.1.elapsed() > window {
            *entry = (0, Instant::now());
        }
        entry.0 = entry.0.saturating_add(1);
        entry.0
    }

    /// 登录成功后清空该账号的失败计数。
    pub fn record_success(&self, user: &str) {
        let key = user.trim().to_ascii_lowercase();
        if !key.is_empty() {
            if let Ok(mut map) = self.inner.lock() {
                map.remove(&key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_delay_grows_and_caps() {
        assert_eq!(UserDelayGuard::delay_ms(200, 3000, 0), 0);
        assert_eq!(UserDelayGuard::delay_ms(200, 3000, 1), 200);
        assert_eq!(UserDelayGuard::delay_ms(200, 3000, 5), 1_000);
        assert_eq!(UserDelayGuard::delay_ms(200, 3000, 15), 3_000, "封顶 3 秒");
    }

    #[test]
    fn user_delay_guard_counts_and_resets() {
        let guard = UserDelayGuard::with_policy(60, 100, 500, 64);
        assert!(guard.delay("Admin").is_zero(), "初始不延时");
        assert_eq!(guard.record_failure("admin"), 1);
        assert_eq!(guard.record_failure("ADMIN"), 2, "账号大小写不敏感");
        assert_eq!(guard.delay("admin"), Duration::from_millis(200));
        guard.record_success("admin");
        assert!(guard.delay("admin").is_zero(), "成功后清零");
        assert!(guard.delay("").is_zero(), "空账号不记录");
        assert_eq!(guard.record_failure(""), 0);
    }

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
