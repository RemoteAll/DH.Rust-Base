//! 可逆加密（AES-256-GCM）——配置/存储中敏感值的“防直读”加密。
//!
//! # 背景
//!
//! 服务类工具常需在配置文件中保存**可再次使用**的敏感值（典型：登录密码——用于
//! 令牌失效时自动重新登录）。这类值既不能直接存明文（配置文件随交付物外流/被顺手
//! 打开），又必须能被程序解码（哈希不可逆，不适用）。本模块提供统一的
//! AES-256-GCM 实现（HlkProductTool 实践沉淀收编）。
//!
//! # 接口
//!
//! - [`encrypt`] / [`decrypt`]：以调用方提供的 32 字节密钥加解密；存储格式
//!   `enc:v1:{base64(nonce || 密文)}`（随机 12 字节 nonce；GCM 完整性校验——
//!   篡改或换钥解密必然失败）；
//! - [`is_encrypted`]：判断是否本模块密文格式（含版本前缀，便于将来换算法）；
//! - [`key_from_seed`]：以字符串种子派生 32 字节密钥（SHA-256），便于调用方
//!   用固定种子管理密钥。
//!
//! # 密钥策略（调用方负责）
//!
//! 库**不内嵌密钥**：密钥来源与“是否硬编码”由各项目自行决定。服务类工具的惯用
//! 策略是：源码常量硬编码（不随配置文件外泄，禁止做成配置项），见 HlkProductTool
//! `src/secret.rs` 的“工具策略层”封装。
//!
//! # 安全边界
//!
//! 保护级别为“防直读”：能获得二进制并逆向的一方理论上可提取密钥（与内嵌签名密钥
//! 同等信任假设）；对配置文件外流、随手打开查看等常见场景有效。

use aes_gcm::aead::rand_core::RngCore;
use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::Engine as _;
use sha2::{Digest, Sha256};

/// 密文前缀（含版本号，便于将来更换算法）。
pub const PREFIX: &str = "enc:v1:";

/// 是否本模块的加密存储格式（`enc:v1:` 前缀）。
pub fn is_encrypted(text: &str) -> bool {
    text.starts_with(PREFIX)
}

/// 以字符串种子派生 32 字节密钥（SHA-256；同一项目使用同一固定种子即可）。
pub fn key_from_seed(seed: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(seed.as_bytes());
    hasher.finalize().into()
}

/// 加密（返回带 `enc:v1:` 前缀的密文文本；空串原样返回空串）。
pub fn encrypt(plain: &str, key: &[u8; 32]) -> Result<String, String> {
    if plain.is_empty() {
        return Ok(String::new());
    }

    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let mut nonce = [0u8; 12];
    OsRng.fill_bytes(&mut nonce);
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce), plain.as_bytes())
        .map_err(|e| format!("加密失败：{e}"))?;

    let mut packed = Vec::with_capacity(nonce.len() + ciphertext.len());
    packed.extend_from_slice(&nonce);
    packed.extend_from_slice(&ciphertext);
    Ok(format!(
        "{PREFIX}{}",
        base64::engine::general_purpose::STANDARD.encode(packed)
    ))
}

/// 解密（输入须为带 `enc:v1:` 前缀的密文；返回明文）。
pub fn decrypt(text: &str, key: &[u8; 32]) -> Result<String, String> {
    let Some(body) = text.strip_prefix(PREFIX) else {
        return Err("不是加密存储格式".to_string());
    };
    let packed = base64::engine::general_purpose::STANDARD
        .decode(body.trim())
        .map_err(|e| format!("密文 Base64 解码失败：{e}"))?;
    if packed.len() <= 12 {
        return Err("密文长度不足".to_string());
    }

    let (nonce, ciphertext) = packed.split_at(12);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let plain = cipher
        .decrypt(Nonce::from_slice(nonce), ciphertext)
        .map_err(|_| "密文解密失败（密钥变化或数据损坏）".to_string())?;
    String::from_utf8(plain).map_err(|e| format!("解密结果不是合法文本：{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_from_seed_is_deterministic_and_sized() {
        let a = key_from_seed("hlk-product-tool");
        let b = key_from_seed("hlk-product-tool");
        let c = key_from_seed("another-tool");
        assert_eq!(a, b, "同种子应派生同密钥");
        assert_ne!(a, c, "不同种子应派生不同密钥");
        assert_eq!(a.len(), 32);
    }

    #[test]
    fn encrypt_decrypt_round_trip() {
        let key = key_from_seed("test-seed");
        let plain = "P@ssw0rd-中文-123";
        let enc = encrypt(plain, &key).unwrap();
        assert!(is_encrypted(&enc));
        assert!(!enc.contains(plain));
        assert_eq!(decrypt(&enc, &key).unwrap(), plain);

        // 同一明文两次加密结果不同（随机 nonce）
        assert_ne!(encrypt(plain, &key).unwrap(), enc);
    }

    #[test]
    fn wrong_key_and_tamper_are_rejected() {
        let key = key_from_seed("key-a");
        let other = key_from_seed("key-b");
        let enc = encrypt("secret", &key).unwrap();

        assert!(decrypt(&enc, &other).is_err(), "换钥应解密失败");

        let mut tampered = enc.clone();
        tampered.push('A');
        assert!(decrypt(&tampered, &key).is_err(), "篡改应被完整性校验拒绝");
    }

    #[test]
    fn empty_and_invalid_inputs() {
        let key = key_from_seed("k");
        assert_eq!(encrypt("", &key).unwrap(), "");
        assert!(!is_encrypted("plain-text"));
        assert!(decrypt("plain-text", &key).is_err());
        assert!(decrypt("enc:v1:not-base64!!", &key).is_err());
        // Base64 合法但长度不足（nonce 都放不下）
        assert!(decrypt("enc:v1:aGVsbG8=", &key).is_err());
    }
}
