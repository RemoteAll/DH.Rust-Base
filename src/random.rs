//! 安全随机（OS 熵；`rand::thread_rng`）。
//!
//! 来源：跨 Agent 传输令牌等安全凭证场景（2026-09-29）；区别于应用层的"非安全随机后缀"（时间+进程混淆）。

use rand::RngCore;

use crate::sign::base64_url_no_pad;

/// 生成 `len` 字节安全随机数据。
pub fn bytes(len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    rand::thread_rng().fill_bytes(&mut buf);
    buf
}

/// 生成 22 字符 URL 安全令牌（16 随机字节 → base64url 无填充）。
///
/// 对齐 C# `GenerateTransferToken`（GUID 16 字节 → base64url 去填充）的强度与形态。
pub fn token() -> String {
    base64_url_no_pad(&bytes(16))
}

/// 生成 `2 * len` 位小写十六进制随机串（连接 ID / 请求 ID 等通用短标识形态）。
///
/// 例：`hex(8)` → 16 位小写 hex，与 `format!("{:016x}", rand::random::<u64>())` 形态一致
/// （来源：PekSendToMo 成员 ID 收编 2026-09-29）。
pub fn hex(len: usize) -> String {
    const TABLE: &[u8; 16] = b"0123456789abcdef";
    let data = bytes(len);
    let mut out = String::with_capacity(len * 2);
    for byte in data {
        out.push(TABLE[(byte >> 4) as usize] as char);
        out.push(TABLE[(byte & 0x0F) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_shape_and_uniqueness() {
        let a = token();
        let b = token();
        assert_eq!(a.len(), 22); // 16 字节 base64url 去填充
        assert!(a
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert_ne!(a, b);
    }

    #[test]
    fn hex_shape_and_charset() {
        let id = hex(8);
        assert_eq!(id.len(), 16);
        assert!(id
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_ne!(hex(8), hex(8));
        assert_eq!(hex(0), "");
    }
}
