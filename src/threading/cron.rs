//! 轻量级 Cron 表达式，语义与 DH.NCore `NewLife.Threading.Cron` 对齐。
//!
//! 基本构成：秒 + 分 + 时 + 天 + 月 + 星期（缺省补 `*`）。
//! 每段构成：
//! - `*` 所有可能的值
//! - `,` 列出枚举值
//! - `-` 范围，横杠表示的一个区间
//! - `/` 指定数值的增量，间隔多少选一个
//! - `?` 不指定值，等价于 `*`
//! - `#` 确定每个月第几个星期几，`L` 表示倒数，仅星期域支持
//!
//! 星期部分采用 Linux 与 .NET 风格：`0` 表示周日、`1` 表示周一。
//! 可设置 [`Cron::sunday`] 为 `1`，则表示 `1` 为周日、`2` 为周一。
//!
//! # 示例
//!
//! ```
//! use dhrust::threading::Cron;
//! use chrono::NaiveDate;
//!
//! // 每两秒一次
//! let cron = Cron::parse("*/2").unwrap();
//! let dt = NaiveDate::from_ymd_opt(2026, 9, 28).unwrap().and_hms_opt(0, 0, 0).unwrap();
//! assert!(cron.is_time(&dt));
//! assert_eq!(
//!     cron.get_next(dt).unwrap(),
//!     dt + chrono::Duration::seconds(2)
//! );
//! ```

use chrono::{Datelike, Duration, Months, NaiveDate, NaiveDateTime, Timelike};

/// 轻量级 Cron 表达式。
#[derive(Clone, Debug, Default)]
pub struct Cron {
    /// 秒数集合
    pub seconds: Vec<i32>,
    /// 分钟集合
    pub minutes: Vec<i32>,
    /// 小时集合
    pub hours: Vec<i32>,
    /// 日期集合
    pub days_of_month: Vec<i32>,
    /// 月份集合
    pub months: Vec<i32>,
    /// 星期集合。key 是星期数，value 是第几个，负数表示倒数
    pub days_of_week: std::collections::BTreeMap<i32, i32>,
    /// 星期天偏移量。周日对应的数字，默认 0。1 表示周日时，2 表示周一
    pub sunday: i32,
    expression: String,
}

impl Cron {
    /// 分析表达式，失败返回 `None`。
    ///
    /// 对应 C# `new Cron(expression)`，解析失败等价于 C# 的 `Parse` 返回 `false`。
    pub fn parse(expression: &str) -> Option<Cron> {
        let mut cron = Cron::default();
        if !cron.try_parse(expression) {
            return None;
        }
        Some(cron)
    }

    /// 分析表达式并写入当前实例。
    pub fn try_parse(&mut self, expression: &str) -> bool {
        // C# 使用 Split([' ']) 后去除空项，这里保持一致（只按空格切分）
        let ss: Vec<&str> = expression.split(' ').filter(|x| !x.is_empty()).collect();
        if ss.is_empty() {
            return false;
        }

        let Some(seconds) = try_parse_field(ss[0], 0, 60) else {
            return false;
        };
        let Some(minutes) = try_parse_field(ss.get(1).copied().unwrap_or("*"), 0, 60) else {
            return false;
        };
        let Some(hours) = try_parse_field(ss.get(2).copied().unwrap_or("*"), 0, 24) else {
            return false;
        };
        let Some(days_of_month) = try_parse_field(ss.get(3).copied().unwrap_or("*"), 1, 32) else {
            return false;
        };
        let Some(months) = try_parse_field(ss.get(4).copied().unwrap_or("*"), 1, 13) else {
            return false;
        };
        let Some(days_of_week) = try_parse_week(ss.get(5).copied().unwrap_or("*"), 0, 7) else {
            return false;
        };

        self.seconds = seconds;
        self.minutes = minutes;
        self.hours = hours;
        self.days_of_month = days_of_month;
        self.months = months;
        self.days_of_week = days_of_week;
        self.expression = expression.to_string();

        true
    }

    /// 原始表达式。
    pub fn expression(&self) -> &str {
        &self.expression
    }

    /// 指定时间是否位于表达式之内。
    pub fn is_time(&self, time: &NaiveDateTime) -> bool {
        // 基础时间判断
        if !self.seconds.contains(&(time.second() as i32))
            || !self.minutes.contains(&(time.minute() as i32))
            || !self.hours.contains(&(time.hour() as i32))
            || !self.days_of_month.contains(&(time.day() as i32))
            || !self.months.contains(&(time.month() as i32))
        {
            return false;
        }

        let w = time.weekday().num_days_from_sunday() as i32 + self.sunday;
        let Some(&index) = self.days_of_week.get(&w) else {
            return false;
        };

        // 第几个星期几判断
        if index > 0 {
            let mut index = index;
            let start = NaiveDate::from_ymd_opt(time.year(), time.month(), 1)
                .expect("valid first day of month");
            let mut dt = start;
            while dt <= time.date() {
                if dt.weekday() == time.weekday() {
                    index -= 1;
                }
                dt = dt.succ_opt().expect("valid next day");
            }
            if index != 0 {
                return false;
            }
        } else if index < 0 {
            let mut index = index;
            let (y, m) = if time.month() == 12 {
                (time.year() + 1, 1)
            } else {
                (time.year(), time.month() + 1)
            };
            let first_next = NaiveDate::from_ymd_opt(y, m, 1).expect("valid first day of month");
            let mut dt = first_next.pred_opt().expect("valid last day of month");
            while dt >= time.date() {
                if dt.weekday() == time.weekday() {
                    index += 1;
                }
                dt = dt.pred_opt().expect("valid previous day");
            }
            if index != 0 {
                return false;
            }
        }

        true
    }

    /// 获得指定时间之后的下一次执行时间，不含指定时间。
    ///
    /// 如果指定时间带有毫秒，则向前对齐。如 `09:14:00.123` 的 `"15 * * *"` 下一次是 `10:15` 而不是 `09:15`。
    ///
    /// # 参数
    /// - `time`：从该时间秒的下一秒算起的下一个执行时间
    ///
    /// 下一次执行时间（秒级）；如果没有匹配则返回 `None`（对应 C# 的 `DateTime.MinValue`）。
    pub fn get_next(&self, time: NaiveDateTime) -> Option<NaiveDateTime> {
        // 如果指定时间带有毫秒，则向前对齐。如09:14.123格式化为09:15，计算下一次就从09:16开始
        let start = floor_to_second(time);
        let start = if start != time {
            start + Duration::seconds(2)
        } else {
            start + Duration::seconds(1)
        };

        // 设置末尾，避免死循环越界
        let end = time
            .checked_add_months(Months::new(12))
            .expect("valid end of scan");
        let mut dt = start;
        while dt < end {
            if self.is_time(&dt) {
                return Some(dt);
            }
            dt += Duration::seconds(1);
        }

        None
    }

    /// 获得与指定时间时间符合表达式的最远时间（秒级）。
    pub fn get_previous(&self, time: NaiveDateTime) -> Option<NaiveDateTime> {
        // 如果指定时间带有毫秒，则向前对齐
        let start = floor_to_second(time);
        let start = if start != time {
            start - Duration::seconds(1)
        } else {
            start - Duration::seconds(2)
        };

        // 设置末尾，避免死循环越界
        let end = time
            .checked_sub_months(Months::new(12))
            .expect("valid end of scan");
        let mut last = false;
        let mut dt = start;
        while dt > end {
            if !last {
                last = self.is_time(&dt); // 找真值
            } else if !self.is_time(&dt) {
                // 真值找到了找假值，减多了，返回真值
                return Some(dt + Duration::seconds(1));
            }
            dt -= Duration::seconds(1);
        }

        None
    }

    /// 对一批 Cron 表达式，获取下一次执行时间。
    pub fn get_next_multi(crons: &[Cron], time: NaiveDateTime) -> Option<NaiveDateTime> {
        crons.iter().filter_map(|c| c.get_next(time)).min()
    }

    /// 对一批 Cron 表达式，获取前一次执行时间。
    pub fn get_previous_multi(crons: &[Cron], time: NaiveDateTime) -> Option<NaiveDateTime> {
        crons.iter().filter_map(|c| c.get_previous(time)).max()
    }
}

impl std::fmt::Display for Cron {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.expression.is_empty() {
            f.write_str("Cron")
        } else {
            f.write_str(&self.expression)
        }
    }
}

/// 按秒向下取整（对齐 C# `Convert.Trim(value, "s")` 的 ticks 截断）。
fn floor_to_second(time: NaiveDateTime) -> NaiveDateTime {
    time.with_nanosecond(0).expect("valid second")
}

/// NewLife 的 `ToInt()` 语义：解析失败按 0 处理。
fn to_int_or_0(text: &str) -> i32 {
    text.trim().parse::<i32>().unwrap_or(0)
}

/// 分析单段表达式（秒/分/时/天/月）。
///
/// 与 C# `Cron.TryParse` 逐行对应，包含以下细节：
/// - 固定整数走快速路径（不校验范围，如 `99` 解析为 `[99]`）；
/// - 步进值 `<= 0` 视为非法（防止循环永不推进）；
/// - 范围 `a-b` 时 `max = b + 1`，且不会重置步进值；
/// - `*` 与 `?` 均从 0 开始。
fn try_parse_field(value: &str, start: i32, max: i32) -> Option<Vec<i32>> {
    // 固定值，最为常见，优先计算
    if let Ok(n) = value.trim().parse::<i32>() {
        return Some(vec![n]);
    }

    let mut rs = Vec::new();

    // 递归处理混合值
    if value.contains(',') {
        for item in value.split(',') {
            let arr = try_parse_field(item, start, max)?;
            rs.extend(arr);
        }
        return Some(rs);
    }

    // 步进值
    let mut step = 1;
    let mut value_rest = value;
    let mut max = max;
    if let Some(p) = value.find('/') {
        if p > 0 {
            step = to_int_or_0(&value[p + 1..]);
            // 步进值为 0 会导致下方循环永不推进而挂死
            if step <= 0 {
                return None;
            }
            value_rest = &value[..p];
        }
    }

    // 连续范围
    let s = if value_rest == "*" || value_rest == "?" {
        0
    } else if value_rest.find('-').is_some_and(|p| p > 0) {
        let p = value_rest.find('-').expect("checked");
        let s = to_int_or_0(&value_rest[..p]);
        max = to_int_or_0(&value_rest[p + 1..]) + 1;
        s
    } else if let Ok(n) = value_rest.trim().parse::<i32>() {
        n
    } else {
        return None;
    };

    let mut i = s;
    while i < max {
        if i >= start {
            rs.push(i);
        }
        i += step;
    }

    Some(rs)
}

/// 分析星期段表达式。失败返回 `None`（对应 C# 中区间重复 key 抛出异常的情况）。
///
/// 与 C# 一致，固定值与逗号合并使用索引器覆盖，只有循环展开（区间/步进）在
/// 重复星期数时失败；因此 `5,5` 合法，而 `1-3,2-4` 失败。
fn try_parse_week(
    value: &str,
    start: i32,
    max: i32,
) -> Option<std::collections::BTreeMap<i32, i32>> {
    let mut weeks = std::collections::BTreeMap::new();
    if parse_week_into(value, start, max, &mut weeks) {
        Some(weeks)
    } else {
        None
    }
}

fn parse_week_into(
    value: &str,
    start: i32,
    max: i32,
    weeks: &mut std::collections::BTreeMap<i32, i32>,
) -> bool {
    // 固定值，最为常见，优先计算
    if let Ok(n) = value.trim().parse::<i32>() {
        // C# 使用索引器赋值，重复时覆盖
        weeks.insert(n, 0);
        return true;
    }

    // 递归处理混合值
    if value.contains(',') {
        for item in value.split(',') {
            if !parse_week_into(item, start, max, weeks) {
                return false;
            }
        }
        return true;
    }

    // 步进值
    let mut step = 1;
    let mut v = value;
    let mut max = max;
    if let Some(p) = value.find('/') {
        if p > 0 {
            step = to_int_or_0(&value[p + 1..]);
            // 步进值为 0 会导致下方循环永不推进而挂死
            if step <= 0 {
                return false;
            }
            v = &value[..p];
        }
    }

    // 第几个星期几
    let mut index = 0;
    if let Some(p) = v.find('#') {
        if p > 0 {
            let str_part = &v[p + 1..];
            let starts_with_l = str_part
                .get(..1)
                .is_some_and(|s| s.eq_ignore_ascii_case("L"));
            if starts_with_l {
                index = -to_int_or_0(&str_part[1..]);
            } else {
                index = to_int_or_0(str_part);
            }
            v = &v[..p];
            step = 7;
        }
    }

    // 连续范围
    let s = if v == "*" || v == "?" {
        0
    } else if v.find('-').is_some_and(|p| p > 0) {
        let p = v.find('-').expect("checked");
        let s = to_int_or_0(&v[..p]);
        max = to_int_or_0(&v[p + 1..]) + 1;
        // C# 在范围分支把步进值重置为 1
        step = 1;
        s
    } else if let Ok(n) = v.trim().parse::<i32>() {
        n
    } else {
        return false;
    };

    let mut i = s;
    while i < max {
        if i >= start {
            if weeks.contains_key(&i) {
                return false;
            }
            weeks.insert(i, index);
        }
        i += step;
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(text: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S").unwrap()
    }

    fn d(text: &str) -> NaiveDateTime {
        dt(&format!("{text} 00:00:00"))
    }

    #[test]
    fn parse_valid_expressions() {
        // 对应 C# XUnitTest.Core/Threading/CronTests.Valid
        for expression in [
            "*/2",
            "* * * * *",
            "0 * * * *",
            "0,1,2 * * * *",
            "*/2 * * * *",
            "5/20 * * * *",
            "1-4 * * * *",
            "1-55/3 * * * *",
            "1,10,20 * * * *",
            "* 1,10,20 * * *",
            "* 1-10,13,5/20 * * *",
            "*/2 ",
            "* *  * * *",
            "0 * *   * *",
            "0,1,2 *   * *  *",
            "*/2 * *  * * ",
            "5/20 *  * *  *",
            "1-4 * *  * * ",
            "1-55/3  * *  * * ",
            "1,10,20  *  * *  *",
            " * 1,10,20  * *  *",
            " * 1-10,13,5/20 *  * *",
        ] {
            assert!(Cron::parse(expression).is_some(), "解析失败：{expression}");
        }
    }

    #[test]
    fn parse_invalid_expressions() {
        assert!(Cron::parse("").is_none());
        assert!(Cron::parse("   ").is_none());
        assert!(Cron::parse("a * * * *").is_none());
        assert!(Cron::parse("*/0 * * * *").is_none()); // 步进为 0
        assert!(Cron::parse("* * * * * 1-3,2-4").is_none()); // 区间重复星期数
        assert!(Cron::parse("* * * * * 1,1").is_some()); // 固定值重复为覆盖，语义与 C# 一致
    }

    #[test]
    fn is_time_second_test() {
        let cron = Cron::parse("0 * * * *").unwrap();
        assert!(cron.is_time(&dt("2026-09-28 08:00:00")));
        assert!(!cron.is_time(&dt("2026-09-28 08:00:01")));
        assert_eq!(cron.seconds, vec![0]);

        let cron = Cron::parse("0-10 * * * *").unwrap();
        assert!(cron.is_time(&dt("2026-09-28 08:00:00")));
        assert!(cron.is_time(&dt("2026-09-28 08:00:03")));
        assert_eq!(cron.seconds.len(), 11);
        assert_eq!(cron.seconds[0], 0);
        assert_eq!(cron.seconds[10], 10);

        let cron = Cron::parse("*/2 * * * *").unwrap();
        assert!(cron.is_time(&dt("2026-09-28 08:00:00")));
        assert!(cron.is_time(&dt("2026-09-28 08:00:02")));
        assert!(!cron.is_time(&dt("2026-09-28 08:00:03")));
        assert_eq!(cron.seconds.len(), 30);
    }

    #[test]
    fn get_next_second_test() {
        let cron = Cron::parse("5/20 * * * *").unwrap();
        assert!(cron.is_time(&dt("2026-09-28 08:00:05")));
        assert!(cron.is_time(&dt("2026-09-28 08:00:25")));
        assert!(!cron.is_time(&dt("2026-09-28 08:00:20")));
        assert_eq!(cron.seconds.len(), 3);

        // 下一次，5秒后
        let start = d("2026-09-28");
        let next = cron.get_next(start).unwrap();
        assert_eq!(next, start + Duration::seconds(5));

        // 后续每次间隔20秒
        let next = cron.get_next(next).unwrap();
        assert_eq!(next, start + Duration::seconds(25));

        let next = cron.get_next(next).unwrap();
        assert_eq!(next, start + Duration::seconds(45));
    }

    #[test]
    fn get_next_with_sub_second_test() {
        let cron = Cron::parse("5/20 * * * *").unwrap();

        // 带亚秒 → 前提下取整后加 2 秒再扫描（与 C# 一致，跳过紧邻的秒边界）
        let time = d("2026-09-28") + Duration::milliseconds(4500);
        let next = cron.get_next(time).unwrap();
        assert_eq!(next, d("2026-09-28") + Duration::seconds(25));
    }

    #[test]
    fn is_time_hour_test() {
        let cron = Cron::parse("* * 3 * * *").unwrap();
        assert!(cron.is_time(&dt("2026-09-28 03:00:00")));
        assert_eq!(cron.hours, vec![3]);

        let cron = Cron::parse("* * 0,12 * * *").unwrap();
        assert!(cron.is_time(&dt("2026-09-28 12:00:00")));
        assert_eq!(cron.hours, vec![0, 12]);
    }

    #[test]
    fn get_next_hour_test() {
        let cron = Cron::parse("0 0 0,12 * * *").unwrap();
        let start = d("2026-09-28");
        let next = cron.get_next(start).unwrap();
        assert_eq!(next, start + Duration::hours(12));

        let next = cron.get_next(next).unwrap();
        assert_eq!(next, start + Duration::hours(24));

        let next = cron.get_next(next).unwrap();
        assert_eq!(next, start + Duration::hours(36));
    }

    #[test]
    fn is_time_day_of_month_test() {
        let cron = Cron::parse("* * * 1 * *").unwrap();
        assert!(cron.is_time(&d("2010-08-01")));
        assert_eq!(cron.days_of_month, vec![1]);

        let cron = Cron::parse("0 0 0 1 * *").unwrap();
        let start = d("2010-08-01");
        let next = cron.get_next(start).unwrap();
        assert_eq!(next, d("2010-09-01"));
        let next = cron.get_next(next).unwrap();
        assert_eq!(next, d("2010-10-01"));
        let next = cron.get_next(next).unwrap();
        assert_eq!(next, d("2010-11-01"));
    }

    #[test]
    fn is_time_month_test() {
        let cron = Cron::parse("* * * * 1 *").unwrap();
        assert!(cron.is_time(&d("2008-01-01")));
        assert_eq!(cron.months, vec![1]);

        let cron = Cron::parse("* * * * 12 *").unwrap();
        assert!(!cron.is_time(&d("2008-01-01")));
        assert_eq!(cron.months, vec![12]);

        let cron = Cron::parse("* * * * */3 *").unwrap();
        assert!(cron.is_time(&d("2008-03-01")));
        assert!(cron.is_time(&d("2008-06-01")));
        assert_eq!(cron.months, vec![3, 6, 9, 12]);
    }

    #[test]
    fn get_next_month_test() {
        let cron = Cron::parse("0 0 0 1 */3 *").unwrap();
        let start = d("2010-08-01");
        let next = cron.get_next(start).unwrap();
        assert_eq!(next, d("2010-09-01"));
        let next = cron.get_next(next).unwrap();
        assert_eq!(next, d("2010-12-01"));
        let next = cron.get_next(next).unwrap();
        assert_eq!(next, d("2011-03-01"));
    }

    #[test]
    fn is_time_day_of_week_test() {
        let cron = Cron::parse("* * * * * 0").unwrap();
        assert!(cron.is_time(&d("2008-10-12"))); // 周日
        assert!(!cron.is_time(&d("2008-10-13"))); // 周一
        assert_eq!(cron.days_of_week.get(&0), Some(&0));

        let cron = Cron::parse("* * * * * */2").unwrap();
        assert!(cron.is_time(&d("2008-10-14")));
        assert_eq!(cron.days_of_week.len(), 4);
        let weeks: Vec<i32> = cron.days_of_week.keys().copied().collect();
        assert_eq!(weeks, vec![0, 2, 4, 6]);
    }

    #[test]
    fn is_time_test() {
        let cron = Cron::parse("* 0 11 12 10 *").unwrap();
        assert!(cron.is_time(&dt("2008-10-12 11:00:00")));
        assert!(!cron.is_time(&dt("2008-10-12 11:01:00")));
    }

    #[test]
    fn dayweek_test() {
        // 每个月的第二个星期三
        let cron = Cron::parse("0 0 0 ? ? 3#2").unwrap();
        assert!(cron.is_time(&dt("2023-03-08 00:00:00")));
        assert!(!cron.is_time(&dt("2023-03-15 00:00:00")));
        assert!(!cron.is_time(&dt("2023-03-22 00:00:00")));
        assert!(!cron.is_time(&dt("2023-03-29 00:00:00")));

        // 每月第一周的任意一天（周一~周日）
        let cron = Cron::parse("0 0 0 ? ? 0-6#1").unwrap();
        assert!(!cron.is_time(&dt("2023-02-27 00:00:00")));
        assert!(!cron.is_time(&dt("2023-02-28 00:00:00")));
        for day in 1..=7 {
            assert!(
                cron.is_time(&dt(&format!("2023-03-{day:02} 00:00:00"))),
                "2023-03-{day:02} 应为第一周"
            );
        }
        assert!(!cron.is_time(&dt("2023-03-08 00:00:00")));
        assert!(!cron.is_time(&dt("2023-03-09 00:00:00")));

        // 每月第一周的任意一天（周一~周日），Sunday=1
        let mut cron = Cron::parse("0 0 0 ? ? 1-7#1").unwrap();
        cron.sunday = 1;
        assert!(cron.is_time(&dt("2023-03-05 00:00:00"))); // 3月5日是周日

        // 每个月倒数第二个星期三到星期五
        let cron = Cron::parse("0 0 0 ? ? 3-5#L2").unwrap();
        assert!(!cron.is_time(&dt("2023-03-08 00:00:00")));
        assert!(!cron.is_time(&dt("2023-03-15 00:00:00")));
        assert!(cron.is_time(&dt("2023-03-22 00:00:00")));
        assert!(cron.is_time(&dt("2023-03-23 00:00:00")));
        assert!(cron.is_time(&dt("2023-03-24 00:00:00")));
        assert!(!cron.is_time(&dt("2023-03-29 00:00:00")));

        // 每个月的第二个星期二，Sunday=1
        let mut cron = Cron::parse("0 0 0 ? ? 3#2").unwrap();
        cron.sunday = 1;
        assert!(!cron.is_time(&dt("2023-03-07 00:00:00")));
        assert!(cron.is_time(&dt("2023-03-14 00:00:00")));
        assert!(!cron.is_time(&dt("2023-03-21 00:00:00")));
        assert!(!cron.is_time(&dt("2023-03-28 00:00:00")));

        // 每个月倒数第二个星期一到星期三，Sunday=1
        let mut cron = Cron::parse("0 0 0 ? ? 2-4#L2").unwrap();
        cron.sunday = 1;
        assert!(!cron.is_time(&dt("2023-03-07 00:00:00")));
        assert!(!cron.is_time(&dt("2023-03-14 00:00:00")));
        assert!(cron.is_time(&dt("2023-03-21 00:00:00")));
        assert!(!cron.is_time(&dt("2023-03-28 00:00:00")));
    }

    #[test]
    fn get_previous_test() {
        // 每天零点
        let cron = Cron::parse("0 0 0 * * *").unwrap();

        let prev = cron.get_previous(dt("2026-09-28 10:30:00")).unwrap();
        assert_eq!(prev, d("2026-09-28"));

        // 带亚秒：先向下取整再回退
        let prev = cron
            .get_previous(dt("2026-09-28 10:30:00") + Duration::milliseconds(500))
            .unwrap();
        assert_eq!(prev, d("2026-09-28"));
    }

    #[test]
    fn multi_crons_test() {
        let crons = vec![
            Cron::parse("0 0 12 * * *").unwrap(),
            Cron::parse("0 0 16 * * *").unwrap(),
        ];
        let start = d("2026-09-28");
        let next = Cron::get_next_multi(&crons, start).unwrap();
        assert_eq!(next, start + Duration::hours(12));

        let prev = Cron::get_previous_multi(&crons, dt("2026-09-28 18:00:00")).unwrap();
        assert_eq!(prev, dt("2026-09-28 16:00:00"));
    }

    #[test]
    fn no_match_returns_none() {
        // 2 月 30 日不存在，一年内无匹配
        let cron = Cron::parse("0 0 0 30 2 *").unwrap();
        assert!(cron.get_next(d("2026-01-01")).is_none());
    }
}
