//! PKCE（RFC 7636）：`code_verifier` → `code_challenge`（S256）。
//!
//! 现代 OIDC 客户端（.NET `AddOpenIdConnect`、oidc-client-js、IdentityModel）
//! **默认就带 PKCE**，服务端不认这个参数时：
//! - 有的库直接报错；
//! - 有的库照发不误，但**授权码变成"拿到就能换票"**，中间人拿到 code 就能换。
//!
//! 所以：服务端要校验、客户端要生成。两边都从这一处走，口径才不会漂。
//!
//! 只有 `S256`（`plain` 明文挑战等于没做，规范也不建议，本库不提供）。

use crate::sign::{base64_url_no_pad, fixed_time_eq, sha256_bytes};

/// 允许的 `verifier` 长度（RFC 7636 §4.1：43~128 个 unreserved 字符）。
pub const MIN_VERIFIER_LEN: usize = 43;
pub const MAX_VERIFIER_LEN: usize = 128;

/// 生成 `code_challenge` = `base64url_nopad(sha256(code_verifier))`。
pub fn challenge_s256(verifier: &str) -> String {
    base64_url_no_pad(&sha256_bytes(verifier.as_bytes()))
}

/// 校验 `code_verifier` 是否与 `code_challenge` 匹配（常量时间比较）。
pub fn verify_s256(verifier: &str, challenge: &str) -> bool {
    if verifier.is_empty() || challenge.is_empty() {
        return false;
    }
    fixed_time_eq(
        challenge_s256(verifier).as_bytes(),
        challenge.trim().as_bytes(),
    )
}

/// 生成随机 `code_verifier`（默认 64 位 `[A-Za-z0-9-._~]`）。
///
/// 长度不在 43~128 之外时自动夹到边界值（宁可长一点，也不要生成一个规范外的串）。
pub fn random_verifier(len: usize) -> String {
    let n = len.clamp(MIN_VERIFIER_LEN, MAX_VERIFIER_LEN);
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    // 取 OS 熵再映射：用 `bytes` 拿真随机，避免 `rand` 的可预测种子问题
    let raw = crate::random::bytes(n);
    raw.iter()
        .map(|b| CHARSET[(*b as usize) % CHARSET.len()] as char)
        .collect()
}

/// `verifier` 是否符合规范（长度 + 字符集）。服务端校验请求时先过这一关，
/// 否则超长 verifier 会让 `sha256` 白算一遍（拒绝要快、且理由明确）。
pub fn is_valid_verifier(verifier: &str) -> bool {
    let len = verifier.len();
    if !(MIN_VERIFIER_LEN..=MAX_VERIFIER_LEN).contains(&len) {
        return false;
    }
    verifier
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
}

/// `code_challenge_method` 白名单判定（只认 `S256`；大小写不敏感，各家写法不一）。
pub fn supports_method(method: &str) -> bool {
    method.trim().is_empty() || method.eq_ignore_ascii_case("S256")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 7636 附录 B 的官方示例（钉住「口径对不对」，不是随便造一对值）。
    #[test]
    fn rfc7636_appendix_b_vector() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            challenge_s256(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert!(verify_s256(verifier, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"));
        assert!(!verify_s256("wrong-verifier", "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"));
    }

    #[test]
    fn challenge_is_base64url_without_padding() {
        let c = challenge_s256("abc");
        assert!(!c.contains('='), "不能带 base64 填充: {c}");
        assert!(!c.contains('+') && !c.contains('/'), "必须是 url-safe: {c}");
        assert_eq!(c.len(), 43, "sha256 -> 32 字节 -> 43 个 base64url 字符");
    }

    #[test]
    fn random_verifier_is_spec_compliant() {
        let v = random_verifier(64);
        assert_eq!(v.len(), 64);
        assert!(is_valid_verifier(&v), "生成物必须过自己的校验: {v}");
        // 夹边界
        assert_eq!(random_verifier(10).len(), MIN_VERIFIER_LEN);
        assert_eq!(random_verifier(500).len(), MAX_VERIFIER_LEN);
        // 两次不一样（真随机）
        assert_ne!(random_verifier(64), random_verifier(64));
    }

    #[test]
    fn verifier_rules_reject_what_spec_rejects() {
        assert!(!is_valid_verifier(""), "空");
        assert!(!is_valid_verifier(&"a".repeat(MIN_VERIFIER_LEN - 1)), "太短");
        assert!(!is_valid_verifier(&"a".repeat(MAX_VERIFIER_LEN + 1)), "太长");
        assert!(!is_valid_verifier(&format!("{}!", "a".repeat(MIN_VERIFIER_LEN - 1))), "非法字符");
        assert!(is_valid_verifier(&"a".repeat(MIN_VERIFIER_LEN)));
        // 9 字符 × 5 = 45（≥43）：字符集合法即通过
        assert!(is_valid_verifier(&"-._~azAZ09".repeat(5)));
    }

    #[test]
    fn only_s256_is_accepted() {
        assert!(supports_method("S256"));
        assert!(supports_method("s256"), "大小写不敏感");
        assert!(supports_method(""), "不传 = 没启用 PKCE");
        assert!(!supports_method("plain"), "明文挑战等于没做");
        assert!(!supports_method("S512"));
    }
}
