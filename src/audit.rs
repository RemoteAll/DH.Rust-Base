//! 审计日志的脱敏与摘要工具（纯函数，无 IO、无状态）。
//!
//! 提炼自各管理后台（Pek.RAdmin / Pek.RAgent / HlkProductTool / DHDeploy）逐仓同款的
//! 「请求参数摘要 + 敏感字段脱敏」实现——凡是要把请求参数落审计库的地方都该走这里，
//! 避免每个项目各写一份正则/递归。
//!
//! 约定：
//! - 敏感键判定为**不区分大小写的包含匹配**（`password`/`secret`/`token`/`apikey`），
//!   宁可多脱敏（`passwordConfirm`、`refreshToken` 都会命中）；
//! - JSON 体递归脱敏（数组/嵌套对象都处理），表单体按 `k=v&...` 逐对处理；
//! - 摘要统一截断到 400 字符（`dhrust::text::truncate_chars`，不破坏 UTF-8 边界）。
//!
//! ```no_run
//! use dhrust::audit::{is_sensitive_key, redact_form, summarize_body};
//!
//! assert!(is_sensitive_key("NewPassword"));
//! assert_eq!(redact_form("user=bob&password=pw123"), "user=bob&password=***");
//! let detail = summarize_body("token=abc", "application/json", br#"{"name":"x"}"#);
//! assert!(detail.contains("token=***"));
//! ```

/// 摘要最大长度（字符）。
pub const SUMMARY_MAX_CHARS: usize = 400;

/// 是否敏感键（不区分大小写的包含匹配）。
pub fn is_sensitive_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    k.contains("password") || k.contains("secret") || k.contains("token") || k.contains("apikey")
}

/// JSON 体递归脱敏（敏感键的值替换为 `"***"`；数组与嵌套对象都处理）。
pub fn redact_json(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(map) => {
            for (k, val) in map.iter_mut() {
                if is_sensitive_key(k) {
                    *val = serde_json::Value::String("***".to_string());
                } else {
                    redact_json(val);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                redact_json(item);
            }
        }
        _ => {}
    }
}

/// `k=v&k2=v2` 形式串的脱敏（敏感键的值替换为 `***`）。
pub fn redact_form(text: &str) -> String {
    text.split('&')
        .map(|pair| match pair.split_once('=') {
            Some((k, _)) if is_sensitive_key(k) => format!("{k}=***"),
            _ => pair.to_string(),
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// 请求参数摘要：查询串 + JSON/表单体（敏感字段脱敏；截断至 [`SUMMARY_MAX_CHARS`] 字符）。
///
/// `content_type` 传请求头值（小写匹配 `application/json` / `x-www-form-urlencoded`）；
/// 其他类型只记 `body={n} 字节`（不落二进制内容）；大于 64KB 的 JSON 不解析。
pub fn summarize_body(query: &str, content_type: &str, body: &[u8]) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !query.is_empty() {
        parts.push(redact_form(query));
    }
    if !body.is_empty() {
        let content_type = content_type.to_ascii_lowercase();
        if content_type.contains("application/json") && body.len() <= 64 * 1024 {
            match serde_json::from_slice::<serde_json::Value>(body) {
                Ok(mut v) => {
                    redact_json(&mut v);
                    parts.push(v.to_string());
                }
                Err(_) => parts.push(format!("body={} 字节", body.len())),
            }
        } else if content_type.contains("x-www-form-urlencoded") {
            parts.push(redact_form(&String::from_utf8_lossy(body)));
        } else {
            parts.push(format!("body={} 字节", body.len()));
        }
    }
    crate::text::truncate_chars(&parts.join(" | "), SUMMARY_MAX_CHARS)
}

/// 路径最后一段（审计动作名，如 `/panel/userSave` → `userSave`）。
pub fn action_name_of(path: &str) -> String {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_string()
}

/// 动作中文名（`titles` 为应用的「动作 → 中文名」映射；未知动作回退为 `操作 {action}`）。
pub fn action_title(action: &str, titles: &[(&str, &str)]) -> String {
    titles
        .iter()
        .find(|(name, _)| *name == action)
        .map(|(_, title)| (*title).to_string())
        .unwrap_or_else(|| format!("操作 {action}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensitive_key_matches_common_credential_names() {
        for key in [
            "password",
            "Password",
            "newPassword",
            "passwordConfirm",
            "client_secret",
            "refreshToken",
            "ApiKey",
        ] {
            assert!(is_sensitive_key(key), "{key} 应判为敏感");
        }
        for key in ["userName", "remark", "path", "code"] {
            assert!(!is_sensitive_key(key), "{key} 不应判为敏感");
        }
    }

    #[test]
    fn redact_json_handles_nested_and_arrays() {
        let mut v = serde_json::json!({
            "user": "bob",
            "password": "pw123",
            "nested": { "token": "t0k", "ok": 1 },
            "list": [{ "secret": "s" }, { "name": "x" }],
        });
        redact_json(&mut v);
        assert_eq!(v["user"], serde_json::json!("bob"));
        assert_eq!(v["password"], serde_json::json!("***"));
        assert_eq!(v["nested"]["token"], serde_json::json!("***"));
        assert_eq!(v["nested"]["ok"], serde_json::json!(1));
        assert_eq!(v["list"][0]["secret"], serde_json::json!("***"));
        assert_eq!(v["list"][1]["name"], serde_json::json!("x"));
    }

    #[test]
    fn redact_form_keeps_non_sensitive_pairs() {
        assert_eq!(
            redact_form("user=bob&password=pw123&ApiKey=k1&page=2"),
            "user=bob&password=***&ApiKey=***&page=2"
        );
        assert_eq!(redact_form(""), "");
    }

    #[test]
    fn summarize_body_combines_query_and_body_with_limit() {
        let detail = summarize_body(
            "token=abc",
            "application/json",
            br#"{"name":"x","secret":"s"}"#,
        );
        assert!(detail.contains("token=***"), "{detail}");
        assert!(detail.contains(r#""secret":"***""#), "{detail}");
        assert!(!detail.contains("s\""), "原值不应出现：{detail}");

        // 非 JSON/表单：只记字节数
        assert_eq!(
            summarize_body("", "application/octet-stream", b"12345"),
            "body=5 字节"
        );

        // 超长摘要被截断（按字符，不破坏 UTF-8）
        let long = "a".repeat(1000);
        let detail = summarize_body(&format!("k={long}"), "", b"");
        assert!(
            detail.chars().count() <= SUMMARY_MAX_CHARS + 1,
            "{}",
            detail.len()
        );
    }

    #[test]
    fn action_name_and_title() {
        assert_eq!(action_name_of("/panel/userSave"), "userSave");
        assert_eq!(action_name_of("/api/db/"), "db");
        assert_eq!(action_name_of("userSave"), "userSave");
        assert_eq!(
            action_title("userSave", &[("userSave", "保存用户")]),
            "保存用户"
        );
        assert_eq!(action_title("unknown", &[]), "操作 unknown");
    }
}
