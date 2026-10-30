//! 挑战式登录（`nonce` + `response`）：**密码与口令散列都不出网**。
//!
//! # 为什么需要它
//! 「IP + 端口」直连部署（Agent / 管理端）**常常没有 TLS**——此时若前端只做 MD5，
//! 抓到的摘要可直接重放登录（pass-the-hash）。挑战式的应答**一次一密**，抓到也无法重放。
//!
//! # 口径（前后端必须逐字节一致）
//! ```text
//! digest   = SHA256(salt + ":" + 明文密码)      ← 等于库里存的口令散列
//! response = SHA256(nonce + ":" + digest)
//! ```
//! 服务端下发的 `salt` 与库中一致；`expect`（= 库里的散列）**只存在服务端**，
//! 校验时用它重算 `response` 比对——客户端无法「自带答案」。
//!
//! # 安全边界
//! - ✅ 密码不出网、摘要不可重放（`nonce` 一次性 + TTL）、防用户名枚举（用户不存在也发随机挑战）
//! - ❌ **不防数据库泄露**（拿到 `expect` 即可登录）→ 存储仍需加盐散列；有 TLS 时仍应开 TLS
//!
//! # 典型用法
//! ```ignore
//! // ① 下发挑战（用户不存在时用随机 salt/expect，避免枚举）
//! let (salt, expect) = store_credential(user).unwrap_or_else(random_fake);
//! let nonce = challenges.issue(user, &salt, &expect);
//! // → 返回 { nonce, salt }
//!
//! // ② 校验（一次性消费）
//! if challenges.verify(user, &nonce, &response) { /* 发令牌 */ }
//! ```

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::sign::sha256_hex;

/// 默认挑战有效期（秒）：够慢网络完成一次登录，又不给重放留窗口。
pub const DEFAULT_TTL_SECS: i64 = 120;

/// 客户端应答：`SHA256(nonce + ":" + digest)`。
pub fn response(nonce: &str, digest: &str) -> String {
    sha256_hex(format!("{nonce}:{digest}").as_bytes())
}

/// 前端摘要：`SHA256(salt + ":" + 明文密码)`（与 [`crate::sign::salted_sha256_hex`] 同值）。
pub fn digest(salt: &str, password: &str) -> String {
    crate::sign::salted_sha256_hex(salt, password)
}

struct Entry {
    user: String,
    /// 库中存的口令散列（等于前端算出的 `digest`）
    expect: String,
    /// 过期时刻（Unix 毫秒）
    expire: i64,
}

/// 挑战仓库（内存态、有界、接 TTL 自动清理）。
///
/// 每个实例独立；进程内通常只需一个（放进 App/State）。并发安全（内部 `Mutex`）。
pub struct ChallengeStore {
    ttl_ms: i64,
    inner: Mutex<HashMap<String, Entry>>,
}

impl ChallengeStore {
    /// 以默认 TTL 构建。
    pub fn new() -> Self {
        Self::with_ttl_secs(DEFAULT_TTL_SECS)
    }

    /// 自定义 TTL（秒；`<=0` 回退默认值）。
    pub fn with_ttl_secs(ttl_secs: i64) -> Self {
        Self {
            ttl_ms: if ttl_secs > 0 {
                ttl_secs * 1000
            } else {
                DEFAULT_TTL_SECS * 1000
            },
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// 下发挑战：登记 `(nonce → 该用户的 expect)`，返回 `nonce`。
    ///
    /// `salt` 仅用于调用方回传给前端，本函数不关心；
    /// **用户不存在时也要调用**（传随机 `expect`），以保持响应形态一致、防用户名枚举。
    pub fn issue(&self, user: &str, expect: &str) -> String {
        let nonce = crate::random::hex(16);
        let now = now_ms();
        let mut map = match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        // 清理过期项（顺带限制容量：攻击者狂刷 challenge 也不会撑爆内存）
        map.retain(|_, e| e.expire > now);
        if map.len() > 4096 {
            map.clear();
        }
        map.insert(
            nonce.clone(),
            Entry {
                user: user.trim().to_string(),
                expect: expect.to_string(),
                expire: now + self.ttl_ms,
            },
        );
        nonce
    }

    /// 校验应答：**一次性消费**（无论成功失败都删除该 nonce）。
    pub fn verify(&self, user: &str, nonce: &str, resp: &str) -> bool {
        let nonce = nonce.trim();
        if nonce.is_empty() || resp.trim().is_empty() {
            return false;
        }
        let entry = {
            let mut map = match self.inner.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            map.remove(nonce)
        };
        let Some(entry) = entry else {
            return false;
        };
        if entry.expire <= now_ms() || !entry.user.eq_ignore_ascii_case(user.trim()) {
            return false;
        }
        // 等长逐字节比较，避免按字节提前返回泄露前缀信息
        let expect = response(nonce, &entry.expect);
        let (a, b) = (expect.as_bytes(), resp.trim().as_bytes());
        if a.len() != b.len() {
            return false;
        }
        let mut diff = 0u8;
        for (x, y) in a.iter().zip(b.iter()) {
            diff |= x ^ y;
        }
        diff == 0
    }

    /// 当前未过期的挑战数（诊断用）。
    pub fn len(&self) -> usize {
        let now = now_ms();
        match self.inner.lock() {
            Ok(g) => g.values().filter(|e| e.expire > now).count(),
            Err(p) => p.into_inner().values().filter(|e| e.expire > now).count(),
        }
    }

    /// 是否没有未过期挑战。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for ChallengeStore {
    fn default() -> Self {
        Self::new()
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with(expect: &str) -> (ChallengeStore, String, String) {
        let s = ChallengeStore::new();
        let salt = "ZZsalt0011ab";
        let nonce = s.issue("admin", expect);
        (s, nonce, salt.to_string())
    }

    #[test]
    fn correct_response_passes_and_is_one_shot() {
        let expect = digest("ZZsalt0011ab", "P@ssw0rd");
        let (s, nonce, _) = store_with(&expect);
        let resp = response(&nonce, &digest("ZZsalt0011ab", "P@ssw0rd"));
        assert!(s.verify("admin", &nonce, &resp), "正确应答应通过");
        // 一次性：重放必须失败
        assert!(!s.verify("admin", &nonce, &resp), "同一 nonce 不能二次使用");
    }

    #[test]
    fn wrong_password_or_user_fails() {
        let expect = digest("sz", "P@ssw0rd");
        let s = ChallengeStore::new();
        let nonce = s.issue("admin", &expect);
        // 密码错 → digest 不同 → 应答不同
        let bad = response(&nonce, &digest("sz", "wrong"));
        assert!(!s.verify("admin", &nonce, &bad));

        // 用户名不符
        let nonce = s.issue("admin", &expect);
        let ok = response(&nonce, &digest("sz", "P@ssw0rd"));
        assert!(!s.verify("someone-else", &nonce, &ok));
    }

    #[test]
    fn expired_challenge_fails() {
        let expect = digest("sz", "p");
        let s = ChallengeStore::with_ttl_secs(-1); // 回退默认；直接改内部过期时间模拟过期
        let nonce = s.issue("admin", &expect);
        {
            let mut map = s.inner.lock().unwrap();
            if let Some(e) = map.get_mut(&nonce) {
                e.expire = now_ms() - 1;
            }
        }
        let resp = response(&nonce, &digest("sz", "p"));
        assert!(!s.verify("admin", &nonce, &resp), "过期挑战必须失败");
    }

    #[test]
    fn unknown_nonce_fails() {
        let s = ChallengeStore::new();
        assert!(!s.verify("admin", "deadbeef", "whatever"));
    }

    #[test]
    fn digest_matches_salted_sha256() {
        assert_eq!(
            digest("salt", "pwd"),
            crate::sign::salted_sha256_hex("salt", "pwd")
        );
    }
}
