//! Web 辅助（对应 DH.NCore `NewLife.Web` 系列工具；收编自 DHDeploy.Agent / tcp-scanner-server 等工程的重复实现）。
//!
//! - [`url_encode`]：RFC 3986 百分号编码（URL 查询串与 `application/x-www-form-urlencoded` 值）；
//! - [`json_escape`]：JSON 字符串转义（不含两端引号，供请求体模板内嵌 JSON 值）；
//! - [`format_bytes`] / [`format_bytes_iec`] / [`format_speed`]：流量/速率可读格式化
//!   （Pek 系面板与部署代理共用，口径分别对齐 C# `StarApi.FormatBytes` 与 `FormatBytes`）。

/// RFC 3986 百分号编码：除 `A-Za-z0-9-._~` 外的字节全部转义为 `%XX`（大写十六进制）。
///
/// 说明：空格编码为 `%20`（非 `+`）；表单风格（空格→`+`）请在上层对结果自行处理。
pub fn url_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// JSON 字符串转义（去掉两端引号；结果可直接嵌入 JSON 字符串字面量内部）。
///
/// 转义规则与 `serde_json` 一致（非 ASCII 字符原样保留）；空输入返回空串。
pub fn json_escape(value: &str) -> String {
    match serde_json::to_string(value) {
        Ok(quoted) if quoted.len() >= 2 => quoted[1..quoted.len() - 1].to_string(),
        _ => String::new(),
    }
}

/// 字节数可读格式（对齐 C# `StarApi.FormatBytes`：B/KB/MB/GB，1024 进制；
/// 用于 Pek 系面板流量展示——Pek.RAgent 实践沉淀）。
pub fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / 1024.0 / 1024.0)
    } else {
        format!("{:.2} GB", bytes as f64 / 1024.0 / 1024.0 / 1024.0)
    }
}

/// 字节数可读格式（IEC 单位：B/KiB/MiB/GiB/TiB/PiB，最多两位小数去尾零；
/// 对齐 C# `FormatBytes`——DHDeploy.Agent 实践沉淀）。
pub fn format_bytes_iec(bytes: i64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut size = bytes as f64;
    let mut idx = 0usize;
    while size >= 1024.0 && idx < units.len() - 1 {
        size /= 1024.0;
        idx += 1;
    }
    format!("{} {}", trim2(size), units[idx])
}

/// 速率格式化（人类可读：B/s、KB/s、MB/s；Pek 系面板流量卡）。
pub fn format_speed(bps: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    let v = bps as f64;
    if v >= MB {
        format!("{:.1} MB/s", v / MB)
    } else if v >= KB {
        format!("{:.1} KB/s", v / KB)
    } else {
        format!("{v:.0} B/s")
    }
}

/// 最多两位小数、去尾零（对齐 C# `:0.##`）。
fn trim2(v: f64) -> String {
    let s = format!("{v:.2}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() {
        "0".to_string()
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_encode_is_rfc3986() {
        assert_eq!(url_encode("abc-_.~"), "abc-_.~");
        assert_eq!(url_encode("中 文"), "%E4%B8%AD%20%E6%96%87");
        assert_eq!(url_encode("a b&c"), "a%20b%26c");
    }

    #[test]
    fn json_escape_without_quotes() {
        assert_eq!(json_escape(r#"HLT "A"&B"#), r#"HLT \"A\"&B"#);
        assert_eq!(json_escape("中文条码"), "中文条码");
        assert_eq!(json_escape(""), "");
        assert_eq!(json_escape("line\nbreak"), "line\\nbreak");
    }

    #[test]
    fn format_bytes_matches_csharp_starapi() {
        // 对齐 C# StarApi.FormatBytes：B/KB/MB/GB，1024 进制（Pek.RAgent 黄金样本）
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.00 GB");
    }

    #[test]
    fn format_bytes_iec_matches_csharp() {
        // 对齐 C# FormatBytes：IEC 单位、最多两位小数去尾零（DHDeploy 口径）
        assert_eq!(format_bytes_iec(0), "0 B");
        assert_eq!(format_bytes_iec(1024), "1 KiB");
        assert_eq!(format_bytes_iec(1536), "1.5 KiB");
        assert_eq!(format_bytes_iec(20 * 1024 * 1024 * 1024), "20 GiB");
        assert_eq!(format_bytes_iec(1024 * 1024 * 1024 + 512 * 1024 * 1024), "1.5 GiB");
    }

    #[test]
    fn format_speed_units() {
        assert_eq!(format_speed(0), "0 B/s");
        assert_eq!(format_speed(900), "900 B/s");
        assert_eq!(format_speed(2048), "2.0 KB/s");
        assert_eq!(format_speed(3 * 1024 * 1024), "3.0 MB/s");
    }
}
