// pub fn create_signature() -> String {

// }

use rand::Rng;

/// 获取随机数
pub fn getrand(length: usize) -> String {
    let rng = rand::thread_rng();
    let random_string: String = rng
        .sample_iter(&rand::distributions::Alphanumeric)
        .take(length)
        .map(char::from)
        .collect();
    println!("Random string: {}", random_string);

    random_string
}
