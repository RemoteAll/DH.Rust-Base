use md5::Md5;
use rand::Rng;
use sha1::{Digest, Sha1};

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
    let digest = Md5::digest(text.as_bytes());
    let mut out = String::with_capacity(32);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
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
}
