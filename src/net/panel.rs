//! 单账号面板脚手架：登录 / 登出 / 当前用户 + 令牌 + 登录限流 + 会话持久化。
//!
//! ## 适用场景
//!
//! 管理面板只需要**一个账号**（账号写在配置文件里，不建用户表）时，
//! `login` / `logout` / `me` / 鉴权包装这几段代码在每个项目里逐字重复。
//! 本模块把它收成可复用组件（`Pek.RAgent` / `Pek.RPanlServer` / `HlktechIoT` /
//! `HlkProductTool` / `DHDeploy.Agent` 同一套）：
//!
//! ```ignore
//! let session = Arc::new(PanelSession::new("admin", "admin", AuthLevel::LocalOnly, 24 * 3600)
//!     .with_session_file(base.join("Data/panel-sessions.json")));
//! session.restore_sessions();                 // 重启/升级后浏览器不用重新登录
//! c = c.post("login", move |ctx| session.login(ctx));
//! c = c.get("status", panel::guarded(session.clone(), move |s, ctx| status(s, ctx)));
//! ```
//!
//! 权限级别语义见 [`AuthLevel`]（`None` 全放行 / `LocalOnly` 本机免登录 / `Full` 必须令牌）。
//!
//! ## 配置热重载
//!
//! 账号/密码/级别改配置后调 [`PanelSession::set_account`]，**已签发的令牌继续有效**
//! （避免改一次配置就把所有在线会话踢下线）；需要强制失效时调 [`PanelSession::revoke_all`]。

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use serde_json::json;

use super::controller::{json_body, json_error, json_result, ActionResult};
use super::login_guard::LoginGuard;
use super::panel_auth::{bearer_token, client_ip, is_loopback, AuthLevel, TokenStore};
use super::router::Ctx;

/// 默认令牌有效期（秒）：24 小时（与 `TokenStore::new` 一致）。
pub const DEFAULT_TOKEN_TTL_SECS: u64 = 24 * 3600;

/// 面板账号（配置文件里的一份凭据）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelAccount {
    /// 用户名
    pub user: String,
    /// 密码（配置文件明文；也可放哈希值由调用方自行比较前替换策略）
    pub password: String,
    /// 鉴权级别
    pub level: AuthLevel,
}

/// 会话落盘记录（令牌 + 过期毫秒时间戳 + 用户名）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct SessionRec {
    token: String,
    expire: i64,
    user: String,
}

/// 单账号面板会话管理器。
pub struct PanelSession {
    /// 当前账号（可热更新）。
    account: RwLock<PanelAccount>,
    /// Bearer 令牌表（token → 用户名）。
    tokens: TokenStore<String>,
    /// 登录失败限流（按 IP）。
    logins: LoginGuard,
    /// 会话落盘文件（`None` = 不持久化，进程重启即掉线）。
    session_file: RwLock<Option<PathBuf>>,
    /// 上次落盘时间（毫秒；滑动续期时节流写盘，避免每个请求都写文件）。
    saved_at: std::sync::atomic::AtomicI64,
}

impl PanelSession {
    /// 新建会话管理器。
    ///
    /// # 参数
    /// - `user`：用户名。
    /// - `password`：密码（明文比较；如需哈希校验，调用方可先自行改成哈希值再传入）。
    /// - `level`：鉴权级别。
    /// - `token_ttl_secs`：令牌有效期秒数（`0` = 用默认 24 小时）。
    ///
    /// # 返回值
    /// 会话管理器。
    pub fn new(
        user: impl Into<String>,
        password: impl Into<String>,
        level: AuthLevel,
        token_ttl_secs: u64,
    ) -> Self {
        let ttl_secs = if token_ttl_secs == 0 { DEFAULT_TOKEN_TTL_SECS } else { token_ttl_secs };
        Self {
            account: RwLock::new(PanelAccount {
                user: user.into(),
                password: password.into(),
                level,
            }),
            tokens: TokenStore::with_ttl_ms(ttl_secs as i64 * 1000),
            logins: LoginGuard::new(),
            session_file: RwLock::new(None),
            saved_at: std::sync::atomic::AtomicI64::new(0),
        }
    }

    /// 节流落盘：最多每 `interval_ms` 毫秒写一次（滑动续期时用）。
    ///
    /// # 参数
    /// - `interval_ms`：节流窗口（毫秒）。
    ///
    /// # 返回值
    /// 本次是否真的写了文件。
    pub fn save_sessions_throttled(&self, interval_ms: i64) -> bool {
        let now = crate::times::getmilltimestamp() as i64;
        if now - self.saved_at.load(std::sync::atomic::Ordering::Relaxed) < interval_ms {
            return false;
        }
        self.save_sessions();
        true
    }

    /// 设置会话落盘文件（链式调用；默认不持久化）。
    ///
    /// # 参数
    /// - `path`：JSON 文件路径（父目录会自动创建）。
    ///
    /// # 返回值
    /// `self`。
    pub fn with_session_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.session_file = RwLock::new(Some(path.into()));
        self
    }

    /// 当前账号快照。
    ///
    /// # 返回值
    /// 账号副本。
    pub fn account(&self) -> PanelAccount {
        match self.account.read() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// 热更新账号（配置改后调用；已签发令牌保持有效）。
    ///
    /// # 参数
    /// - `user`：新用户名。
    /// - `password`：新密码。
    /// - `level`：新鉴权级别。
    pub fn set_account(&self, user: impl Into<String>, password: impl Into<String>, level: AuthLevel) {
        let next = PanelAccount { user: user.into(), password: password.into(), level };
        match self.account.write() {
            Ok(mut guard) => *guard = next,
            Err(poisoned) => *poisoned.into_inner() = next,
        }
    }

    /// 是否仍是出厂默认口令（`admin`/`admin`）——面板横幅与启动日志用。
    ///
    /// # 返回值
    /// 默认口令为 `true`。
    pub fn uses_default_password(&self) -> bool {
        let account = self.account();
        account.user.trim().eq_ignore_ascii_case("admin") && account.password == "admin"
    }

    /// 令牌存储（需要额外操作时使用，如按用户吊销）。
    ///
    /// # 返回值
    /// 令牌表引用。
    pub fn tokens(&self) -> &TokenStore<String> {
        &self.tokens
    }

    /// 吊销全部令牌（改密后强制下线）。
    ///
    /// # 返回值
    /// 吊销数量。
    pub fn revoke_all(&self) -> usize {
        let count = self.tokens.revoke_where(|_| true);
        self.save_sessions();
        count
    }

    /// 判定当前请求的主体（`None` = 未认证）。
    ///
    /// `None`/`LocalOnly` 级别下本机回环免登录；`Full` 必须带有效令牌。
    ///
    /// # 参数
    /// - `ctx`：请求上下文。
    ///
    /// # 返回值
    /// 已认证用户名。
    pub fn principal(&self, ctx: &Ctx) -> Option<String> {
        let account = self.account();
        if let Some(token) = bearer_token(ctx) {
            if let Some(user) = self.tokens.get(token.trim()) {
                // 滑动续期：一直在用就一直有效；落盘节流 60s（重启后到期时间不倒退）
                self.tokens.touch(token.trim());
                self.save_sessions_throttled(60_000);
                return Some(user);
            }
        }
        match account.level {
            AuthLevel::None => {
                Some(if is_loopback(&client_ip(ctx)) { account.user } else { "anon".to_string() })
            }
            AuthLevel::LocalOnly => {
                if is_loopback(&client_ip(ctx)) {
                    Some(account.user)
                } else {
                    None
                }
            }
            AuthLevel::Full => None,
        }
    }

    /// 登录端点：`POST` JSON `{user, password}` → `{token, user, authLevel, defaultPassword}`。
    ///
    /// # 参数
    /// - `ctx`：请求上下文。
    ///
    /// # 返回值
    /// 动作结果。
    pub fn login(&self, ctx: &Ctx) -> ActionResult {
        let ip = client_ip(ctx);
        if self.logins.is_blocked(&ip) {
            return json_error(429, "尝试次数过多，请稍后再试");
        }
        let Some(body) = json_body(ctx) else {
            return json_error(400, "Missing body");
        };
        let user = body.get("user").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        let password = body.get("password").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if user.is_empty() || password.is_empty() {
            return json_error(400, "请输入用户名与密码");
        }
        let account = self.account();
        if !user.eq_ignore_ascii_case(account.user.trim()) || password != account.password {
            self.logins.record_failure(&ip);
            log::warn!("面板登录失败：user={user} ip={ip}");
            return json_error(401, "用户名或密码错误");
        }
        self.logins.record_success(&ip);
        let token = self.tokens.issue_with(account.user.clone());
        self.save_sessions();
        log::info!("面板登录成功：user={} ip={ip}", account.user);
        json_result(
            0,
            "",
            Some(json!({
                "token": token,
                "user": account.user,
                "authLevel": account.level.as_str(),
                "defaultPassword": self.uses_default_password(),
            })),
        )
    }

    /// 登出端点：吊销请求携带的令牌。
    ///
    /// # 参数
    /// - `ctx`：请求上下文。
    ///
    /// # 返回值
    /// 动作结果。
    pub fn logout(&self, ctx: &Ctx) -> ActionResult {
        if let Some(token) = bearer_token(ctx) {
            self.tokens.revoke(token.trim());
            self.save_sessions();
        }
        json_result(0, "已退出登录", None)
    }

    /// 当前用户端点：`{user, authLevel, defaultPassword}`。
    ///
    /// # 参数
    /// - `ctx`：请求上下文。
    ///
    /// # 返回值
    /// 动作结果。
    pub fn me(&self, ctx: &Ctx) -> ActionResult {
        match self.principal(ctx) {
            Some(user) => {
                let account = self.account();
                json_result(
                    0,
                    "",
                    Some(json!({
                        "user": user,
                        "authLevel": account.level.as_str(),
                        "defaultPassword": self.uses_default_password(),
                    })),
                )
            }
            None => json_error(401, "Unauthorized"),
        }
    }

    /// 恢复上次落盘的会话（启动时调用；文件缺失/损坏时静默跳过）。
    pub fn restore_sessions(&self) {
        let path = {
            let guard = match self.session_file.read() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            match guard.as_ref() {
                Some(path) => path.clone(),
                None => return,
            }
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        let Ok(list) = serde_json::from_str::<Vec<SessionRec>>(&text) else {
            log::warn!("面板会话文件解析失败（{}），忽略", path.display());
            return;
        };
        let items: Vec<(String, i64, String)> =
            list.into_iter().map(|r| (r.token, r.expire, r.user)).collect();
        let count = self.tokens.restore(items);
        if count > 0 {
            log::info!("面板会话已恢复 {count} 个（重启/升级后仍保持登录）：{}", path.display());
        }
    }

    /// 会话落盘（Unix 下权限收紧到 0600）；未配置文件路径时为空操作。
    pub fn save_sessions(&self) {
        let path = {
            let guard = match self.session_file.read() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            match guard.as_ref() {
                Some(path) => path.clone(),
                None => return,
            }
        };
        if let Some(dir) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(dir) {
                log::warn!("面板会话目录创建失败（{}）：{e}", dir.display());
                return;
            }
        }
        let list: Vec<SessionRec> = self
            .tokens
            .snapshot()
            .into_iter()
            .map(|(token, expire, user)| SessionRec { token, expire, user })
            .collect();
        let text = serde_json::to_string_pretty(&list).unwrap_or_else(|_| "[]".to_string());
        if let Err(e) = std::fs::write(&path, text) {
            log::warn!("面板会话写入失败（{}）：{e}", path.display());
            return;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(&path) {
                let mut perm = meta.permissions();
                perm.set_mode(0o600);
                let _ = std::fs::set_permissions(&path, perm);
            }
        }
        self.saved_at
            .store(crate::times::getmilltimestamp() as i64, std::sync::atomic::Ordering::Relaxed);
        let _: &Path = path.as_path();
    }
}

/// 受保护动作包装（认证 → 执行）：处理器内不用再判权。
///
/// # 参数
/// - `session`：会话管理器。
/// - `f`：业务处理器（第一个参数是会话本身，便于取当前账号）。
///
/// # 返回值
/// 可挂到 `Controller` 的动作闭包。
pub fn guarded<F>(
    session: Arc<PanelSession>,
    f: F,
) -> impl Fn(&Ctx) -> ActionResult + Send + Sync + 'static
where
    F: Fn(&PanelSession, &Ctx) -> ActionResult + Send + Sync + 'static,
{
    move |ctx| {
        if session.principal(ctx).is_none() {
            return json_error(401, "Unauthorized");
        }
        f(&session, ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::super::http::HttpRequest;
    use super::*;

    /// 构造请求（`remote_addr` 为空 = 非回环；`body` 用静态字节避免额外依赖）。
    fn request(method: &str, headers: Vec<(&str, &str)>, body: &'static [u8]) -> Ctx {
        let req = HttpRequest {
            method: method.to_string(),
            path: "/api/test".to_string(),
            query: String::new(),
            headers: headers
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body: hyper::body::Bytes::from_static(body),
            remote_addr: None,
        };
        Ctx::build(req)
    }

    fn body_text(outcome: ActionResult) -> String {
        match outcome {
            ActionResult::Response(resp) => String::from_utf8_lossy(&resp.body).to_string(),
            ActionResult::View { .. } => panic!("不应返回视图"),
        }
    }

    /// `Full` 级别下：无令牌 401，正确口令登录后令牌可用，登出后失效。
    #[test]
    fn full_level_login_logout_flow() {
        let session = PanelSession::new("admin", "secret", AuthLevel::Full, 3600);
        assert!(session.principal(&request("GET", vec![], b"")).is_none());

        // 错误口令：不得签发令牌
        let text = body_text(session.login(&request(
            "POST",
            vec![],
            br#"{"user":"admin","password":"bad"}"#,
        )));
        assert!(text.contains("401"), "{text}");
        assert!(session.tokens().snapshot().is_empty());

        // 正确口令 → 令牌
        let text = body_text(session.login(&request(
            "POST",
            vec![],
            br#"{"user":"admin","password":"secret"}"#,
        )));
        assert!(text.contains("\"token\""), "{text}");

        let token = session
            .tokens()
            .snapshot()
            .first()
            .map(|(t, _, _)| t.clone())
            .expect("应签发令牌");
        let auth = format!("Bearer {token}");
        assert_eq!(
            session.principal(&request("GET", vec![("authorization", &auth)], b"")).as_deref(),
            Some("admin")
        );

        // 登出后令牌失效
        let _ = session.logout(&request("POST", vec![("authorization", &auth)], b""));
        assert!(session
            .principal(&request("GET", vec![("authorization", &auth)], b""))
            .is_none());
    }

    /// `LocalOnly` 级别：非回环来源必需令牌；`None` 级别全放行。
    #[test]
    fn auth_levels_behave_as_documented() {
        let local = PanelSession::new("admin", "secret", AuthLevel::LocalOnly, 3600);
        assert!(local.principal(&request("GET", vec![], b"")).is_none());
        let open = PanelSession::new("admin", "secret", AuthLevel::None, 3600);
        assert!(open.principal(&request("GET", vec![], b"")).is_some());
    }

    /// 默认口令判定与热更新账号。
    #[test]
    fn default_password_and_hot_update() {
        let session = PanelSession::new("admin", "admin", AuthLevel::LocalOnly, 0);
        assert!(session.uses_default_password());
        session.set_account("root", "s3cret", AuthLevel::Full);
        assert!(!session.uses_default_password());
        assert_eq!(session.account().user, "root");
        assert_eq!(session.account().level, AuthLevel::Full);
    }
}
