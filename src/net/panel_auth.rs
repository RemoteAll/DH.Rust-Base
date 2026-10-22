//! 面板鉴权助手（Pek 系 Web 面板通用）：鉴权级别、Bearer 解析、客户端 IP、回环判定、面板令牌表。
//!
//! 收编自 Pek.RAgent 与 HlkProductTool 两处逐行同款实现（2026-10-03 收拢轮）；
//! 面板登录限流见 [`crate::net::login_guard`]。
//!
//! # 鉴权级别（对齐 C# `AuthLevel`）
//!
//! - `None` 全部放行；`LocalOnly`（默认）本机回环免鉴权、远程需令牌；`Full` 一律需令牌。
//! - 判定见 [`allows`]（令牌有效性由调用方提供，如 [`TokenStore::validate`]）。

use std::collections::HashMap;
use std::sync::Mutex;

use crate::net::router::Ctx;

/// 面板鉴权级别（对齐 Pek.RAgent / C# `AuthLevel`）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AuthLevel {
    /// 不鉴权
    None,
    /// 本机免鉴权，远程需鉴权（默认）
    LocalOnly,
    /// 全部鉴权
    Full,
}

impl AuthLevel {
    /// 解析配置文本（大小写不敏感；未知或空值回退 `LocalOnly`，与 C# 一致）。
    pub fn parse(text: &str) -> AuthLevel {
        match text.trim().to_ascii_lowercase().as_str() {
            "none" => AuthLevel::None,
            "full" => AuthLevel::Full,
            _ => AuthLevel::LocalOnly,
        }
    }

    /// 配置文本形式。
    pub fn as_str(self) -> &'static str {
        match self {
            AuthLevel::None => "None",
            AuthLevel::LocalOnly => "LocalOnly",
            AuthLevel::Full => "Full",
        }
    }
}

/// 解析 `Authorization` 头的 Bearer 令牌（纯函数，便于测试）。
pub fn parse_bearer(header: Option<&str>) -> Option<String> {
    let auth = header?;
    let prefix = "Bearer ";
    if auth.len() <= prefix.len() || !auth[..prefix.len()].eq_ignore_ascii_case(prefix) {
        return None;
    }
    let token = auth[prefix.len()..].trim();
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

/// 从请求头解析 Bearer 令牌。
pub fn bearer_token(ctx: &Ctx) -> Option<String> {
    parse_bearer(ctx.req.header("Authorization"))
}

/// 解析 `IP:Port` 文本为纯 IP（去端口/方括号；无地址返回 `"unknown"`；纯函数，便于测试）。
pub fn ip_of(addr: Option<&str>) -> String {
    match addr {
        Some(addr) => match addr.rsplit_once(':') {
            Some((host, _)) => host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .to_string(),
            None => addr.to_string(),
        },
        None => "unknown".to_string(),
    }
}

/// 客户端 IP（无端口；`unknown` 表示无法识别）。
pub fn client_ip(ctx: &Ctx) -> String {
    ip_of(ctx.req.remote_addr.as_deref())
}

/// 是否本机回环地址（`127.0.0.0/8`、`::1` 及 IPv4-mapped 形式）。
pub fn is_loopback(ip: &str) -> bool {
    ip.starts_with("127.") || ip == "::1" || ip.starts_with("::ffff:127.")
}

/// 请求鉴权：按鉴权级别判定。
///
/// - `None`：全部放行；
/// - `LocalOnly`：本机回环免鉴权，其余需有效令牌；
/// - `Full`：一律校验令牌。
///
/// 令牌有效性由 `token_ok` 提供（如 `|t| store.validate(t)`）。
pub fn allows(ctx: &Ctx, level: AuthLevel, token_ok: impl Fn(&str) -> bool) -> bool {
    if level == AuthLevel::None {
        return true;
    }
    if level == AuthLevel::LocalOnly && is_loopback(&client_ip(ctx)) {
        return true;
    }
    match bearer_token(ctx) {
        Some(token) => token_ok(&token),
        None => false,
    }
}

/// 面板令牌表容量上限（`restore` 导入时的兜底，正常使用远低于此值）。
pub const MAX_TOKENS: usize = 512;

/// 面板令牌表（内存态；token → (到期毫秒, 附加数据)）。
///
/// 泛型 `T` 承载会话主体（用户/权限快照等；默认 `()` 即纯令牌表，
/// 覆盖“只关心 token 是否有效”的场景）。单表合一后：过期/吊销即绑定数据失效，
/// 不会出现「主体表残留」的双表同步问题。
///
/// 收编自 Pek.RAgent 与 HlkProductTool 两处同款实现（2026-10-03）；
/// 2026-10-09 泛型化，会话主体随令牌表合并（Pek.RAgent / HlkProductTool /
/// Pek.RPanlServer 三处「令牌表 + 会话表」双表模式收编）。
pub struct TokenStore<T = ()> {
    /// 有效期（毫秒）
    ttl_ms: i64,
    /// 令牌表（token → (到期毫秒, 附加数据)）
    tokens: Mutex<HashMap<String, (i64, T)>>,
}

impl<T> Default for TokenStore<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> TokenStore<T> {
    /// 默认有效期 24 小时。
    pub fn new() -> Self {
        Self::with_ttl_ms(24 * 3_600_000)
    }

    /// 自定义有效期（毫秒）。
    pub fn with_ttl_ms(ttl_ms: i64) -> Self {
        TokenStore {
            ttl_ms,
            tokens: Mutex::new(HashMap::new()),
        }
    }

    /// 签发令牌并绑定数据（安全随机；顺带清理过期令牌）。
    pub fn issue_with(&self, value: T) -> String {
        let token = crate::random::token();
        let now = crate::times::getmilltimestamp() as i64;
        let mut tokens = self.tokens.lock().unwrap();
        tokens.retain(|_, (expire, _)| *expire > now);
        tokens.insert(token.clone(), (now + self.ttl_ms, value));
        token
    }

    /// 校验令牌（过期即清除）。
    pub fn validate(&self, token: &str) -> bool {
        if token.is_empty() {
            return false;
        }

        let now = crate::times::getmilltimestamp() as i64;
        let mut tokens = self.tokens.lock().unwrap();
        match tokens.get(token) {
            Some((expire, _)) if *expire > now => true,
            Some(_) => {
                tokens.remove(token);
                false
            }
            None => false,
        }
    }

    /// 吊销令牌（含绑定数据）。
    pub fn revoke(&self, token: &str) {
        if !token.is_empty() {
            self.tokens.lock().unwrap().remove(token);
        }
    }

    /// 有效期（毫秒）。
    pub fn ttl_ms(&self) -> i64 {
        self.ttl_ms
    }

    /// 清空全部令牌（改密码/「退出所有设备」后统一失效）；返回清除数量。
    pub fn clear(&self) -> usize {
        let mut tokens = self.tokens.lock().unwrap();
        let count = tokens.len();
        tokens.clear();
        count
    }

    /// 撤销除 `keep` 之外的全部令牌（保留当前会话）；返回移除数量。
    ///
    /// `keep` 为空等价于 [`TokenStore::clear`]。
    pub fn revoke_all_except(&self, keep: &str) -> usize {
        let mut tokens = self.tokens.lock().unwrap();
        let before = tokens.len();
        if keep.is_empty() {
            tokens.clear();
        } else {
            tokens.retain(|token, _| token == keep);
        }
        before - tokens.len()
    }

    /// 就地更新令牌绑定数据（存在且未过期才执行；返回是否命中）。
    pub fn update(&self, token: &str, f: impl FnOnce(&mut T)) -> bool {
        if token.is_empty() {
            return false;
        }
        let now = crate::times::getmilltimestamp() as i64;
        let mut tokens = self.tokens.lock().unwrap();
        match tokens.get_mut(token) {
            Some((expire, value)) if *expire > now => {
                f(value);
                true
            }
            Some(_) => {
                tokens.remove(token);
                false
            }
            None => false,
        }
    }

    /// 遍历刷新全部令牌绑定数据：回调返回 `Some` 换新值、`None` 移除（被踢）；
    /// 返回被移除的数量（过期项一并清理，不计入）。
    pub fn refresh_all(&self, mut f: impl FnMut(&T) -> Option<T>) -> usize {
        let now = crate::times::getmilltimestamp() as i64;
        let mut tokens = self.tokens.lock().unwrap();
        let before = tokens.len();
        tokens.retain(|_, (expire, value)| {
            if *expire <= now {
                return false;
            }
            match f(value) {
                Some(fresh) => {
                    *value = fresh;
                    true
                }
                None => false,
            }
        });
        before - tokens.len()
    }

    /// 条件吊销：回调为 `true` 的令牌移除；返回被移除的数量。
    pub fn revoke_where(&self, mut f: impl FnMut(&T) -> bool) -> usize {
        let mut tokens = self.tokens.lock().unwrap();
        let before = tokens.len();
        tokens.retain(|_, (_, value)| !f(value));
        before - tokens.len()
    }
}

impl<T: Clone> TokenStore<T> {
    /// 取令牌绑定数据（过期即清除；`None` = 无效）。
    pub fn get(&self, token: &str) -> Option<T> {
        if token.is_empty() {
            return None;
        }
        let now = crate::times::getmilltimestamp() as i64;
        let mut tokens = self.tokens.lock().unwrap();
        match tokens.get(token) {
            Some((expire, value)) if *expire > now => Some(value.clone()),
            Some(_) => {
                tokens.remove(token);
                None
            }
            None => None,
        }
    }

    /// 滑动续期：令牌仍有效且**剩余时间不足一半**时，把到期时间重置为 `now + ttl`。
    ///
    /// 返回 `true` 表示到期时间已改动（持久化调用方据此落盘）。
    /// 只在剩余时间过半时才续期，避免每个请求都触发写文件；
    /// 效果是「一直在用就一直不过期，连续 `ttl/2` 不用才失效」。
    pub fn touch(&self, token: &str) -> bool {
        if token.is_empty() {
            return false;
        }
        let now = crate::times::getmilltimestamp() as i64;
        let mut tokens = self.tokens.lock().unwrap();
        match tokens.get_mut(token) {
            Some((expire, _)) if *expire > now => {
                if *expire - now <= self.ttl_ms / 2 {
                    *expire = now + self.ttl_ms;
                    true
                } else {
                    false
                }
            }
            Some(_) => {
                tokens.remove(token);
                false
            }
            None => false,
        }
    }

    /// 导出**未过期**令牌快照 `(令牌, 到期毫秒, 绑定数据)`。
    ///
    /// 供调用方序列化落盘（进程重启后 [`TokenStore::restore`] 恢复）。
    pub fn snapshot(&self) -> Vec<(String, i64, T)> {
        let now = crate::times::getmilltimestamp() as i64;
        let tokens = self.tokens.lock().unwrap();
        tokens
            .iter()
            .filter(|(_, (expire, _))| *expire > now)
            .map(|(token, (expire, value))| (token.clone(), *expire, value.clone()))
            .collect()
    }

    /// 导入令牌快照（过期项忽略，与现有表合并）；返回新增条数。
    ///
    /// 表容量上限 [`MAX_TOKENS`]，超出时优先丢弃最早到期的项（防损坏文件把内存撑爆）。
    pub fn restore(&self, items: Vec<(String, i64, T)>) -> usize {
        let now = crate::times::getmilltimestamp() as i64;
        let mut tokens = self.tokens.lock().unwrap();
        let mut added = 0usize;
        for (token, expire, value) in items {
            if token.is_empty() || expire <= now {
                continue;
            }
            if tokens.insert(token, (expire, value)).is_none() {
                added += 1;
            }
        }
        if tokens.len() > MAX_TOKENS {
            let mut entries: Vec<(String, i64)> = tokens
                .iter()
                .map(|(token, (expire, _))| (token.clone(), *expire))
                .collect();
            entries.sort_by_key(|(_, expire)| *expire);
            let drop_count = tokens.len() - MAX_TOKENS;
            for (token, _) in entries.into_iter().take(drop_count) {
                tokens.remove(&token);
            }
        }
        added
    }
}

impl<T: Default> TokenStore<T> {
    /// 签发令牌（无绑定数据场景；`T` 取默认值）。
    pub fn issue(&self) -> String {
        self.issue_with(T::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_level_parse_is_lenient() {
        assert_eq!(AuthLevel::parse("none"), AuthLevel::None);
        assert_eq!(AuthLevel::parse("FULL"), AuthLevel::Full);
        assert_eq!(AuthLevel::parse(""), AuthLevel::LocalOnly);
        assert_eq!(AuthLevel::parse("whatever"), AuthLevel::LocalOnly);
        assert_eq!(AuthLevel::None.as_str(), "None");
        assert_eq!(AuthLevel::Full.as_str(), "Full");
    }

    #[test]
    fn bearer_parsing() {
        assert_eq!(parse_bearer(None), None);
        assert_eq!(parse_bearer(Some("Basic abc")), None);
        assert_eq!(parse_bearer(Some("Bearer ")), None);
        assert_eq!(
            parse_bearer(Some("bearer t-123 ")),
            Some("t-123".to_string())
        );
        assert_eq!(
            parse_bearer(Some("Bearer t-123")),
            Some("t-123".to_string())
        );
    }

    #[test]
    fn ip_parsing_strips_port_and_brackets() {
        assert_eq!(ip_of(Some("127.0.0.1:5600")), "127.0.0.1");
        assert_eq!(ip_of(Some("[::1]:5600")), "::1");
        assert_eq!(ip_of(Some("::ffff:127.0.0.1:1234")), "::ffff:127.0.0.1");
        assert_eq!(ip_of(Some("10.0.0.8")), "10.0.0.8");
        assert_eq!(ip_of(None), "unknown");
    }

    #[test]
    fn loopback_detection() {
        assert!(is_loopback("127.0.0.1"));
        assert!(is_loopback("127.8.8.8"));
        assert!(is_loopback("::1"));
        assert!(is_loopback("::ffff:127.0.0.1"));
        assert!(!is_loopback("192.168.1.5"));
        assert!(!is_loopback(""));
    }

    #[test]
    fn token_store_lifecycle() {
        let store: TokenStore = TokenStore::new();
        let token = store.issue();
        assert!(store.validate(&token));
        assert!(!store.validate("not-a-token"));
        store.revoke(&token);
        assert!(!store.validate(&token));
    }

    #[test]
    fn token_store_expires() {
        let store: TokenStore = TokenStore::with_ttl_ms(1);
        let token = store.issue();
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert!(!store.validate(&token), "过期令牌应失效");
    }

    #[test]
    fn token_store_binds_value() {
        let store: TokenStore<String> = TokenStore::new();
        let token = store.issue_with("alice".to_string());
        assert_eq!(store.get(&token), Some("alice".to_string()));
        assert_eq!(store.get(""), None);
        assert!(store.update(&token, |v| *v = "bob".to_string()));
        assert_eq!(store.get(&token), Some("bob".to_string()));
        assert!(!store.update("not-a-token", |v| *v = "x".to_string()));
        store.revoke(&token);
        assert_eq!(store.get(&token), None);
    }

    #[test]
    fn token_store_refresh_and_revoke_where() {
        let store: TokenStore<i32> = TokenStore::new();
        let a = store.issue_with(1);
        let b = store.issue_with(2);
        // refresh_all：1 → 刷新为 10；2 → 移除（None）
        let removed = store.refresh_all(|v| if *v == 1 { Some(10) } else { None });
        assert_eq!(removed, 1);
        assert_eq!(store.get(&a), Some(10));
        assert_eq!(store.get(&b), None, "被移除的令牌应失效");
        // revoke_where：移除值 = 10 的令牌
        let c = store.issue_with(2);
        let removed = store.revoke_where(|v| *v == 2);
        assert_eq!(removed, 1);
        assert_eq!(store.get(&c), None);
        assert_eq!(store.get(&a), Some(10), "不匹配的令牌保留");
    }

    #[test]
    fn token_store_snapshot_restore_and_touch() {
        // 快照只含未过期项；恢复后可直接取到原绑定数据
        let store: TokenStore<u32> = TokenStore::with_ttl_ms(1000);
        let token = store.issue_with(7);
        let snap = store.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].0, token);

        let other: TokenStore<u32> = TokenStore::with_ttl_ms(1000);
        let now = crate::times::getmilltimestamp() as i64;
        let added = other.restore(vec![
            ("t-live".to_string(), now + 100, 9u32),
            ("t-dead".to_string(), now - 1, 8u32),
        ]);
        assert_eq!(added, 1, "过期项不导入");
        assert_eq!(other.get("t-live"), Some(9));
        assert_eq!(other.get("t-dead"), None);
        // 剩余 100ms ≤ ttl/2（500ms）→ 续期为整段 ttl
        assert!(other.touch("t-live"));
        assert!(!other.touch("t-live"), "刚续期后剩余充足，不再重复续期");
        assert!(!other.touch("t-missing"));
        assert_eq!(other.revoke_all_except("t-live"), 0);
        assert_eq!(other.clear(), 1);
        assert!(other.snapshot().is_empty());
    }

    #[test]
    fn token_store_revoke_all_except() {
        let store: TokenStore<()> = TokenStore::new();
        let a = store.issue();
        let b = store.issue();
        assert_eq!(store.revoke_all_except(&a), 1);
        assert!(store.validate(&a));
        assert!(!store.validate(&b));
        assert_eq!(store.revoke_all_except(""), 1, "空 keep = 全部清除");
        assert!(!store.validate(&a));
    }
}
