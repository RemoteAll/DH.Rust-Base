//! JSON 值读取小工具（第三方接口响应解析；收编各项目重复的 `json_text`/`json_i64`）。
//!
//! 全部返回 `Option`，区分“键不存在/类型不符”和“空字符串/0”。

use serde_json::Value;

/// 读取字符串（数字/布尔自动转字符串）。
pub fn get_str(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(value_to_string)
}

/// 值转字符串（数字/布尔自动转；其他类型返回 `None`）。
pub fn value_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

/// 读取整数（字符串也尝试解析）。
pub fn get_i64(value: &Value, key: &str) -> Option<i64> {
    value.get(key).and_then(value_to_i64)
}

/// 值转整数（字符串也尝试解析）。
pub fn value_to_i64(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

/// 读取布尔值（兼容 `"true"/"1"/"yes"/"on"` 文本）。
pub fn get_bool(value: &Value, key: &str) -> Option<bool> {
    value.get(key).and_then(value_to_bool)
}

/// 值转布尔（兼容 `"true"/"1"/"yes"/"on"` 与 `"false"/"0"/"no"/"off"` 文本）。
pub fn value_to_bool(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(flag) => Some(*flag),
        Value::Number(number) => number.as_i64().map(|n| n != 0),
        Value::String(text) => {
            let text = text.trim();
            if ["true", "1", "yes", "on"]
                .iter()
                .any(|v| text.eq_ignore_ascii_case(v))
            {
                Some(true)
            } else if ["false", "0", "no", "off"]
                .iter()
                .any(|v| text.eq_ignore_ascii_case(v))
            {
                Some(false)
            } else {
                None
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_typed_values_and_distinguishes_missing() {
        let value = json!({
            "text": "abc",
            "number": 42,
            "float": 1.5,
            "flag": true,
            "yes": "yes",
            "off": "0",
            "empty": ""
        });
        assert_eq!(get_str(&value, "text").as_deref(), Some("abc"));
        assert_eq!(get_str(&value, "number").as_deref(), Some("42"));
        assert_eq!(get_str(&value, "flag").as_deref(), Some("true"));
        assert_eq!(get_str(&value, "missing"), None);
        assert_eq!(get_i64(&value, "number"), Some(42));
        assert_eq!(get_i64(&value, "float"), None);
        assert_eq!(get_i64(&value, "text"), None);
        assert_eq!(get_bool(&value, "flag"), Some(true));
        assert_eq!(get_bool(&value, "yes"), Some(true));
        assert_eq!(get_bool(&value, "off"), Some(false));
        assert_eq!(get_bool(&value, "text"), None);
        assert_eq!(get_str(&value, "empty").as_deref(), Some(""));

        assert_eq!(value_to_string(&json!(42)).as_deref(), Some("42"));
        assert_eq!(value_to_i64(&json!("7")), Some(7));
        assert_eq!(value_to_bool(&json!("off")), Some(false));
        assert_eq!(value_to_string(&json!(null)), None);
    }
}
