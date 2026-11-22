//! RS256（RSA PKCS#1 v1.5 + SHA-256）JWT 签名、JWK 公钥与 JWKS 验签。
//!
//! **为什么 OIDC 必须要它**：`id_token` 要交给第三方客户端验签，而 HS256 是对称的——
//! 公钥没法公开，客户端只能预先配一份共享密钥（很多库默认干脆不支持）。
//! RS256 用私钥签、公钥验，`jwks_uri` 一挂，`.NET` / `jose` / `oidc-client` 开箱即验。
//!
//! ## 三个容易写错、写错也不报错的点（本模块已钉住）
//!
//! 1. **JWK 的 `n`/`e` 是无符号大端整数的 base64url（无填充）**，
//!    不是十六进制、也不是标准 base64 —— 写成后者，客户端只会回 `invalid signature`。
//! 2. **`kid` 必须与 header 里的一致**，且客户端会拿它去 JWKS 里挑钥匙；
//!    这里由公钥 DER 的 sha256 前 16 位十六进制推导，**同一把钥匙永远同一个 kid**。
//! 3. **签名输入是 `header.payload` 的原始 ASCII 字节**，不能重新序列化后再签
//!    （JSON 键顺序/空白一变，验签必失败）。
//!
//! ## 密钥放哪
//!
//! 本模块不碰文件系统：`generate_pem()` 给你 PEM 字符串，落盘位置由调用方决定
//! （本项目惯例：`{数据目录}/Config/keys/*.key`，且**不做成可配置项**）。

use rsa::pkcs1v15::{Signature, SigningKey, VerifyingKey};
use rsa::pkcs8::{DecodePrivateKey, EncodePrivateKey, LineEnding};
use rsa::signature::{SignatureEncoding, Signer, Verifier};
use rsa::traits::PublicKeyParts;
use rsa::{BigUint, RsaPrivateKey, RsaPublicKey};
use serde_json::{json, Map, Value};
use sha2::Sha256;

use crate::sign::{base64_url_no_pad, sha256_bytes};

/// 默认密钥位数。2048 是兼容性最好的档（更低不安全，4096 会让每次签名慢一个量级）。
pub const DEFAULT_BITS: usize = 2048;

/// base64url 解码（无填充；与 [`base64_url_no_pad`] 对称）。
///
/// 本库的 `sign::base64_decode` 是**标准** base64（`+/`、可带填充），
/// JWT/JWK 用的都是 url-safe 无填充，两者混用会解出垃圾且不报错，所以单独放一个。
fn b64url_decode(text: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    let t = text.trim().trim_end_matches('=');
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(t)
        .ok()
}

/// 解析 PEM 私钥（PKCS#8 或 PKCS#1 都认 —— 各家工具链导出的不一样）。
fn parse_private(pem: &str) -> Result<RsaPrivateKey, String> {
    let t = pem.trim();
    if t.contains("BEGIN RSA PRIVATE KEY") {
        use rsa::pkcs1::DecodeRsaPrivateKey;
        return RsaPrivateKey::from_pkcs1_pem(t).map_err(|e| format!("私钥 PEM 解析失败（PKCS#1）：{e}"));
    }
    RsaPrivateKey::from_pkcs8_pem(t).map_err(|e| format!("私钥 PEM 解析失败（PKCS#8）：{e}"))
}

/// 生成一对新密钥，返回 **PKCS#8 PEM**（私钥）。
///
/// 2048 位在普通机器上几十到几百毫秒，**只在首次启动/轮换时调用一次**，别放进请求路径。
pub fn generate_pem(bits: usize) -> Result<String, String> {
    let bits = if bits < 2048 { DEFAULT_BITS } else { bits };
    let mut rng = rand::rngs::OsRng;
    let key = RsaPrivateKey::new(&mut rng, bits).map_err(|e| format!("生成 RSA 密钥失败：{e}"))?;
    key.to_pkcs8_pem(LineEnding::LF)
        .map(|s| s.to_string())
        .map_err(|e| format!("导出 PEM 失败：{e}"))
}

/// 由私钥 PEM 推导 `kid`：公钥 DER 的 sha256 前 16 位十六进制。
///
/// 同一把钥匙恒定（客户端缓存 JWKS 后不会因为服务重启而换 kid）；
/// 轮换密钥 → kid 自然变化，客户端拉到新 JWKS 即可。
pub fn kid_of(pem: &str) -> Result<String, String> {
    let key = parse_private(pem)?;
    let der = RsaPublicKey::from(&key).n().to_bytes_be();
    Ok(sha256_bytes(&der)[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// 公钥 JWK（单个 key 对象）：`{kty,use,alg,kid,n,e}`。
pub fn public_jwk(pem: &str, kid: &str) -> Result<Value, String> {
    let key = parse_private(pem)?;
    let pub_key = RsaPublicKey::from(&key);
    Ok(json!({
        "kty": "RSA",
        "use": "sig",
        "alg": "RS256",
        "kid": kid,
        "n": base64_url_no_pad(&pub_key.n().to_bytes_be()),
        "e": base64_url_no_pad(&pub_key.e().to_bytes_be()),
    }))
}

/// 把若干 JWK 包成标准的 `{"keys":[…]}`（`jwks_uri` 的响应体）。
pub fn jwks(keys: Vec<Value>) -> Value {
    json!({ "keys": keys })
}

/// 用 PEM 私钥签一枚 RS256 JWT。
///
/// `claims` 里通常要有 `iss/aud/sub/exp/iat`；本函数**只加 `iat`（缺失时）**，
/// 其余由调用方决定（IdP 与 RP 的口径不同，公共库不该替业务拍板）。
pub fn sign(claims: &Map<String, Value>, pem: &str, kid: &str) -> Result<String, String> {
    let key = parse_private(pem)?;
    let mut payload = claims.clone();
    if !payload.contains_key("iat") {
        payload.insert("iat".to_string(), json!(crate::times::gettimestamp() as i64));
    }
    let header = json!({ "alg": "RS256", "typ": "JWT", "kid": kid });
    let h = base64_url_no_pad(header.to_string().as_bytes());
    let p = base64_url_no_pad(Value::Object(payload).to_string().as_bytes());
    let signing_input = format!("{h}.{p}");
    let sk = SigningKey::<Sha256>::new(key);
    let sig = sk.sign(signing_input.as_bytes());
    Ok(format!("{signing_input}.{}", base64_url_no_pad(&sig.to_bytes())))
}

/// 用 JWKS（`{"keys":[…]}`）或单个 JWK 验证 RS256 令牌，成功返回 claims。
///
/// 校验项：**签名**、`exp`（过期）、`nbf`（未生效）。`iss` / `aud` / `nonce`
/// 属于业务校验，由调用方比对（各家的受众规则不一样）。
pub fn verify_with_jwks(token: &str, jwks: &Value) -> Result<Map<String, Value>, String> {
    let mut parts = token.split('.');
    let (h, p, s) = match (parts.next(), parts.next(), parts.next()) {
        (Some(h), Some(p), Some(s)) if !s.is_empty() => (h, p, s),
        _ => return Err("令牌不是三段式 JWT".to_string()),
    };
    let header: Value = serde_json::from_slice(&b64url_decode(h).ok_or("JWT header 不是 base64url")?)
        .map_err(|_| "JWT header 不是 JSON".to_string())?;
    if header.get("alg").and_then(|v| v.as_str()) != Some("RS256") {
        return Err("令牌不是 RS256 签名".to_string());
    }
    let kid = header.get("kid").and_then(|v| v.as_str()).unwrap_or_default();

    let single;
    let keys: &Vec<Value> = if let Some(arr) = jwks.get("keys").and_then(|v| v.as_array()) {
        arr
    } else {
        single = vec![jwks.clone()];
        &single
    };
    // 有 kid 就按 kid 挑（多把钥匙并存、轮换期间）；没有就试全部
    let candidates: Vec<&Value> = keys
        .iter()
        .filter(|k| kid.is_empty() || k.get("kid").and_then(|v| v.as_str()) == Some(kid))
        .collect();
    if candidates.is_empty() {
        return Err(format!("JWKS 里没有 kid={kid} 的公钥"));
    }

    let sig_bytes = b64url_decode(s).ok_or("签名不是 base64url")?;
    let signature = Signature::try_from(sig_bytes.as_slice()).map_err(|_| "签名长度不合法".to_string())?;
    let signing_input = format!("{h}.{p}");
    let mut verified = false;
    for k in candidates {
        let n = k.get("n").and_then(|v| v.as_str()).and_then(b64url_decode);
        let e = k.get("e").and_then(|v| v.as_str()).and_then(b64url_decode);
        let (Some(n), Some(e)) = (n, e) else { continue };
        let Ok(pub_key) = RsaPublicKey::new(BigUint::from_bytes_be(&n), BigUint::from_bytes_be(&e))
        else {
            continue;
        };
        if VerifyingKey::<Sha256>::new(pub_key)
            .verify(signing_input.as_bytes(), &signature)
            .is_ok()
        {
            verified = true;
            break;
        }
    }
    if !verified {
        return Err("RS256 验签失败".to_string());
    }

    let claims: Map<String, Value> =
        serde_json::from_slice(&b64url_decode(p).ok_or("JWT payload 不是 base64url")?)
            .map_err(|_| "JWT payload 不是 JSON".to_string())?;
    let now = crate::times::gettimestamp() as i64;
    if let Some(exp) = claims.get("exp").and_then(|v| v.as_i64()) {
        if exp > 0 && exp <= now {
            return Err("令牌已过期".to_string());
        }
    }
    if let Some(nbf) = claims.get("nbf").and_then(|v| v.as_i64()) {
        if nbf > now {
            return Err("令牌尚未生效".to_string());
        }
    }
    Ok(claims)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims() -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("iss".into(), json!("https://idp.example.com"));
        m.insert("aud".into(), json!("erp-portal"));
        m.insert("sub".into(), json!("acme:12"));
        m.insert("exp".into(), json!(crate::times::gettimestamp() as i64 + 300));
        m
    }

    #[test]
    fn sign_then_verify_roundtrip() {
        let pem = generate_pem(2048).expect("生成密钥");
        let kid = kid_of(&pem).expect("kid");
        let jwk = public_jwk(&pem, &kid).expect("jwk");
        let token = sign(&claims(), &pem, &kid).expect("签名");

        let got = verify_with_jwks(&token, &jwks(vec![jwk])).expect("验签");
        assert_eq!(got.get("sub").and_then(|v| v.as_str()), Some("acme:12"));
        assert_eq!(got.get("aud").and_then(|v| v.as_str()), Some("erp-portal"));
        // 自动补 iat
        assert!(got.get("iat").and_then(|v| v.as_i64()).unwrap_or(0) > 0);
    }

    #[test]
    fn jwk_fields_are_base64url_without_padding() {
        let pem = generate_pem(2048).expect("生成密钥");
        let kid = kid_of(&pem).expect("kid");
        let jwk = public_jwk(&pem, &kid).expect("jwk");
        assert_eq!(jwk["kty"], "RSA");
        assert_eq!(jwk["alg"], "RS256");
        assert_eq!(jwk["use"], "sig");
        assert_eq!(jwk["kid"], json!(kid));
        for f in ["n", "e"] {
            let v = jwk[f].as_str().expect("字段是字符串");
            assert!(!v.contains('=') && !v.contains('+') && !v.contains('/'), "{f} 必须是 url-safe 无填充：{v}");
        }
        // e 恒为 65537 -> "AQAB"
        assert_eq!(jwk["e"].as_str(), Some("AQAB"));
        // kid 是 16 位十六进制（8 字节）
        assert_eq!(kid.len(), 16);
        assert!(kid.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn tampered_token_is_rejected() {
        let pem = generate_pem(2048).expect("生成密钥");
        let kid = kid_of(&pem).expect("kid");
        let token = sign(&claims(), &pem, &kid).expect("签名");
        let other = generate_pem(2048).expect("另一把钥匙");
        let other_kid = kid_of(&other).expect("kid2");
        let other_jwks = jwks(vec![public_jwk(&other, &other_kid).expect("jwk2")]);
        assert!(verify_with_jwks(&token, &other_jwks).is_err(), "换一把公钥必须验不过");
        // 改 payload（把 sub 换掉）→ 签名对不上
        let mut parts: Vec<&str> = token.split('.').collect();
        let forged = base64_url_no_pad(br#"{"iss":"https://idp.example.com","aud":"x","sub":"evil","exp":9999999999}"#);
        parts[1] = &forged;
        assert!(verify_with_jwks(&parts.join("."), &jwks(vec![public_jwk(&pem, &kid).unwrap()])).is_err());
    }

    #[test]
    fn expired_token_is_rejected() {
        let pem = generate_pem(2048).expect("生成密钥");
        let kid = kid_of(&pem).expect("kid");
        let mut m = claims();
        m.insert("exp".into(), json!(crate::times::gettimestamp() as i64 - 10));
        let token = sign(&m, &pem, &kid).expect("签名");
        let err = verify_with_jwks(&token, &jwks(vec![public_jwk(&pem, &kid).unwrap()])).unwrap_err();
        assert!(err.contains("过期"), "{err}");
    }

    #[test]
    fn hs256_token_is_not_accepted_as_rs256() {
        // 防「算法混淆」：拿 HS256 的令牌来当 RS256 验，必须直接拒
        let pem = generate_pem(2048).expect("生成密钥");
        let kid = kid_of(&pem).expect("kid");
        let header = base64_url_no_pad(br#"{"alg":"HS256","typ":"JWT"}"#);
        let payload = base64_url_no_pad(br#"{"sub":"x"}"#);
        let token = format!("{header}.{payload}.aaa");
        let err = verify_with_jwks(&token, &jwks(vec![public_jwk(&pem, &kid).unwrap()])).unwrap_err();
        assert!(err.contains("RS256"), "{err}");
    }
}
