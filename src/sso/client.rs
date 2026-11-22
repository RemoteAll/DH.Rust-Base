//! OAuth2 / OIDC **客户端**：本系统去登别人的 IdP（Pek 生态各 Rust 项目共用）。
//!
//! 三步：`authorize_url()` 让浏览器跳过去 → 回调里 `exchange_code()` 换令牌 →
//! `userinfo()` / `validate()` 拿用户或校验令牌。
//!
//! ## 为什么要放公共库
//!
//! 这段代码在各项目里被抄过（`HLZX.EIP/src/sso.rs` 原本就有一份），
//! 而**抄错都不报错**：表单编码少一个 `urlencode`（`redirect_uri` 里的 `&` 会被截断）、
//! 令牌放 query 而不是 `Authorization`（有的服务器会把令牌写进访问日志）、
//! `state` 不比对（等于放弃 CSRF 防护）。
//!
//! ## 安全要点（本模块已内置）
//!
//! - `state` 由调用方生成并**回跳时比对** —— 只提供工具，比对必须调用方做；
//! - `nonce` 写进授权请求、并可从 `id_token` 里读回来比对（防 id_token 重放）；
//! - 支持 **PKCE(S256)**：见 [`crate::sso::pkce`]，`authorize_url_pkce()` 一次性给出
//!   挑战与 verifier，`exchange_code_pkce()` 负责回传；
//! - 令牌一律走 `Authorization: Bearer`，**不放 query**（避免进反代日志）。

use std::time::Duration;

use serde_json::Value;

use super::pkce;
use crate::web::url_encode;

/// 客户端配置。
#[derive(Debug, Clone)]
pub struct SsoClientOptions {
    pub base_url: String,
    pub client_id: String,
    pub client_secret: String,
    pub redirect_uri: String,
    pub scopes: Vec<String>,
    /// 出站请求超时（秒）
    pub timeout_secs: u64,
}

impl SsoClientOptions {
    pub fn new(
        base_url: impl Into<String>,
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
        redirect_uri: impl Into<String>,
    ) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            redirect_uri: redirect_uri.into(),
            scopes: vec!["openid".to_string()],
            timeout_secs: 15,
        }
    }

    pub fn with_scopes(mut self, scopes: &[&str]) -> Self {
        self.scopes = scopes.iter().map(|s| s.to_string()).collect();
        self
    }
}

/// 一次 PKCE 授权要用到的两半：`challenge` 给浏览器，`verifier` 留在自己手里。
#[derive(Debug, Clone)]
pub struct PkcePair {
    pub verifier: String,
    pub challenge: String,
}

impl PkcePair {
    /// 生成一对（verifier 默认 64 位，规范要求 43~128）。
    pub fn generate() -> Self {
        let verifier = pkce::random_verifier(64);
        let challenge = pkce::challenge_s256(&verifier);
        Self { verifier, challenge }
    }
}

/// 换票结果（保留原始 JSON，另给几个常用取值器）。
#[derive(Debug, Clone)]
pub struct SsoTokens(pub Value);

impl SsoTokens {
    pub fn access_token(&self) -> &str {
        self.0.get("access_token").and_then(|v| v.as_str()).unwrap_or("")
    }

    pub fn refresh_token(&self) -> &str {
        self.0.get("refresh_token").and_then(|v| v.as_str()).unwrap_or("")
    }

    /// OIDC 模式才有（HS256 的兼容口径通常不发 id_token）。
    pub fn id_token(&self) -> &str {
        self.0.get("id_token").and_then(|v| v.as_str()).unwrap_or("")
    }

    pub fn scope(&self) -> &str {
        self.0.get("scope").and_then(|v| v.as_str()).unwrap_or("")
    }

    /// `expires_in`（秒）；缺省 0。
    pub fn expires_in(&self) -> i64 {
        self.0.get("expires_in").and_then(|v| v.as_i64()).unwrap_or(0)
    }

    pub fn raw(&self) -> &Value {
        &self.0
    }
}

pub struct SsoClient {
    opt: SsoClientOptions,
}

impl SsoClient {
    pub fn new(opt: SsoClientOptions) -> Self {
        Self { opt }
    }

    pub fn options(&self) -> &SsoClientOptions {
        &self.opt
    }

    /// 授权地址（不带 PKCE / nonce；老 IdP 或纯后端流程用）。
    pub fn authorize_url(&self, state: &str) -> String {
        self.build_authorize(state, None, None)
    }

    /// 授权地址 + **PKCE**（现代客户端默认要求）。返回 `(url, pair)`：
    /// `url` 给浏览器，`pair.verifier` 自己存着、换票时回传。
    pub fn authorize_url_pkce(&self, state: &str, nonce: Option<&str>) -> (String, PkcePair) {
        let pair = PkcePair::generate();
        let url = self.build_authorize(state, nonce, Some(&pair.challenge));
        (url, pair)
    }

    fn build_authorize(&self, state: &str, nonce: Option<&str>, challenge: Option<&str>) -> String {
        let mut url = format!(
            "{}/sso/authorize?client_id={}&redirect_uri={}&response_type=code&scope={}&state={}",
            self.opt.base_url,
            url_encode(&self.opt.client_id),
            url_encode(&self.opt.redirect_uri),
            url_encode(&self.opt.scopes.join(" ")),
            url_encode(state)
        );
        if let Some(n) = nonce.filter(|s| !s.is_empty()) {
            url.push_str("&nonce=");
            url.push_str(&url_encode(n));
        }
        if let Some(c) = challenge.filter(|s| !s.is_empty()) {
            url.push_str("&code_challenge=");
            url.push_str(&url_encode(c));
            url.push_str("&code_challenge_method=S256");
        }
        url
    }

    /// 用授权码换令牌（`POST /sso/token`，表单编码）。
    pub fn exchange_code(&self, code: &str) -> Result<SsoTokens, String> {
        self.exchange_code_inner(code, None)
    }

    /// 换票 + 回传 PKCE `code_verifier`。
    pub fn exchange_code_pkce(&self, code: &str, verifier: &str) -> Result<SsoTokens, String> {
        self.exchange_code_inner(code, Some(verifier))
    }

    fn exchange_code_inner(&self, code: &str, verifier: Option<&str>) -> Result<SsoTokens, String> {
        let mut body = format!(
            "grant_type=authorization_code&client_id={}&client_secret={}&code={}&redirect_uri={}",
            url_encode(&self.opt.client_id),
            url_encode(&self.opt.client_secret),
            url_encode(code),
            url_encode(&self.opt.redirect_uri)
        );
        if let Some(v) = verifier.filter(|s| !s.is_empty()) {
            body.push_str("&code_verifier=");
            body.push_str(&url_encode(v));
        }
        self.post_form("/sso/token", &body).map(SsoTokens)
    }

    /// 刷新令牌。
    pub fn refresh(&self, refresh_token: &str) -> Result<SsoTokens, String> {
        let body = format!(
            "grant_type=refresh_token&client_id={}&client_secret={}&refresh_token={}",
            url_encode(&self.opt.client_id),
            url_encode(&self.opt.client_secret),
            url_encode(refresh_token)
        );
        self.post_form("/sso/token", &body).map(SsoTokens)
    }

    /// 取用户信息（`Authorization: Bearer`，**不放 query**）。
    pub fn userinfo(&self, access_token: &str) -> Result<Value, String> {
        let url = format!("{}/sso/userinfo", self.opt.base_url);
        self.get_json(&url, access_token)
    }

    /// 只校验令牌是否有效（资源服务器用）。
    pub fn validate(&self, access_token: &str) -> Result<Value, String> {
        let url = format!("{}/sso/auth", self.opt.base_url);
        self.get_json(&url, access_token)
    }

    /// 注销一枚令牌（RFC 7009）。失败不致命，调用方按需忽略。
    pub fn revoke(&self, token: &str) -> Result<Value, String> {
        let body = format!(
            "token={}&client_id={}&client_secret={}",
            url_encode(token),
            url_encode(&self.opt.client_id),
            url_encode(&self.opt.client_secret)
        );
        self.post_form("/sso/revoke", &body)
    }

    /// 取发现文档（匿名）。
    pub fn discovery(&self) -> Result<Value, String> {
        let url = format!("{}/.well-known/openid-configuration", self.opt.base_url);
        self.get_json(&url, "")
    }

    fn post_form(&self, path: &str, body: &str) -> Result<Value, String> {
        let url = format!("{}{}", self.opt.base_url, path);
        let resp = crate::net::http_client::blocking_request_auto_with(
            "POST",
            &url,
            &[("Accept", "application/json")],
            Some("application/x-www-form-urlencoded"),
            body.as_bytes().to_vec(),
            crate::net::http_client::HttpClientOptions {
                timeout: Duration::from_secs(self.opt.timeout_secs),
                ..Default::default()
            },
        )
        .map_err(|e| format!("调用 {url} 失败：{e}"))?;
        parse_reply(&url, resp)
    }

    fn get_json(&self, url: &str, token: &str) -> Result<Value, String> {
        let mut headers = vec![("Accept", "application/json")];
        let auth = format!("Bearer {token}");
        if !token.is_empty() {
            headers.push(("Authorization", auth.as_str()));
        }
        let resp = crate::net::http_client::blocking_request_auto_with(
            "GET",
            url,
            &headers,
            None,
            Vec::new(),
            crate::net::http_client::HttpClientOptions {
                timeout: Duration::from_secs(self.opt.timeout_secs),
                ..Default::default()
            },
        )
        .map_err(|e| format!("调用 {url} 失败：{e}"))?;
        parse_reply(url, resp)
    }
}

/// 统一解析响应：
/// 1. **标准 OAuth2 错误**（`{"error":"invalid_grant","error_description":…}`）→ Err；
/// 2. 本项目信封 `{state, info|message}` → Ok(info) / Err(message)；
/// 3. 其它 JSON 原样返回（第三方 IdP 常直接给 `{access_token:…}`）。
fn parse_reply(url: &str, resp: crate::net::http_client::HttpResponse) -> Result<Value, String> {
    let text = resp.body_text();
    let v: Value = serde_json::from_str(&text).map_err(|_| {
        format!(
            "{url} 返回的不是 JSON：{}",
            text.chars().take(200).collect::<String>()
        )
    })?;
    if let Some(err) = v.get("error").and_then(|s| s.as_str()) {
        let desc = v
            .get("error_description")
            .and_then(|s| s.as_str())
            .unwrap_or_default();
        return Err(if desc.is_empty() {
            err.to_string()
        } else {
            format!("{err}：{desc}")
        });
    }
    match v.get("state").and_then(|s| s.as_str()) {
        Some("success") => Ok(v.get("info").cloned().unwrap_or(Value::Null)),
        Some(_) => Err(v
            .get("message")
            .and_then(|s| s.as_str())
            .unwrap_or("调用失败")
            .to_string()),
        None => Ok(v),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> SsoClient {
        SsoClient::new(SsoClientOptions::new(
            "https://idp.example.com/",
            "erp-portal",
            "s3cret",
            "https://erp.example.com/cb?a=1&b=2",
        ))
    }

    #[test]
    fn base_url_trailing_slash_is_trimmed() {
        assert_eq!(client().options().base_url, "https://idp.example.com");
    }

    #[test]
    fn authorize_url_encodes_redirect_and_carries_state() {
        let url = client().authorize_url("st ate&x");
        assert!(url.starts_with("https://idp.example.com/sso/authorize?"));
        // redirect_uri 里的 `&` 必须被编码，否则回调地址会被截断
        assert!(url.contains("redirect_uri=https%3A%2F%2Ferp.example.com%2Fcb%3Fa%3D1%26b%3D2"), "{url}");
        assert!(url.contains("state=st%20ate%26x"), "state 必须编码：{url}");
        assert!(url.contains("response_type=code"));
        // 不带 PKCE 的旧入口不能凭空出现挑战
        assert!(!url.contains("code_challenge"));
    }

    #[test]
    fn pkce_url_carries_challenge_and_nonce_but_not_verifier() {
        let (url, pair) = client().authorize_url_pkce("s1", Some("n1"));
        assert!(url.contains("code_challenge_method=S256"), "{url}");
        assert!(url.contains(&url_encode(&pair.challenge)), "{url}");
        assert!(url.contains("nonce=n1"), "{url}");
        // verifier 绝不能出现在浏览器地址里
        assert!(!url.contains(&pair.verifier), "verifier 泄露到 URL 了：{url}");
        assert!(pkce::verify_s256(&pair.verifier, &pair.challenge));
    }

    #[test]
    fn parse_reply_understands_three_envelopes() {
        use crate::net::http_client::HttpResponse;
        let mk = |status: u16, body: &str| HttpResponse {
            status,
            headers: vec![],
            body: body.as_bytes().to_vec(),
        };
        let ok_env = mk(200, r#"{"state":"success","info":{"a":1}}"#);
        assert_eq!(parse_reply("u", ok_env).unwrap()["a"], 1);

        let warn_env = mk(200, r#"{"state":"warning","message":"不行"}"#);
        assert_eq!(parse_reply("u", warn_env).unwrap_err(), "不行");

        // 标准 OAuth2 错误优先于别的字段
        let oauth_err = mk(400, r#"{"error":"invalid_grant","error_description":"code 已用"}"#);
        let msg = parse_reply("u", oauth_err).unwrap_err();
        assert!(msg.contains("invalid_grant") && msg.contains("code 已用"), "{msg}");

        // 第三方 IdP 直接回裸对象 → 原样返回
        let raw = mk(200, r#"{"access_token":"t","token_type":"Bearer"}"#);
        assert_eq!(parse_reply("u", raw).unwrap()["access_token"], "t");
    }

    #[test]
    fn tokens_accessors_are_safe_on_missing_fields() {
        let t = SsoTokens(serde_json::json!({ "access_token": "a" }));
        assert_eq!(t.access_token(), "a");
        assert_eq!(t.refresh_token(), "");
        assert_eq!(t.id_token(), "");
        assert_eq!(t.expires_in(), 0);
    }
}
