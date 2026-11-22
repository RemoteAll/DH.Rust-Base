//! SSO / OIDC 通用件（**本库当 IdP 或当 Client 都能用**）。
//!
//! 这里只放「协议本身」的纯逻辑与网络调用，不掺任何业务：
//!
//! | 模块 | 特性 | 内容 |
//! |---|---|---|
//! | [`pkce`] | `sso` | S256 挑战（`code_challenge`）/ 校验 / 随机 `code_verifier` |
//! | [`rs256`] | `sso-rs256` | RS256 签名、公钥 JWK、用 JWKS 验签、密钥生成（PKCS#8 PEM） |
//! | [`client`] | `sso-client` | `SsoClient`：授权码 + PKCE + nonce + 换票 + userinfo + refresh |
//!
//! ## 为什么 PKCE / RS256 要放公共库
//!
//! 这两件事**每个项目都会写一遍、而且写错都不报错**：
//!
//! - PKCE 的 `code_challenge` 是 `base64url_nopad(sha256(verifier))`，
//!   少一个 `nopad`、或多做一次 urlencode，服务端只回 `invalid_grant`，排查起来像"密码错了"。
//! - RS256 的 JWK 里 `n` / `e` 必须是**无符号大端**、`base64url` 无填充；
//!   写成十六进制或标准 base64，`.NET` / `jose` 会直接 `invalid signature`。
//!
//! ## 典型用法
//!
//! **当 IdP**（本系统发令牌）：
//!
//! ```ignore
//! use dhrust::sso::rs256;
//! let pem = rs256::generate_pem(2048)?;            // 首次生成，落盘到自己项目的 keys 目录
//! let kid = rs256::kid_of(&pem)?;
//! let jwks = rs256::jwks(vec![rs256::public_jwk(&pem, &kid)?]);   // GET /.well-known/jwks.json
//! let id_token = rs256::sign(&claims, &pem, &kid)?;              // 放进 token 响应的 id_token
//! ```
//!
//! **当 Client**（本系统去登别人的 IdP）：
//!
//! ```ignore
//! use dhrust::sso::client::{SsoClient, SsoClientOptions};
//! let cli = SsoClient::new(SsoClientOptions::new(base, id, secret, redirect));
//! let (url, pkce) = cli.authorize_url_pkce("state-xyz", Some("nonce-abc"));
//! // 回调里：
//! let tk = cli.exchange_code_pkce(&code, &pkce.verifier)?;
//! let me = cli.userinfo(tk.access_token())?;
//! ```

pub mod pkce;

#[cfg(feature = "sso-rs256")]
pub mod rs256;

#[cfg(feature = "sso-client")]
pub mod client;
