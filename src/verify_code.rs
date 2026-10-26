//! 一次性验证码仓库（短信 / 邮件 / 图形通用）：**有效期 + 重发间隔 + 错误次数上限**。
//!
//! 业务只关心「发一条验证码」与「校验一次」，限时/限次/一次性这些边界不该每个项目各写一遍
//! （此前 Pek.RCard、HlkProductTool、HLZX.EIP 等处各有实现）。
//!
//! ```no_run
//! use std::sync::OnceLock;
//! use dhrust::verify_code::{CodeStore, IssueError};
//!
//! fn codes() -> &'static CodeStore {
//!     static S: OnceLock<CodeStore> = OnceLock::new();
//!     S.get_or_init(|| CodeStore::new(300, 60, 5))
//! }
//!
//! fn send(mobile: &str) -> Result<String, String> {
//!     match codes().issue(mobile) {
//!         Ok(code) => Ok(code),
//!         Err(IssueError::TooSoon(secs)) => Err(format!("请 {secs} 秒后再试")),
//!         Err(IssueError::Busy) => Err("验证码服务忙".to_string()),
//!     }
//! }
//! # let _ = send("13800138000");
//! ```
//!
//! 设计要点：
//! - **纯内存**（进程重启即失效），够用于单实例；多实例共享请自行换成 Redis 方案；
//! - `issue` 顺手清理过期项，量级很小（每个目标一条），不会成为热点；
//! - 校验**通过即失效**；错误累计到上限也失效（防暴力猜 6 位）。

use std::collections::HashMap;
use std::sync::Mutex;

/// 一条验证码记录。
#[derive(Debug, Clone)]
struct Entry {
    code: String,
    /// 下发时间（Unix 秒）
    at: i64,
    /// 已错误次数
    tries: u32,
}

/// 下发失败原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssueError {
    /// 距上次下发不足 `resend_secs`，需等待的秒数
    TooSoon(i64),
    /// 内部锁不可用（有线程 panic 过）
    Busy,
}

impl std::fmt::Display for IssueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IssueError::TooSoon(secs) => write!(f, "请 {secs} 秒后再试"),
            IssueError::Busy => write!(f, "验证码服务忙"),
        }
    }
}

impl std::error::Error for IssueError {}

/// 验证码仓库：一组策略（TTL / 重发间隔 / 错误上限）+ 一个进程内 Map。
///
/// 一般用 `static` 全局一份；不同业务（登录 / 注册 / 改密）用**不同的 key 前缀**区分即可
/// （本类型不关心 key 语义）。
pub struct CodeStore {
    ttl_secs: i64,
    resend_secs: i64,
    max_tries: u32,
    map: Mutex<HashMap<String, Entry>>,
}

impl CodeStore {
    /// 构造（放 `static` 请包一层 `OnceLock`，见模块文档）。
    pub fn new(ttl_secs: i64, resend_secs: i64, max_tries: u32) -> Self {
        Self {
            ttl_secs: ttl_secs.max(1),
            resend_secs: resend_secs.max(0),
            max_tries: max_tries.max(1),
            map: Mutex::new(HashMap::new()),
        }
    }

    /// 有效期（秒）。
    pub fn ttl_secs(&self) -> i64 {
        self.ttl_secs
    }

    /// 重发间隔（秒）。
    pub fn resend_secs(&self) -> i64 {
        self.resend_secs
    }

    /// 生成一个新验证码并存下来（长度 6 位数字）。
    pub fn issue(&self, key: &str) -> Result<String, IssueError> {
        let code = numeric_code(6);
        self.issue_with(key, &code)?;
        Ok(code)
    }

    /// 存一个**指定的**验证码（对接第三方短信模板时，验证码往往由服务商生成）。
    pub fn issue_with(&self, key: &str, code: &str) -> Result<(), IssueError> {
        let now = crate::times::gettimestamp() as i64;
        let mut map = self.map.lock().map_err(|_| IssueError::Busy)?;
        // 顺手清理过期项（每个目标一条，成本可忽略）
        map.retain(|_, e| now - e.at < self.ttl_secs);
        if let Some(e) = map.get(key) {
            let wait = self.resend_secs - (now - e.at);
            if wait > 0 {
                return Err(IssueError::TooSoon(wait));
            }
        }
        map.insert(
            key.to_string(),
            Entry {
                code: code.trim().to_string(),
                at: now,
                tries: 0,
            },
        );
        Ok(())
    }

    /// 校验：通过即失效；超过错误上限也失效。
    pub fn check(&self, key: &str, code: &str) -> bool {
        let code = code.trim();
        if code.is_empty() {
            return false;
        }
        let now = crate::times::gettimestamp() as i64;
        let Ok(mut map) = self.map.lock() else {
            return false;
        };
        let Some(entry) = map.get_mut(key) else {
            return false;
        };
        if now - entry.at >= self.ttl_secs {
            map.remove(key);
            return false;
        }
        if entry.code == code {
            map.remove(key);
            return true;
        }
        entry.tries += 1;
        if entry.tries >= self.max_tries {
            map.remove(key);
        }
        false
    }

    /// 作废某个目标（发送失败时调用，允许立即重试）。
    pub fn remove(&self, key: &str) {
        if let Ok(mut map) = self.map.lock() {
            map.remove(key);
        }
    }

    /// 清全部（测试/停用功能时用）。
    pub fn clear(&self) {
        if let Ok(mut map) = self.map.lock() {
            map.clear();
        }
    }

    /// 清理过期项，返回清理条数。
    pub fn purge(&self) -> usize {
        let now = crate::times::gettimestamp() as i64;
        let Ok(mut map) = self.map.lock() else {
            return 0;
        };
        let before = map.len();
        map.retain(|_, e| now - e.at < self.ttl_secs);
        before - map.len()
    }

    /// 当前未过期的记录数（监控/测试用）。
    pub fn len(&self) -> usize {
        let now = crate::times::gettimestamp() as i64;
        let Ok(map) = self.map.lock() else {
            return 0;
        };
        map.values().filter(|e| now - e.at < self.ttl_secs).count()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 生成 `len` 位数字验证码（安全随机，按键值拒绝采样避免模偏置）。
pub fn numeric_code(len: usize) -> String {
    let len = len.clamp(1, 32);
    let mut out = String::with_capacity(len);
    while out.len() < len {
        for b in crate::random::bytes(len * 2).iter().copied() {
            if b >= 250 {
                continue; // 250..=255 丢弃，保证 0~9 均匀
            }
            out.push(char::from(b'0' + b % 10));
            if out.len() == len {
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_code_has_expected_length_and_digits() {
        for _ in 0..50 {
            let c = numeric_code(6);
            assert_eq!(c.len(), 6);
            assert!(c.chars().all(|x| x.is_ascii_digit()), "{c}");
        }
        assert_eq!(numeric_code(4).len(), 4);
    }

    #[test]
    fn check_is_one_shot() {
        let store = CodeStore::new(300, 0, 5);
        let code = store.issue("u1").expect("下发");
        assert_eq!(store.len(), 1);
        assert!(!store.check("u1", "000000") || code == "000000");
        assert!(store.check("u1", &code), "正确验证码应通过");
        assert!(!store.check("u1", &code), "通过后应失效");
        assert!(store.is_empty());
    }

    #[test]
    fn resend_interval_blocks_immediately() {
        let store = CodeStore::new(300, 60, 5);
        store.issue("u2").expect("首次");
        match store.issue("u2") {
            Err(IssueError::TooSoon(secs)) => assert!(secs > 0 && secs <= 60, "{secs}"),
            other => panic!("应被重发间隔拦住：{other:?}"),
        }
        // 不同 key 互不影响
        assert!(store.issue("u3").is_ok());
    }

    #[test]
    fn too_many_tries_invalidates() {
        let store = CodeStore::new(300, 0, 3);
        store.issue_with("u4", "123456").expect("写入");
        assert!(!store.check("u4", "111111"));
        assert!(!store.check("u4", "222222"));
        assert!(!store.check("u4", "333333"));
        // 达到上限后连正确码也不再通过
        assert!(!store.check("u4", "123456"));
        assert!(store.is_empty());
    }

    #[test]
    fn expired_code_is_rejected_and_purged() {
        let store = CodeStore::new(1, 0, 5);
        store.issue_with("u5", "654321").expect("写入");
        // 直接改时间不可行（内部私有），用 ttl=1 + 睡 1.1 秒确认过期
        std::thread::sleep(std::time::Duration::from_millis(1100));
        assert!(!store.check("u5", "654321"), "过期应拒绝");
        assert!(!store.check("u5", "654321"), "记录应已删除");
    }

    #[test]
    fn remove_allows_immediate_retry() {
        let store = CodeStore::new(300, 60, 5);
        store.issue("u6").expect("首次");
        store.remove("u6");
        assert!(store.issue("u6").is_ok(), "作废后可立即重发");
    }

    #[test]
    fn purge_drops_only_expired() {
        let store = CodeStore::new(1, 0, 5);
        store.issue_with("a", "111111").expect("写入");
        store.issue_with("b", "222222").expect("写入");
        assert_eq!(store.len(), 2);
        std::thread::sleep(std::time::Duration::from_millis(1100));
        assert_eq!(store.purge(), 2, "两条都已过期");
        assert_eq!(store.len(), 0);
        // 再写一条：仍然可用
        store.issue_with("c", "333333").expect("写入");
        assert_eq!(store.len(), 1);
    }
}
