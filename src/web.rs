//! Web 辅助（对应 DH.NCore `NewLife.Web` 系列工具；收编自 DHDeploy.Agent / tcp-scanner-server 等工程的重复实现）。
//!
//! - [`url_encode`]：RFC 3986 百分号编码（URL 查询串与 `application/x-www-form-urlencoded` 值）；
//! - [`json_escape`]：JSON 字符串转义（不含两端引号，供请求体模板内嵌 JSON 值）；
//! - [`format_bytes`] / [`format_bytes_iec`] / [`format_speed`]：流量/速率可读格式化
//!   （Pek 系面板与部署代理共用，口径分别对齐 C# `StarApi.FormatBytes` 与 `FormatBytes`）；
//! - [`uptime_text`] / [`uptime_text_compact`]：运行时长文本（两种口径，见各自文档）。

/// RFC 3986 百分号编码：除 `A-Za-z0-9-._~` 外的字节全部转义为 `%XX`（大写十六进制）。
///
/// 说明：空格编码为 `%20`（非 `+`）；表单风格（空格→`+`）请在上层对结果自行处理。
pub fn url_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    push_url_encoded(&mut out, value);
    out
}

/// `application/x-www-form-urlencoded` 键值对编码（`k=v&k=v`，RFC 3986 百分号编码）。
///
/// 保持传入顺序（不排序）；阿里巴巴/连陆等表单签名场景可直接复用。
pub fn form_encode<I, K, V>(pairs: I) -> String
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: AsRef<str>,
{
    let mut out = String::new();
    for (key, value) in pairs {
        if !out.is_empty() {
            out.push('&');
        }
        push_url_encoded(&mut out, key.as_ref());
        out.push('=');
        push_url_encoded(&mut out, value.as_ref());
    }
    out
}

/// 追加 RFC 3986 编码（避免每个字段/字节单独创建 String）。
fn push_url_encoded(out: &mut String, value: &str) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for &b in value.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => {
                out.push('%');
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0x0f) as usize] as char);
            }
        }
    }
}

/// 从 URL 提取 authority（`host[:port]`）：去掉 userinfo、路径、查询与片段。
///
/// 支持带/不带 scheme；空字符串或空 authority 返回 `None`。
pub fn url_authority(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .trim();
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    if authority.is_empty() {
        None
    } else {
        Some(authority.to_string())
    }
}

/// 从 URL 提取 host：去掉 userinfo 与端口；IPv6 保留方括号（`[::1]`）。
pub fn url_host(url: &str) -> Option<String> {
    let authority = url_authority(url)?;
    let host = if let Some(rest) = authority.strip_prefix('[') {
        let end = rest.find(']')?;
        let inner = &rest[..end];
        if inner.is_empty() {
            return None;
        }
        format!("[{inner}]")
    } else {
        authority.split(':').next().unwrap_or("").to_string()
    };
    (!host.is_empty()).then_some(host)
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

/// 运行时长文本（`1 天 2 小时` / `1 小时 1 分` / `1 分 30 秒` / `0 秒`）。
///
/// 只保留两个粒度（避免面板串太长），空格分隔，**精确到秒**。
/// 下沉自 HlktechIoT MQTT 接入服务面板（概览页「运行时长」）。
pub fn uptime_text(secs: u64) -> String {
    let d = secs / 86_400;
    let h = (secs % 86_400) / 3_600;
    let m = (secs % 3_600) / 60;
    let s = secs % 60;
    if d > 0 {
        format!("{d} 天 {h} 小时")
    } else if h > 0 {
        format!("{h} 小时 {m} 分")
    } else if m > 0 {
        format!("{m} 分 {s} 秒")
    } else {
        format!("{s} 秒")
    }
}

/// 运行时长文本（**星尘 C# 口径**：`21天13小时` / `3小时5分` / `5分`；无空格、按分钟取整）。
///
/// 用于需要与 C# StarAgent 面板逐字一致的场景（Pek.RAgent 实践沉淀）。
pub fn uptime_text_compact(secs: u64) -> String {
    let total_minutes = secs / 60;
    let days = total_minutes / 1440;
    let hours = (total_minutes % 1440) / 60;
    let minutes = total_minutes % 60;
    if days > 0 {
        format!("{days}天{hours}小时")
    } else if hours > 0 {
        format!("{hours}小时{minutes}分")
    } else {
        format!("{minutes}分")
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
    fn form_encode_preserves_order_and_encodes() {
        assert_eq!(
            form_encode([("a", "b c"), ("中", "文")]),
            "a=b%20c&%E4%B8%AD=%E6%96%87"
        );
        assert_eq!(form_encode::<_, &str, &str>([]), "");
    }

    #[test]
    fn url_authority_and_host() {
        assert_eq!(
            url_authority("https://user:pass@[::1]:8443/path").as_deref(),
            Some("[::1]:8443")
        );
        assert_eq!(
            url_authority("sms.example.com/api").as_deref(),
            Some("sms.example.com")
        );
        assert_eq!(
            url_authority("https://host/path?x=1#frag").as_deref(),
            Some("host")
        );
        assert_eq!(url_authority(""), None);

        assert_eq!(
            url_host("http://user@127.0.0.1:8080/x").as_deref(),
            Some("127.0.0.1")
        );
        assert_eq!(url_host("http://[::1]:8080").as_deref(), Some("[::1]"));
        assert_eq!(url_host("http://:8080"), None);
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
        assert_eq!(
            format_bytes_iec(1024 * 1024 * 1024 + 512 * 1024 * 1024),
            "1.5 GiB"
        );
    }

    #[test]
    fn format_speed_units() {
        assert_eq!(format_speed(0), "0 B/s");
        assert_eq!(format_speed(900), "900 B/s");
        assert_eq!(format_speed(2048), "2.0 KB/s");
        assert_eq!(format_speed(3 * 1024 * 1024), "3.0 MB/s");
    }

    #[test]
    fn uptime_text_two_granularities() {
        // HlktechIoT 面板口径：空格分隔、精确到秒
        assert_eq!(uptime_text(0), "0 秒");
        assert_eq!(uptime_text(90), "1 分 30 秒");
        assert_eq!(uptime_text(3660), "1 小时 1 分");
        assert_eq!(uptime_text(90_000), "1 天 1 小时");
    }

    #[test]
    fn uptime_text_compact_matches_staragent() {
        // 星尘 C# 口径：无空格、按分钟取整
        assert_eq!(uptime_text_compact(0), "0分");
        assert_eq!(uptime_text_compact(90), "1分");
        assert_eq!(uptime_text_compact(3_600 + 300), "1小时5分");
        assert_eq!(uptime_text_compact(21 * 86_400 + 13 * 3_600), "21天13小时");
    }
}
