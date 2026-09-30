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
}
