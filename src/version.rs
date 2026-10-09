//! 版本号工具（数字段比较）——Agent 自动升级的“是否有新版本”判断。
//!
//! 版本串按 `.` 分段后逐段作数值比较（各段取前导数字，缺失段按 0 补齐）：
//! `1.0.24` > `1.0.23`、`1.10.0` > `1.9.9`（数值而非字典序）、`1.0` == `1.0.0`。
//! 段内后缀（如 `1.0.24-beta1`）容忍并按前导数字参与比较。

use std::cmp::Ordering;

/// 数字段版本比较：`a < b` / `a == b` / `a > b`。
/// <summary>数字段版本比较</summary>
pub fn compare_numeric(a: &str, b: &str) -> Ordering {
    let mut pa = a.split('.');
    let mut pb = b.split('.');
    loop {
        let sa = pa.next();
        let sb = pb.next();
        if sa.is_none() && sb.is_none() {
            return Ordering::Equal;
        }
        let va = sa.map(segment_number).unwrap_or(0);
        let vb = sb.map(segment_number).unwrap_or(0);
        match va.cmp(&vb) {
            Ordering::Equal => continue,
            ord => return ord,
        }
    }
}

/// `candidate` 是否比 `current` 新（严格大于）。
/// <summary>判断候选版本是否更新</summary>
pub fn is_newer(candidate: &str, current: &str) -> bool {
    compare_numeric(candidate, current) == Ordering::Greater
}

/// 段解析：取前导数字（`23`→23、`23-beta`→23、空/全非数字→0）。
fn segment_number(seg: &str) -> u64 {
    let digits: String = seg.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_segments_compare() {
        assert!(is_newer("1.0.24", "1.0.23"));
        assert!(is_newer("1.10.0", "1.9.9"));
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(!is_newer("1.0.0", "1.0.0"));
        assert!(!is_newer("1.0.0", "1.0.1"));
    }

    #[test]
    fn missing_segments_treated_as_zero() {
        assert_eq!(compare_numeric("1.0", "1.0.0"), Ordering::Equal);
        assert!(is_newer("1.1", "1.0.9"));
    }

    #[test]
    fn suffix_and_garbage_tolerated() {
        assert!(is_newer("1.0.24-beta1", "1.0.23"));
        assert_eq!(compare_numeric("abc", "0"), Ordering::Equal);
    }
}
