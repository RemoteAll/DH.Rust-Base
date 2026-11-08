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

/// 时间 → **秒级**文本（`yyyy-MM-dd HH:mm:ss`，无小数秒）。
///
/// 与 [`format_datetime`] 的区别要说清楚，两者**输出不同**：
/// - [`format_datetime`]：7 位小数秒（`2026-09-26 18:01:02.1230000`），写 XCode 库列时用；
/// - 本函数：**严格** `%Y-%m-%d %H:%M:%S`，用于业务/界面文本、以及要求秒级严格口径的
///   字符串比较与 `parse_from_str("%Y-%m-%d %H:%M:%S")` 解析。
///
/// 2026-11-08 教训：HLZX.EIP 把「过期时间/日志截止时间」等改用 [`format_datetime`] 后，
/// 值里多了 `.1234567`，导致严格解析取到 0 —— **要秒级就必须显式用本函数**，不能靠默认。
pub fn format_datetime_seconds(value: &NaiveDateTime) -> String {
    format_datetime_with_digits(value, 0)
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

/// 宽松时间解析：`parse_datetime` + C# `Convert.ToDateTime` 常见斜杠格式 + Unix 秒/毫秒数字。
///
/// 用于读取 C# 写出的数据文件（中文文化下 `DateTime.ToString()` 输出 `2024/1/5 14:30:00`）。
pub fn parse_datetime_lax(text: &str) -> Option<NaiveDateTime> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if let Some(value) = parse_datetime(text) {
        return Some(value);
    }

    const DATETIME_FORMATS: [&str; 4] = [
        "%Y/%m/%d %H:%M:%S",
        "%Y/%m/%d %H:%M",
        "%Y/%m/%d %H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.3f",
    ];
    for fmt in DATETIME_FORMATS {
        if let Ok(value) = NaiveDateTime::parse_from_str(text, fmt) {
            return Some(value);
        }
    }
    if let Ok(date) = chrono::NaiveDate::parse_from_str(text, "%Y/%m/%d") {
        return date.and_hms_opt(0, 0, 0);
    }
    text.parse::<i64>().ok().and_then(from_unix_timestamp)
}

/// Unix 秒/毫秒 → 本地时间（`|value| ≥ 1e11` 视为毫秒；越界返回 `None`）。
pub fn from_unix_timestamp(value: i64) -> Option<NaiveDateTime> {
    let (secs, nanos) = if value.abs() >= 100_000_000_000 {
        (
            value.div_euclid(1000),
            value.rem_euclid(1000) as u32 * 1_000_000,
        )
    } else {
        (value, 0)
    };
    chrono::DateTime::from_timestamp(secs, nanos).map(|utc| {
        use chrono::TimeZone;
        chrono::Local
            .timestamp_opt(utc.timestamp(), utc.timestamp_subsec_nanos())
            .single()
            .map(|local| local.naive_local())
            .unwrap_or_else(|| utc.naive_utc())
    })
}

/// 解析“年 / 年月 / 年月日”模式并返回**闭区间**（按日精度；供日志/报表分组查询）。
///
/// 例：`2024` → `[2024-01-01 00:00:00, 2024-12-31 23:59:59]`；
/// `2024-02`、`2024-01-31` 同理；格式不符或日期越界返回 `None`。
pub fn date_range_of(pattern: &str) -> Option<(NaiveDateTime, NaiveDateTime)> {
    let pattern = pattern.trim();
    let parts: Vec<&str> = pattern.split('-').collect();
    let (start, end) = match parts.as_slice() {
        [year] if year.len() == 4 && year.chars().all(|c| c.is_ascii_digit()) => {
            let year: i32 = year.parse().ok()?;
            (
                chrono::NaiveDate::from_ymd_opt(year, 1, 1)?,
                chrono::NaiveDate::from_ymd_opt(year, 12, 31)?,
            )
        }
        [year, month]
            if year.len() == 4
                && month.len() == 2
                && year.chars().all(|c| c.is_ascii_digit())
                && month.chars().all(|c| c.is_ascii_digit()) =>
        {
            let year: i32 = year.parse().ok()?;
            let month: u32 = month.parse().ok()?;
            let first = chrono::NaiveDate::from_ymd_opt(year, month, 1)?;
            let next = if month == 12 {
                chrono::NaiveDate::from_ymd_opt(year + 1, 1, 1)?
            } else {
                chrono::NaiveDate::from_ymd_opt(year, month + 1, 1)?
            };
            (first, next.pred_opt()?)
        }
        [year, month, day]
            if year.len() == 4
                && month.len() == 2
                && day.len() == 2
                && year.chars().all(|c| c.is_ascii_digit())
                && month.chars().all(|c| c.is_ascii_digit())
                && day.chars().all(|c| c.is_ascii_digit()) =>
        {
            let date = chrono::NaiveDate::from_ymd_opt(
                year.parse().ok()?,
                month.parse().ok()?,
                day.parse().ok()?,
            )?;
            (date, date)
        }
        _ => return None,
    };
    Some((start.and_hms_opt(0, 0, 0)?, end.and_hms_opt(23, 59, 59)?))
}

/// 现在（UTC 朴素时间）。
pub fn now_utc() -> NaiveDateTime {
    chrono::Utc::now().naive_utc()
}

/// 现在（UTC）→ `yyyy-MM-dd HH:mm:ss`（对齐 C# `DateTime.UtcNow.ToString("yyyy-MM-dd HH:mm:ss")`）。
pub fn now_utc_str() -> String {
    format_datetime_with_digits(&now_utc(), 0)
}

/// 现在（UTC）→ ISO 8601 `yyyy-MM-ddTHH:mm:ssZ`（阿里云 API 等签名/请求口径）。
pub fn iso8601_utc() -> String {
    now_utc().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Unix 秒 → UTC 日期 `yyyy-MM-dd`（越界返回 `None`）。
pub fn utc_date_of(secs: i64) -> Option<String> {
    chrono::DateTime::from_timestamp(secs, 0).map(|dt| dt.format("%Y-%m-%d").to_string())
}

/// 现在（UTC）→ `(Unix 秒, yyyy-MM-dd)`（腾讯云等需要同时取时间戳与日期）。
pub fn utc_now_parts() -> (i64, String) {
    let now = chrono::Utc::now();
    (now.timestamp(), now.format("%Y-%m-%d").to_string())
}

/// 现在（**本地时区**）→ `%Y-%m-%d %H:%M:%S`。
///
/// 业务表里的"创建/更新时间"列一律用本地时间文本（与 PHP/C# 端写入口径一致，
/// 便于直接比对），故消费方几乎都要这一行；此前各项目各自复制 `now_text()`。
pub fn now_local_text() -> String {
    chrono::Local::now()
        .naive_local()
        .format(DATETIME_SECONDS_FORMAT)
        .to_string()
}

/// 现在（**本地时区**）→ `%Y-%m-%d %H:%M:%S%.3f`（毫秒精度，审计/日志用）。
pub fn now_local_ms_text() -> String {
    let now = chrono::Local::now().naive_local();
    format_datetime_with_digits(&now, 3)
}

/// 本地日期 → `yyyy-MM-dd` 文本（报表/日志的「今天」口径）。
///
/// 与 [`now_local_text`] 的日期部分一致，但只取日期，避免各处手写
/// `chrono::Local::now().format("%Y-%m-%d")`（2026-11-08 全盘扫到 48 处同类自写格式化）。
pub fn local_date_text() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// 本地日期 → `yyyyMMdd` 整数键（日汇总/配额/「今日」缓存键）。
///
/// 例：`20261108`。解析失败不可能发生（格式固定），仍按 `0` 兜底保持总函数。
pub fn local_date_key() -> i64 {
    chrono::Local::now()
        .format("%Y%m%d")
        .to_string()
        .parse()
        .unwrap_or(0)
}

/// 本地时间 → 紧凑秒级时间戳 `yyyyMMddHHmmss`（文件名/流水号用）。
///
/// 例：`20261108143012`。
pub fn local_stamp_secs() -> String {
    chrono::Local::now().format("%Y%m%d%H%M%S").to_string()
}

/// 本地时间 → 紧凑毫秒级时间戳 `yyyyMMddHHmmssSSS`（同一秒内需要唯一时用）。
///
/// 例：`20261108143012507`。
pub fn local_stamp_millis() -> String {
    chrono::Local::now().format("%Y%m%d%H%M%S%3f").to_string()
}

/// 今日时间范围 `[今天零点, 明天零点)`（**半开区间**，日汇总/配额比较用）。
///
/// 与 [`date_range_of`] 的区别：那个按 `yyyy`/`yyyy-MM`/`yyyy-MM-dd` 模式解析并返回
/// **闭区间**（`23:59:59` 边界会被漏掉）;本函数固定「今天」，且右端开区间 ——
/// `CreateTime >= start AND CreateTime < end` 才能覆盖当天最后一毫秒。
pub fn today_range() -> (NaiveDateTime, NaiveDateTime) {
    let today = chrono::Local::now().date_naive();
    let start = today.and_hms_opt(0, 0, 0).unwrap_or_default();
    let end = today
        .succ_opt()
        .and_then(|date| date.and_hms_opt(0, 0, 0))
        .unwrap_or(start);
    (start, end)
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

    /// 紧凑日期键 / 时间戳 / 今日半开区间（收编各项目自写的 `%Y%m%d` 系列）。
    #[test]
    fn local_date_and_stamp_helpers() {
        let today = chrono::Local::now().date_naive();
        let expect_key: i64 = today.format("%Y%m%d").to_string().parse().unwrap();
        assert_eq!(local_date_key(), expect_key);
        assert_eq!(local_date_text(), today.format("%Y-%m-%d").to_string());

        // 秒级 14 位、毫秒级 17 位，且都是纯数字
        let secs = local_stamp_secs();
        assert_eq!(secs.len(), 14, "{secs}");
        assert!(secs.chars().all(|c| c.is_ascii_digit()), "{secs}");
        let millis = local_stamp_millis();
        assert_eq!(millis.len(), 17, "{millis}");
        assert!(millis.chars().all(|c| c.is_ascii_digit()), "{millis}");
        assert!(millis.starts_with(&secs[..12]), "{millis} vs {secs}");

        // 今日范围：左闭右开，长度正好一天
        let (start, end) = today_range();
        assert_eq!(start.date(), today);
        assert_eq!(end.date(), today.succ_opt().unwrap());
        assert_eq!(end - start, chrono::TimeDelta::days(1));
        // 边界语义：今天 23:59:59.999 属于本区间，明天 00:00 不属于
        let last_moment = end - chrono::TimeDelta::milliseconds(1);
        assert!(last_moment >= start && last_moment < end);
        assert!(end >= end);
    }

    #[test]
    fn seconds_and_fraction_formats_are_distinct() {
        // 这两个函数**输出不同**，混用会污染库里的时间文本（2026-11-08 HLZX.EIP 踩过）：
        // 用 format_datetime 写「过期时间」→ 值里多出 `.1234567` → 严格解析取到 0
        let t = chrono::NaiveDate::from_ymd_opt(2026, 11, 8)
            .unwrap()
            .and_hms_milli_opt(15, 4, 5, 123)
            .unwrap();

        let secs = format_datetime_seconds(&t);
        assert_eq!(secs, "2026-11-08 15:04:05");
        assert_eq!(secs.len(), 19, "秒级必须无小数：{secs}");
        // 秒级结果必须能被严格 `%Y-%m-%d %H:%M:%S` 解析（format_datetime 做不到）
        assert!(chrono::NaiveDateTime::parse_from_str(&secs, DATETIME_SECONDS_FORMAT).is_ok());

        let fraction = format_datetime(&t);
        assert_eq!(fraction, "2026-11-08 15:04:05.1230000");
        assert!(
            chrono::NaiveDateTime::parse_from_str(&fraction, DATETIME_SECONDS_FORMAT).is_err(),
            "7 位小数版本不应被秒级严格解析接受"
        );
    }

    #[test]
    fn now_local_text_shapes() {
        // 秒级：`yyyy-MM-dd HH:mm:ss`（19 字符、无小数）
        let text = now_local_text();
        assert_eq!(text.len(), 19);
        assert!(parse_datetime(&text).is_some());
        assert!(!text.contains('.'));

        // 毫秒级：`yyyy-MM-dd HH:mm:ss.fff`（23 字符）
        let ms = now_local_ms_text();
        assert_eq!(ms.len(), 23);
        assert_eq!(ms.len() - ms.rfind('.').unwrap() - 1, 3);

        // 与本地时间同一分钟（跨分钟执行时允许差 1）
        let now = chrono::Local::now().naive_local();
        let minutes_apart = (parse_datetime(&text).unwrap() - now).num_minutes().abs();
        assert!(minutes_apart <= 1);
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
    fn parse_datetime_lax_covers_csharp_formats_and_timestamps() {
        let expected = chrono::NaiveDate::from_ymd_opt(2024, 1, 5)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        assert_eq!(parse_datetime_lax("2024/1/5 14:30:00"), Some(expected));
        assert_eq!(parse_datetime_lax("2024/01/05 14:30:00"), Some(expected));
        assert_eq!(
            parse_datetime_lax("2024/1/5"),
            Some(
                chrono::NaiveDate::from_ymd_opt(2024, 1, 5)
                    .unwrap()
                    .and_hms_opt(0, 0, 0)
                    .unwrap()
            )
        );
        // 原格式仍可用
        assert_eq!(parse_datetime_lax("2024-01-05 14:30:00"), Some(expected));
        // Unix 秒 / 毫秒 → 同一本地时刻（时区无关断言）
        let secs = expected.and_utc().timestamp();
        let expected_local = from_unix_timestamp(secs).unwrap();
        assert_eq!(parse_datetime_lax(&secs.to_string()), Some(expected_local));
        let millis = expected.and_utc().timestamp_millis();
        assert_eq!(
            parse_datetime_lax(&millis.to_string()),
            Some(from_unix_timestamp(millis).unwrap())
        );
        assert_eq!(parse_datetime_lax(""), None);
        assert_eq!(parse_datetime_lax("abc"), None);
    }

    #[test]
    fn date_range_of_year_month_day() {
        let (start, end) = date_range_of("2024").unwrap();
        assert_eq!(
            start.format("%Y-%m-%d %H:%M:%S").to_string(),
            "2024-01-01 00:00:00"
        );
        assert_eq!(
            end.format("%Y-%m-%d %H:%M:%S").to_string(),
            "2024-12-31 23:59:59"
        );

        let (start, end) = date_range_of("2024-02").unwrap();
        assert_eq!(start.format("%Y-%m-%d").to_string(), "2024-02-01");
        assert_eq!(end.format("%Y-%m-%d").to_string(), "2024-02-29");

        let (start, end) = date_range_of("2024-01-31").unwrap();
        assert_eq!(
            start.format("%Y-%m-%d %H:%M:%S").to_string(),
            "2024-01-31 00:00:00"
        );
        assert_eq!(
            end.format("%Y-%m-%d %H:%M:%S").to_string(),
            "2024-01-31 23:59:59"
        );

        assert!(date_range_of("2024-1").is_none());
        assert!(date_range_of("abc").is_none());
        assert!(date_range_of("2024-13").is_none());
        assert!(date_range_of("2024-02-30").is_none());
    }

    #[test]
    fn utc_helpers_shape() {
        let iso = iso8601_utc();
        assert_eq!(iso.len(), 20);
        assert!(iso.ends_with('Z'));
        assert!(parse_datetime(&iso).is_some());
        assert_eq!(utc_date_of(1_700_000_000).as_deref(), Some("2023-11-14"));
        assert!(utc_date_of(i64::MAX).is_none());
        let (secs, date) = utc_now_parts();
        assert!(secs > 1_700_000_000);
        assert_eq!(date.len(), 10);
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
