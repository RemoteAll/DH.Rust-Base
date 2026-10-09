use std::time::{SystemTime, UNIX_EPOCH};

use chrono::NaiveDateTime;

pub fn gettimestamp() -> u64 {
    // 返回不带毫秒的时间戳
    let now = SystemTime::now();
    let since_epoch = now.duration_since(UNIX_EPOCH).expect("Time Went backwards");

    since_epoch.as_secs()
}

pub fn getmilltimestamp() -> u128 {
    // 返回带毫秒的时间戳
    let now = SystemTime::now();
    let since_epoch = now.duration_since(UNIX_EPOCH).expect("Time Went backwards");

    since_epoch.as_millis()
}

/// 时间文本格式（秒级）。
pub const DATETIME_SECONDS_FORMAT: &str = "%Y-%m-%d %H:%M:%S";

/// XCode 数据库时间文本的小数秒位数（C# XCode 写入数据库用 7 位）。
pub const DATETIME_FRACTION_DIGITS: usize = 7;

/// 时间 → 文本（可指定小数秒位数，范围 0–9）。
///
/// chrono 的 `%.Nf` 仅支持 3/6/9 位，这里按位数手动拼接小数部分，
/// 保证 7 位（XCode 数据库格式）与 3 位（Redis 负载格式）都能精确输出。
pub fn format_datetime_with_digits(value: &NaiveDateTime, digits: usize) -> String {
    let digits = digits.min(9);
    if digits == 0 {
        return value.format(DATETIME_SECONDS_FORMAT).to_string();
    }

    let nanos = value.and_utc().timestamp_subsec_nanos();
    let fraction = nanos / 10u32.pow((9 - digits) as u32);
    format!(
        "{}.{fraction:0width$}",
        value.format(DATETIME_SECONDS_FORMAT),
        width = digits
    )
}

/// 时间 → XCode 兼容文本（7 位小数秒，如 `2026-09-26 18:01:02.1230000`，与 C# 写入数据库一致）。
pub fn format_datetime(value: &NaiveDateTime) -> String {
    format_datetime_with_digits(value, DATETIME_FRACTION_DIGITS)
}

/// 时间 → 毫秒精度文本（3 位小数秒，如 `2026-09-26 18:01:02.123`），
/// 对齐 C# `DateTime.ToString("yyyy-MM-dd HH:mm:ss.fff")`（Redis 负载等场景）。
pub fn format_datetime_ms(value: &NaiveDateTime) -> String {
    format_datetime_with_digits(value, 3)
}

/// 文本 → 时间（兼容 C# 写入的多种格式：含/不含小数秒、ISO 8601、纯日期）。
pub fn parse_datetime(text: &str) -> Option<NaiveDateTime> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }

    const FORMATS: [&str; 5] = [
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%d",
    ];
    for fmt in FORMATS {
        if let Ok(v) = NaiveDateTime::parse_from_str(text, fmt) {
            return Some(v);
        }
    }

    // 带 Z / 时区偏移的 ISO8601（InfluxDB、ClickHouse 等常见输出），统一换算为 UTC 朴素时间
    if let Ok(v) = chrono::DateTime::parse_from_rfc3339(text) {
        return Some(v.naive_utc());
    }

    // 纯日期需要补零点
    chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
}

/// 现在（UTC 朴素时间）。
pub fn now_utc() -> NaiveDateTime {
    chrono::Utc::now().naive_utc()
}

/// 现在（UTC）→ `yyyy-MM-dd HH:mm:ss`（对齐 C# `DateTime.UtcNow.ToString("yyyy-MM-dd HH:mm:ss")`）。
pub fn now_utc_str() -> String {
    format_datetime_with_digits(&now_utc(), 0)
}

/// 时间 → System.Text.Json 形态文本（`yyyy-MM-ddTHH:mm:ss`，100ns 精度内尾零裁剪）。
///
/// 对齐 C# `System.Text.Json` 序列化 `DateTime`（本地）的输出：
/// `2026-10-01T12:33:04.12`（小数尾零裁剪）；无小数时不输出小数点。
pub fn format_datetime_stj(value: &NaiveDateTime) -> String {
    use chrono::{Datelike, Timelike};

    let mut s = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        value.year(),
        value.month(),
        value.day(),
        value.hour(),
        value.minute(),
        value.second()
    );

    let frac = value.and_utc().timestamp_subsec_nanos() / 100;
    if frac != 0 {
        let mut f = format!("{frac:07}");
        while f.ends_with('0') {
            f.pop();
        }
        s.push('.');
        s.push_str(&f);
    }
    s
}

/// Unix 时间戳（秒+纳秒）→ 本地时间 → System.Text.Json 形态文本。
pub fn format_timestamp_stj(secs: i64, nanos: u32) -> String {
    let dt = chrono::DateTime::from_timestamp(secs, nanos)
        .map(|d| d.with_timezone(&chrono::Local))
        .unwrap_or_else(chrono::Local::now);
    format_datetime_stj(&dt.naive_local())
}

/// 系统时间 → 本地时间 → System.Text.Json 形态文本。
pub fn format_system_time_stj(t: SystemTime) -> String {
    let (secs, nanos) = match t.duration_since(UNIX_EPOCH) {
        Ok(d) => (d.as_secs() as i64, d.subsec_nanos()),
        Err(e) => {
            // 早于纪元（罕见）：按负偏移换算
            let d = e.duration();
            (-(d.as_secs() as i64), d.subsec_nanos())
        }
    };
    format_timestamp_stj(secs, nanos)
}

/// Unix 秒 → 本地时区文本（`%Y-%m-%d %H:%M:%S`；None/越界返回空串）。
///
/// 与 [`build_time_text!`](crate::build_time_text!) 配套：消费方 `build.rs` 注入 Unix 秒
/// （`cargo:rustc-env=XXX=<秒>`），运行时代码经宏取用。
pub fn unix_local_text(secs: Option<i64>) -> String {
    secs.and_then(|s| chrono::DateTime::from_timestamp(s, 0))
        .map(|utc| {
            utc.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_default()
}

/// 构建时间文本（在**调用方 crate** 展开 `option_env!`）。
///
/// `option_env!` 是编译期宏——若把本能力做成普通函数放进库，环境变量会在**库编译时**
/// 求值（恒为空）；宏在调用方展开，才能读到消费方 `build.rs` 注入的值。
///
/// 用法：`build.rs` 注入 `cargo:rustc-env=XXX=<Unix 秒>`；代码中
/// `dhrust::build_time_text!("XXX")` → 本地时区文本（如 `2026-10-09 09:23:06`；缺失/非法为空串）。
#[macro_export]
macro_rules! build_time_text {
    ($env:literal) => {
        $crate::times::unix_local_text(option_env!($env).and_then(|s| s.parse::<i64>().ok()))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2026, 9, 26)
            .unwrap()
            .and_hms_milli_opt(18, 1, 2, 123)
            .unwrap()
    }

    #[test]
    fn datetime_format_matches_csharp() {
        let dt = sample();
        assert_eq!(format_datetime(&dt), "2026-09-26 18:01:02.1230000");
        assert_eq!(format_datetime_ms(&dt), "2026-09-26 18:01:02.123");
        assert_eq!(format_datetime_with_digits(&dt, 0), "2026-09-26 18:01:02");
        assert_eq!(
            format_datetime_with_digits(&dt, 6),
            "2026-09-26 18:01:02.123000"
        );
    }

    #[test]
    fn parse_datetime_variants() {
        let dt = sample();
        assert_eq!(parse_datetime("2026-09-26 18:01:02.1230000"), Some(dt));
        assert!(parse_datetime("2026-09-26 18:01:02").is_some());
        assert!(parse_datetime("2026-09-26T18:01:02Z").is_some());
        assert!(parse_datetime("2026-09-26").is_some());
        assert!(parse_datetime("").is_none());
        assert!(parse_datetime("不是时间").is_none());
    }

    #[test]
    fn unix_local_text_formats_and_handles_none() {
        assert_eq!(unix_local_text(None), "");
        let text = unix_local_text(Some(1_700_000_000));
        assert_eq!(text.len(), 19);
        // 与 chrono 本地换算一致（时区无关断言）
        let expected = chrono::DateTime::from_timestamp(1_700_000_000, 0)
            .unwrap()
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        assert_eq!(text, expected);
        // 宏：库自身构建未注入该变量 → 空串（消费方注入后才有值）
        assert_eq!(crate::build_time_text!("DHRUST_TEST_MISSING_UNIX"), "");
    }

    #[test]
    fn stj_format_matches_dotnet() {
        // 毫秒尾零裁剪（120ms → ".12"）
        let dt = chrono::NaiveDate::from_ymd_opt(2026, 10, 1)
            .unwrap()
            .and_hms_milli_opt(12, 33, 4, 120)
            .unwrap();
        assert_eq!(format_datetime_stj(&dt), "2026-10-01T12:33:04.12");

        // 无小数时不输出小数点
        let dt = chrono::NaiveDate::from_ymd_opt(2026, 10, 1)
            .unwrap()
            .and_hms_opt(12, 33, 4)
            .unwrap();
        assert_eq!(format_datetime_stj(&dt), "2026-10-01T12:33:04");

        // now_utc_str 形状
        assert_eq!(now_utc_str().len(), 19);

        // 系统时间路径与 chrono 本地换算一致（时区无关断言）
        let secs = 1_700_000_000i64;
        let expected = chrono::DateTime::from_timestamp(secs, 0)
            .unwrap()
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string();
        let st = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs as u64);
        assert_eq!(format_system_time_stj(st), expected);
        assert_eq!(format_timestamp_stj(secs, 0), expected);
    }
}
