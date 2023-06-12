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
