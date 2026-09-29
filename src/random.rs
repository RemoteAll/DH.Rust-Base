//! 安全随机（OS 熵；`rand::thread_rng`）。
//!
//! 来源：跨 Agent 传输令牌等安全凭证场景（2026-09-29）；区别于应用层的"非安全随机后缀"（时间+进程混淆）。

use rand::RngCore;

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

/// base64url 编码（无填充；`+`→`-`、`/`→`_`）。
fn base64_url_no_pad(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((n >> 6) & 63) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(TABLE[(n & 63) as usize] as char);
        }
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
}
