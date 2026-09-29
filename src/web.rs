//! Web 辅助（对应 DH.NCore `NewLife.Web` 系列工具；收编自 DHDeploy.Agent / tcp-scanner-server 等工程的重复实现）。
//!
//! - [`url_encode`]：RFC 3986 百分号编码（URL 查询串与 `application/x-www-form-urlencoded` 值）；
//! - [`json_escape`]：JSON 字符串转义（不含两端引号，供请求体模板内嵌 JSON 值）。

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
}
