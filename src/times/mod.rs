use std::time::{SystemTime, UNIX_EPOCH};

pub fn foo() {
    println!("Hello from times!");
}

pub fn gettimestamp() -> u64 {
    // 返回不带毫秒的时间戳
    let now = SystemTime::now();
    let since_epoch = now.duration_since(UNIX_EPOCH).expect("Time Went backwards");
    let timestamp = since_epoch.as_secs();

    timestamp
}

pub fn getmilltimestamp() -> u128 {
    // 返回带毫秒的时间戳
    let now = SystemTime::now();
    let since_epoch = now.duration_since(UNIX_EPOCH).expect("Time Went backwards");
    let timestamp = since_epoch.as_millis();

    timestamp
}
