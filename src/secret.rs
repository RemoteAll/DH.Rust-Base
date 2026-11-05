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
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

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

// ————— 密钥文件（数据目录 `keys/*.key`；密钥不作配置项）—————

/// 默认密钥文件（相对数据目录）。
pub const KEY_FILE: &str = "keys/secret.key";

/// 进程内全局密钥种子（`init_keyfile` 初始化后由 [`protect`] / [`reveal`] 使用）。
///
/// 存**种子**而非派生后的密钥：同一进程内可能先后服务多个数据目录（测试、多实例工具），
/// 再次 `init_keyfile` 时可为新目录补一份同样的密钥文件，避免「用 A 目录密钥加密、B 目录解不开」。
static FILE_SEED: OnceLock<String> = OnceLock::new();

/// 当前全局密钥（未初始化返回 `None`）。
fn current_key() -> Option<[u8; 32]> {
    FILE_SEED.get().map(|seed| key_from_seed(seed))
}

/// 密钥文件路径（`rel` 为空时用 [`KEY_FILE`]）。
///
/// # 参数
/// - `dir`：数据目录。
/// - `rel`：相对数据目录的路径。
///
/// # 返回值
/// 密钥文件绝对路径。
pub fn keyfile_path(dir: &Path, rel: &str) -> PathBuf {
    dir.join(if rel.trim().is_empty() { KEY_FILE } else { rel.trim() })
}

/// 读取或首次生成密钥文件，并初始化进程内全局密钥（幂等）。
///
/// 文件内容为 64 个十六进制字符（32 字节熵）；首次启动用 OS 熵生成，之后复用。
/// 密钥**不做配置项**（不随配置外流），但需随数据目录一起备份——丢失后已加密的口令解不开。
///
/// # 参数
/// - `dir`：数据目录。
/// - `rel`：相对路径（空 = [`KEY_FILE`]）。
///
/// # 返回值
/// 成功返回 `Ok(())`；读写失败返回中文错误。
pub fn init_keyfile(dir: &Path, rel: &str) -> Result<(), String> {
    let path = keyfile_path(dir, rel);
    if let Some(seed) = FILE_SEED.get() {
        // 已初始化：目标目录缺密钥文件时补写同一份（同进程多数据目录/测试场景）
        if !path.is_file() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("创建密钥目录失败（{}）：{e}", parent.display()))?;
            }
            std::fs::write(&path, seed)
                .map_err(|e| format!("写入密钥文件失败（{}）：{e}", path.display()))?;
        }
        return Ok(());
    }
    let seed = if path.is_file() {
        std::fs::read_to_string(&path)
            .map_err(|e| format!("读取密钥文件失败（{}）：{e}", path.display()))?
            .trim()
            .to_string()
    } else {
        let seed = crate::random::hex(32);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("创建密钥目录失败（{}）：{e}", parent.display()))?;
        }
        std::fs::write(&path, &seed)
            .map_err(|e| format!("写入密钥文件失败（{}）：{e}", path.display()))?;
        crate::warn!(
            "已生成凭据加密密钥：{}（请随数据目录备份；丢失后已加密的口令需重新设置）",
            path.display()
        );
        seed
    };
    if seed.len() < 16 {
        return Err(format!(
            "密钥文件内容过短（{} 字符）：{}",
            seed.len(),
            path.display()
        ));
    }
    let _ = FILE_SEED.set(seed);
    Ok(())
}

/// 全局密钥是否已初始化。
pub fn keyfile_ready() -> bool {
    FILE_SEED.get().is_some()
}

/// 加密落盘用：未初始化密钥或空串时**原样返回**（保证调用方无密钥也不丢值）。
///
/// **确定性加密**：同一密钥 + 同一明文 → 同一密文（nonce 由明文 SHA-256 前 12 字节派生）。
/// 这是为「配置文件渲染必须幂等」而做的取舍：随机 nonce 会让配置每次保存都产生新文本，
/// 破坏 `save_if_changed`（无变化不写盘）与配置热重载（每次重载都被当成改动）。
/// 代价是同一密钥下相同明文会得到相同密文（泄露"两处值相同"），对口令类场景可接受；
/// 需要语义安全时用 [`encrypt`]（随机 nonce）。
///
/// # 参数
/// - `plain`：明文。
///
/// # 返回值
/// 密文（`enc:v1:` 前缀）或原值。
pub fn protect(plain: &str) -> String {
    if plain.is_empty() {
        return String::new();
    }
    match current_key() {
        Some(key) => encrypt_deterministic(plain, &key).unwrap_or_else(|e| {
            crate::warn!("凭据加密失败，按明文写入：{e}");
            plain.to_string()
        }),
        None => plain.to_string(),
    }
}

/// 确定性加密（nonce 由明文摘要派生；仅供 [`protect`] 与同类「渲染幂等」场景使用）。
fn encrypt_deterministic(plain: &str, key: &[u8; 32]) -> Result<String, String> {
    let digest = Sha256::digest(plain.as_bytes());
    let mut nonce = [0u8; 12];
    nonce.copy_from_slice(&digest[..12]);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
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

/// 读配置用：密文解密，非密文（老配置的明文）原样返回，便于平滑迁移。
///
/// # 参数
/// - `text`：配置中的值。
///
/// # 返回值
/// 明文；密文需要密钥而未初始化/解密失败时报错。
pub fn reveal(text: &str) -> Result<String, String> {
    if text.is_empty() || !is_encrypted(text) {
        return Ok(text.to_string());
    }
    let Some(key) = current_key() else {
        return Err("密文需要密钥文件，但本进程尚未初始化（应先调用 dhrust::secret::init_keyfile）"
            .to_string());
    };
    decrypt(text, &key)
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

    #[test]
    fn keyfile_round_trip_and_lenient() {
        let dir = std::env::temp_dir().join(format!("dhrust-secret-{}", crate::random::hex(6)));
        let rel = "keys/test.key";
        let path = keyfile_path(&dir, rel);
        assert!(!path.exists());

        init_keyfile(&dir, rel).expect("首次应生成密钥文件");
        let seed = std::fs::read_to_string(&path).expect("密钥文件应可读");
        assert!(seed.trim().len() >= 16, "种子长度应足够");

        let enc = protect("P@ssw0rd");
        assert!(is_encrypted(&enc), "protect 应产出密文");
        assert_eq!(reveal(&enc).unwrap(), "P@ssw0rd");
        // 确定性：同明文重复加密结果一致（配置渲染幂等的前提）
        assert_eq!(protect("P@ssw0rd"), enc, "protect 必须幂等");
        assert_ne!(protect("P@ssw0rd2"), enc, "不同明文应得不同密文");
        // 明文（老配置）原样返回，便于平滑迁移
        assert_eq!(reveal("plain-old").unwrap(), "plain-old");
        assert_eq!(protect(""), "");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
