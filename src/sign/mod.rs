use hmac::{Hmac, Mac};
use md5::Md5;
use rand::Rng;
use sha1::{Digest, Sha1};
use sha2::Sha256;

/// 根据参数生成校验值
/// timestamp1: 毫秒时间戳
/// nonce: 随机数
/// token: 密钥
pub fn create_signature(timestamp1: String, nonce: String, token: String) -> String {
    let mut array: [&str; 3] = [timestamp1.as_str(), nonce.as_str(), token.as_str()];
    array.sort(); // 升序
    let text = array.join(""); //在指定 String 数组的每个元素之间串联指定的分隔符 String，从而产生单个串联的字符串

    let mut hasher = Sha1::new();
    hasher.update(text);
    let result = hasher.finalize();
    let sign = format!("{:x}", result);
    sign
}

/// 获取随机数
pub fn getrand(length: usize) -> String {
    let rng = rand::thread_rng();
    let random_string: String = rng
        .sample_iter(&rand::distributions::Alphanumeric)
        .take(length)
        .map(char::from)
        .collect();

    random_string
}

/// 计算字符串的 MD5（32 位小写十六进制），对齐 NewLife 的 `MD5()` 扩展。
pub fn md5_hex(text: &str) -> String {
    md5_hex_bytes(text.as_bytes())
}

/// 计算字节数组的 MD5 摘要（16 字节）。
pub fn md5_bytes(data: &[u8]) -> [u8; 16] {
    let digest = Md5::digest(data);
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest);
    out
}

/// 字节数组 MD5 → 32 位小写十六进制（对齐 C# `BitConverter.ToString(hash).Replace("-", "").ToLowerInvariant()`）。
pub fn md5_hex_bytes(data: &[u8]) -> String {
    to_hex_lower(&md5_bytes(data))
}

/// 流式计算文件 MD5（64KB 缓冲；大文件不整读）。
pub fn md5_file_hex<P: AsRef<std::path::Path>>(path: P) -> std::io::Result<String> {
    use std::io::Read;

    let mut file = std::fs::File::open(path)?;
    let mut hasher = Md5::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }

    Ok(format!("{:x}", hasher.finalize()))
}

/// SHA1 十六进制小写（40 位；对齐 NewLife `Encrypt.GetSha1`）。
///
/// 收编自 HlktechIoT 设备签名算法（2026-10-09）：`SHA1(升序拼接 + 设备密钥)`
/// 与 C# `Encrypt.GetSha1` 输出一致；`create_signature` 内部同为 SHA1。
pub fn sha1_hex(text: &str) -> String {
    let digest = Sha1::digest(text.as_bytes());
    to_hex_lower(&digest)
}

/// SHA-256 摘要（32 字节）。
pub fn sha256_bytes(data: &[u8]) -> [u8; 32] {
    let digest = Sha256::digest(data);
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

/// SHA-256 → 64 位小写十六进制。
pub fn sha256_hex(data: &[u8]) -> String {
    to_hex_lower(&sha256_bytes(data))
}

/// 面板账号密码哈希：`SHA-256(salt:password)` hex（不可逆）。
///
/// Pek.RAgent / DHDeploy / Pek.RPanlServer 面板账号统一口径（2026-10-06 收编）；
/// 盐值由调用方生成并与账号一同存储（如 `random::hex(16)`），本函数保证拼接
/// 格式一致、避免各面板自行实现导致哈希口径漂移。
pub fn salted_sha256_hex(salt: &str, password: &str) -> String {
    sha256_hex(format!("{salt}:{password}").as_bytes())
}

/// HMAC-SHA256（对齐 C# `new HMACSHA256(key).ComputeHash(data)`）。
pub fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC 接受任意长度密钥");
    mac.update(data);
    let out = mac.finalize().into_bytes();
    let mut result = [0u8; 32];
    result.copy_from_slice(&out);
    result
}

/// HMAC-SHA256 → 64 位小写十六进制。
pub fn hmac_sha256_hex(key: &[u8], data: &[u8]) -> String {
    to_hex_lower(&hmac_sha256(key, data))
}

/// 字节数组 → 小写十六进制字符串（自 `plugin::hex_encode` 迁入，2026-10-09；
/// plugin 内保留同名转发，原调用路径不变）。
pub fn hex_encode(bytes: &[u8]) -> String {
    to_hex_lower(bytes)
}

/// 十六进制字符串 → 字节数组（奇数长度或含非法字符返回 `None`；大小写均可）。
pub fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    let val = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut i = 0;
    while i < bytes.len() {
        out.push((val(bytes[i])? << 4) | val(bytes[i + 1])?);
        i += 2;
    }
    Some(out)
}

/// 标准 Base64 编码（带 `=` 填充）。
pub fn base64_encode(data: &[u8]) -> String {
    use base64::Engine as _;

    base64::engine::general_purpose::STANDARD.encode(data)
}

/// Base64 解码（标准与 URL 安全字符混用、容忍缺省填充、忽略空白）。
pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};

    // URL 安全字符归一化为标准字符；空白（含换行）剔除
    let mut normalized = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '-' => normalized.push('+'),
            '_' => normalized.push('/'),
            c if c.is_ascii_whitespace() => {}
            c => normalized.push(c),
        }
    }

    // Indifferent：同时接受有/无填充（兼容各语言实现差异）
    const ENGINE: GeneralPurpose = GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
    );
    ENGINE.decode(normalized.as_bytes()).ok()
}

/// Base64 URL 安全无填充编码（`+`→`-`、`/`→`_`；令牌等 URL 场景）。
pub fn base64_url_no_pad(data: &[u8]) -> String {
    use base64::Engine as _;

    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

/// 字节数组固定时间比较（对齐 C# `CryptographicOperations.FixedTimeEquals`）。
pub fn fixed_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }

    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// 字节数组 → 小写十六进制。
fn to_hex_lower(data: &[u8]) -> String {
    const TABLE: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(data.len() * 2);
    for b in data {
        out.push(TABLE[(b >> 4) as usize] as char);
        out.push(TABLE[(b & 0x0F) as usize] as char);
    }
    out
}

/// FNV-1a 64 位哈希（非加密快速散列；内存混淆、短键散列等场景）。
///
/// 来源：PekSendToMo 房间密码哈希（2026-09-29 收编，算法与行为不变）。
pub fn fnv1a_64(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_hex_matches_newlife() {
        assert_eq!(md5_hex("abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(md5_hex(""), "d41d8cd98f00b204e9800998ecf8427e");
    }

    #[test]
    fn signature_and_rand_are_stable() {
        // 参数排序后拼接：SHA1 十六进制应为 40 位
        let sign = create_signature("123".into(), "7".into(), "key".into());
        assert_eq!(sign.len(), 40);
        assert_eq!(getrand(12).chars().count(), 12);
    }

    #[test]
    fn sha1_and_hex_helpers_match_reference() {
        // SHA1 标准向量
        assert_eq!(sha1_hex("abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(sha1_hex(""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        // hex 编码/解码往返（大小写均可解码）
        assert_eq!(hex_encode(&[0x00, 0x1f, 0xab, 0xff]), "001fabff");
        assert_eq!(hex_decode("001FABff"), Some(vec![0x00, 0x1f, 0xab, 0xff]));
        assert_eq!(hex_decode("abc"), None, "奇数长度拒绝");
        assert_eq!(hex_decode("00zz"), None, "非法字符拒绝");
        assert_eq!(hex_encode(&[]), "");
    }

    #[test]
    fn salted_sha256_matches_format_and_salt() {
        // 面板账号口径：SHA-256(salt:password)
        assert_eq!(salted_sha256_hex("s", "p"), sha256_hex(b"s:p"));
        assert_ne!(salted_sha256_hex("s", "p"), salted_sha256_hex("s2", "p"));
        assert_ne!(salted_sha256_hex("s", "p"), salted_sha256_hex("s", "p2"));
        assert_eq!(salted_sha256_hex("s", "p").len(), 64);
    }

    #[test]
    fn fnv1a_64_stable() {
        // FNV-1a 64 偏移基准（空串；算法标准值）
        assert_eq!(fnv1a_64(""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a_64("hello"), fnv1a_64("hello"));
        assert_ne!(fnv1a_64("hello"), fnv1a_64("world"));
    }

    #[test]
    fn md5_bytes_matches_string_variant() {
        assert_eq!(md5_hex_bytes(b"abc"), md5_hex("abc"));
        assert_eq!(
            md5_hex_bytes(b""),
            "d41d8cd98f00b204e9800998ecf8427e"
        );
    }

    #[test]
    fn md5_file_hex_matches_content_hash() {
        let path = std::env::temp_dir().join(format!("dhrust-md5-{}.bin", std::process::id()));
        std::fs::write(&path, b"abc").unwrap();
        assert_eq!(
            md5_file_hex(&path).unwrap(),
            "900150983cd24fb0d6963f7d28e17f72"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn sha256_known_vectors() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn hmac_sha256_rfc4231_case1() {
        // RFC 4231 Test Case 1: key=0x0b*20, data="Hi There"
        let key = [0x0bu8; 20];
        assert_eq!(
            hmac_sha256_hex(&key, b"Hi There"),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn base64_roundtrip_and_tolerance() {
        for case in [&b""[..], b"f", b"fo", b"foo", b"foob", b"fooba", b"foobar"] {
            let enc = base64_encode(case);
            let dec = base64_decode(&enc).expect("decode");
            assert_eq!(dec, case, "case={case:?} enc={enc}");
        }

        // URL 安全变体、缺省填充与空白容忍
        assert_eq!(base64_decode("Zm9vYmFy").unwrap(), b"foobar");
        assert_eq!(base64_decode("Zm9v-_Yh").is_some(), true);
        assert_eq!(base64_decode(" Zm9\n  vYmFy ").unwrap(), b"foobar");
        assert_eq!(base64_url_no_pad(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn fixed_time_eq_semantics() {
        assert!(fixed_time_eq(b"abc", b"abc"));
        assert!(!fixed_time_eq(b"abc", b"abd"));
        assert!(!fixed_time_eq(b"abc", b"ab"));
    }
}
