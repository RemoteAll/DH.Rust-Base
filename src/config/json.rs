//! JSON 配置读写，对应 C# `JsonConfigProvider`。
//!
//! 读取时与 C# 一致地清理注释（`/* */` 块注释与整行 `//` 行注释），提高容错；
//! 写入使用 `serde_json` 缩进输出，键名与 C# 属性名（PascalCase）一致。

use super::setting::Setting;
use super::ConfigError;

/// 序列化配置为 JSON 文本。
pub(crate) fn to_json(setting: &Setting) -> Result<String, ConfigError> {
    serde_json::to_string_pretty(setting).map_err(|e| ConfigError::Parse(e.to_string()))
}

/// 从 JSON 文本读取配置（自动清理注释，并按字段类型做归一化转换）。
pub(crate) fn from_json(text: &str) -> Result<Setting, ConfigError> {
    let cleaned = trim_comment(text);
    let value: serde_json::Value =
        serde_json::from_str(&cleaned).map_err(|e| ConfigError::Parse(e.to_string()))?;
    let value = super::setting::normalize_setting_value(value);
    serde_json::from_value(value).map_err(|e| ConfigError::Parse(e.to_string()))
}

/// 清理 JSON 字符串中的注释。对应 C# `JsonConfigProvider.TrimComment`。
///
/// - 循环删除 `/* ... */` 块注释（未闭合时停止处理）；
/// - 删除整行为 `//` 注释的行与空行。
pub(crate) fn trim_comment(text: &str) -> String {
    let mut text = text.to_string();

    // 以下处理多行注释 "/**/" 放在一行的情况
    while let Some(p) = text.find("/*") {
        let Some(p2) = text[p + 2..].find("*/").map(|i| p + 2 + i) else {
            break;
        };
        text = format!("{}{}", &text[..p], &text[p2 + 2..]);
    }

    // 处理整行注释与空行
    let lines: Vec<&str> = text
        .split(['\n', '\r'])
        .filter(|line| !line.is_empty() && !line.trim_start().starts_with("//"))
        .collect();

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trim_block_comment_same_line() {
        assert_eq!(trim_comment("{/*注释*/\"a\":1}"), "{\"a\":1}");
    }

    #[test]
    fn trim_block_comment_multiline() {
        let text = "{\n  /*第一行\n  第二行*/\n  \"a\": 1\n}";
        assert_eq!(trim_comment(text), "{\n  \n  \"a\": 1\n}");
    }

    #[test]
    fn trim_line_comment_and_empty_lines() {
        let text = "{\n// 注释\n\n  \"a\": 1\n}";
        assert_eq!(trim_comment(text), "{\n  \"a\": 1\n}");
    }

    #[test]
    fn json_roundtrip_with_comments() {
        let text = r#"{
  // 调试开关
  "Debug": true,
  "LogLevel": "Warn",
  "LogFileMaxBytes": 20,
  "LogPath": ""
}"#;
        let setting = from_json(text).unwrap();
        assert!(setting.debug);
        assert_eq!(setting.log_level, "Warn");
        assert_eq!(setting.log_file_max_bytes, 20);

        let json = to_json(&setting).unwrap();
        let again = from_json(&json).unwrap();
        assert_eq!(setting, again);
    }

    #[test]
    fn json_tolerates_wrong_types() {
        // 类型不符的值：能转换则转换，不能转换则回落字段默认值，不影响其它字段
        let text = r#"{ "Debug": "not-bool", "LogFileMaxBytes": "abc", "LogPath": 123 }"#;
        let setting = from_json(text).unwrap();
        assert!(setting.debug); // 默认 true
        assert_eq!(setting.log_file_max_bytes, 10); // 默认 10
        assert_eq!(setting.log_path, "123"); // 数字转为字符串
    }
}
