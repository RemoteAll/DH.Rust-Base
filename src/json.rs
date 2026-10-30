//! JSON 工具：第三方响应取值 + **C# 兼容的宽松解析/转义**。
//!
//! 两部分：
//! - [`get_str`] / [`get_i64`] / [`get_f64`] / [`get_bool`]：基于 `serde_json::Value` 的取值小工具
//!   （收编各项目重复的 `json_text`/`json_i64`；全部返回 `Option`，区分“键不存在/类型不符”与“空字符串/0”）。
//! - [`JsonValue`] / [`parse_lax`] / [`write_quoted`]：**裸键 JSON**（C# `SimpleJsonString`/`JsonParser` 风格）
//!   的解析与转义——标准 JSON 解析器读不了裸键，本实现同时接受裸键、`//` 行注释、尾逗号与标准 JSON。
//!   来源：`Pek.RMDB` 持久化格式（C# `Pek.MDB` 双向互通），下沉为公共能力。

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

/// 读取浮点数（字符串也尝试解析）。
pub fn get_f64(value: &Value, key: &str) -> Option<f64> {
    value.get(key).and_then(value_to_f64)
}

/// 值转浮点（字符串也尝试解析）。
pub fn value_to_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

/// 值转布尔（兼容 `"true"/"1"/"yes"/"on"` 与 `"false"/"0"/"no"/"off"` 文本）。
pub fn value_to_bool(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(flag) => Some(*flag),
        Value::Number(number) => number.as_i64().map(|n| n != 0),
        Value::String(text) => {
            if ["true", "1", "yes", "on"]
                .iter()
                .any(|v| text.trim().eq_ignore_ascii_case(v))
            {
                Some(true)
            } else if ["false", "0", "no", "off"]
                .iter()
                .any(|v| text.trim().eq_ignore_ascii_case(v))
            {
                Some(false)
            } else {
                None
            }
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// 宽松 JSON（C# 兼容）
// ---------------------------------------------------------------------------

/// JSON 值（宽松解析的中间表示；保持对象键的出现顺序）。
#[derive(Clone, Debug, PartialEq)]
pub enum JsonValue {
    /// null
    Null,
    /// 布尔
    Bool(bool),
    /// 整数
    Int(i64),
    /// 浮点
    Float(f64),
    /// 文本
    Text(String),
    /// 数组
    Array(Vec<JsonValue>),
    /// 对象（键值对按出现顺序）
    Object(Vec<(String, JsonValue)>),
}

impl JsonValue {
    /// 按键取值（**大小写不敏感**；C# 属性名大小写不一致时也能容错）。
    pub fn get(&self, key: &str) -> Option<&JsonValue> {
        match self {
            JsonValue::Object(entries) => entries
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(key))
                .map(|(_, v)| v),
            _ => None,
        }
    }

    /// 字符串值（仅 `Text`）。
    pub fn as_str(&self) -> Option<&str> {
        match self {
            JsonValue::Text(text) => Some(text),
            _ => None,
        }
    }

    /// 整数值（仅 `Int`）。
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            JsonValue::Int(value) => Some(*value),
            _ => None,
        }
    }

    /// 浮点值（`Float` 或 `Int`）。
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            JsonValue::Float(value) => Some(*value),
            JsonValue::Int(value) => Some(*value as f64),
            _ => None,
        }
    }

    /// 布尔值（仅 `Bool`）。
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            JsonValue::Bool(value) => Some(*value),
            _ => None,
        }
    }

    /// 数组元素。
    pub fn as_array(&self) -> Option<&[JsonValue]> {
        match self {
            JsonValue::Array(items) => Some(items),
            _ => None,
        }
    }

    /// 对象键值对。
    pub fn as_object(&self) -> Option<&[(String, JsonValue)]> {
        match self {
            JsonValue::Object(entries) => Some(entries),
            _ => None,
        }
    }

    /// 转为整数（整数/浮点/文本数字；宽松）。
    pub fn to_i64(&self) -> Option<i64> {
        match self {
            JsonValue::Int(value) => Some(*value),
            JsonValue::Float(value) if value.is_finite() => Some(*value as i64),
            JsonValue::Text(text) => text.trim().parse().ok(),
            _ => None,
        }
    }

    /// 转为浮点（整数/浮点/文本数字；宽松）。
    pub fn to_f64(&self) -> Option<f64> {
        match self {
            JsonValue::Float(value) => Some(*value),
            JsonValue::Int(value) => Some(*value as f64),
            JsonValue::Text(text) => text.trim().parse().ok(),
            _ => None,
        }
    }

    /// 转为布尔（布尔/整数非零/文本 `true/1/yes/on`、`false/0/no/off`，大小写不敏感）。
    pub fn to_bool(&self) -> Option<bool> {
        match self {
            JsonValue::Bool(value) => Some(*value),
            JsonValue::Int(value) => Some(*value != 0),
            JsonValue::Float(value) => Some(*value != 0.0),
            JsonValue::Text(text) => crate::text::parse_bool(text),
            _ => None,
        }
    }

    /// 转为文本（`Text` 原样；数字/布尔转字符串；null/数组/对象返回 `None`）。
    pub fn to_text(&self) -> Option<String> {
        match self {
            JsonValue::Null | JsonValue::Array(_) | JsonValue::Object(_) => None,
            JsonValue::Text(text) => Some(text.clone()),
            JsonValue::Bool(value) => Some(value.to_string()),
            JsonValue::Int(value) => Some(value.to_string()),
            JsonValue::Float(value) => Some(value.to_string()),
        }
    }
}

/// 宽松 JSON 解析（C# `JsonParser` / `SimpleJsonString` 兼容）。
///
/// 支持：
/// - 裸键（`{ Id:1, Name:"x" }`）与标准带引号键；
/// - `//` 行注释；
/// - 尾逗号；
/// - 字符串转义（含 `\uXXXX` 与代理对）。
///
/// 失败返回带位置的错误消息。
pub fn parse_lax(text: &str) -> Result<JsonValue, String> {
    let mut parser = LaxParser {
        bytes: text.as_bytes(),
        pos: 0,
    };
    parser.skip_ws();
    let value = parser.parse_value()?;
    parser.skip_ws();
    if parser.pos != parser.bytes.len() {
        return Err(format!("位置 {} 之后存在多余内容", parser.pos));
    }
    Ok(value)
}

/// 写 JSON 字符串（C# `JsonString.ClearNewLine` 口径：转义 `\` 与 `"`，去掉 `\r`/`\n`/`\t`）。
pub fn write_quoted(out: &mut String, text: &str) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\r' | '\n' | '\t' => {}
            c => out.push(c),
        }
    }
    out.push('"');
}

/// 浮点数字文本（C# `double.ToString()` 口径）：
/// 整数值不带 `.0`；非有限值输出 `NaN` / `Infinity` / `-Infinity`。
pub fn format_number(value: f64) -> String {
    if value.is_nan() {
        "NaN".to_string()
    } else if value == f64::INFINITY {
        "Infinity".to_string()
    } else if value == f64::NEG_INFINITY {
        "-Infinity".to_string()
    } else if value == value.trunc() && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

struct LaxParser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl LaxParser<'_> {
    fn skip_ws(&mut self) {
        loop {
            while self.pos < self.bytes.len() && self.bytes[self.pos].is_ascii_whitespace() {
                self.pos += 1;
            }
            // `//` 行注释（对齐 C# `CharSource.clearComment`）
            if self.pos + 1 < self.bytes.len()
                && self.bytes[self.pos] == b'/'
                && self.bytes[self.pos + 1] == b'/'
            {
                while self.pos < self.bytes.len() && self.bytes[self.pos] != b'\n' {
                    self.pos += 1;
                }
                continue;
            }
            break;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn bump(&mut self) -> Result<u8, String> {
        let byte = self
            .peek()
            .ok_or_else(|| format!("位置 {} 处意外结束", self.pos))?;
        self.pos += 1;
        Ok(byte)
    }

    fn expect(&mut self, expected: u8) -> Result<(), String> {
        let byte = self.bump()?;
        if byte != expected {
            return Err(format!(
                "位置 {} 期望 '{}'，实际 '{}'",
                self.pos - 1,
                expected as char,
                byte as char
            ));
        }
        Ok(())
    }

    fn parse_value(&mut self) -> Result<JsonValue, String> {
        self.skip_ws();
        match self.peek() {
            Some(b'{') => self.parse_object(),
            Some(b'[') => self.parse_array(),
            Some(b'"') => self.parse_string().map(JsonValue::Text),
            Some(b't') => {
                self.expect_literal("true")?;
                Ok(JsonValue::Bool(true))
            }
            Some(b'f') => {
                self.expect_literal("false")?;
                Ok(JsonValue::Bool(false))
            }
            Some(b'n') => {
                self.expect_literal("null")?;
                Ok(JsonValue::Null)
            }
            Some(b'-') | Some(b'0'..=b'9') => self.parse_number(),
            Some(other) => Err(format!(
                "位置 {} 处出现非法字符 '{}'",
                self.pos, other as char
            )),
            None => Err("意外的输入结束".to_string()),
        }
    }

    fn expect_literal(&mut self, literal: &str) -> Result<(), String> {
        let bytes = literal.as_bytes();
        if self.bytes.len() < self.pos + bytes.len()
            || &self.bytes[self.pos..self.pos + bytes.len()] != bytes
        {
            return Err(format!("位置 {} 处期望 {literal}", self.pos));
        }
        self.pos += bytes.len();
        Ok(())
    }

    fn parse_object(&mut self) -> Result<JsonValue, String> {
        self.expect(b'{')?;
        let mut entries = Vec::new();
        loop {
            self.skip_ws();
            if self.peek() == Some(b'}') {
                self.pos += 1;
                break;
            }
            if !entries.is_empty() {
                self.expect(b',')?;
                self.skip_ws();
                // 容忍尾逗号
                if self.peek() == Some(b'}') {
                    self.pos += 1;
                    break;
                }
            }
            let key = if self.peek() == Some(b'"') {
                self.parse_string()?
            } else {
                self.parse_bare_key()?
            };
            self.skip_ws();
            self.expect(b':')?;
            let value = self.parse_value()?;
            entries.push((key, value));
        }
        Ok(JsonValue::Object(entries))
    }

    /// 裸键（C# `SimpleJsonString` 格式，如 `Id` / `CreateTime`）。
    fn parse_bare_key(&mut self) -> Result<String, String> {
        let start = self.pos;
        while let Some(byte) = self.peek() {
            if byte.is_ascii_whitespace() || byte == b':' || byte == b',' || byte == b'}' {
                break;
            }
            self.pos += 1;
        }
        if start == self.pos {
            return Err(format!("位置 {} 处缺少对象键", self.pos));
        }
        String::from_utf8(self.bytes[start..self.pos].to_vec())
            .map_err(|e| format!("键名不是合法 UTF-8：{e}"))
    }

    fn parse_array(&mut self) -> Result<JsonValue, String> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        loop {
            self.skip_ws();
            if self.peek() == Some(b']') {
                self.pos += 1;
                break;
            }
            if !items.is_empty() {
                self.expect(b',')?;
                self.skip_ws();
                // C# `ArrayJsonParser` 对连续逗号插入 null；这里容忍尾逗号
                if self.peek() == Some(b']') {
                    self.pos += 1;
                    break;
                }
                if self.peek() == Some(b',') {
                    items.push(JsonValue::Null);
                    continue;
                }
            }
            items.push(self.parse_value()?);
        }
        Ok(JsonValue::Array(items))
    }

    fn parse_string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            let byte = self.bump()?;
            match byte {
                b'"' => break,
                b'\\' => {
                    let escape = self.bump()?;
                    match escape {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{0008}'),
                        b'f' => out.push('\u{000C}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let first = self.parse_hex4()?;
                            // 代理对：\uD83D\uDE00
                            if (0xD800..=0xDBFF).contains(&first) {
                                let save = self.pos;
                                if self.peek() == Some(b'\\') {
                                    self.pos += 1;
                                    if self.peek() == Some(b'u') {
                                        self.pos += 1;
                                        let second = self.parse_hex4()?;
                                        if (0xDC00..=0xDFFF).contains(&second) {
                                            let code = 0x10000
                                                + ((first - 0xD800) << 10)
                                                + (second - 0xDC00);
                                            if let Some(ch) = char::from_u32(code) {
                                                out.push(ch);
                                                continue;
                                            }
                                        }
                                    }
                                }
                                self.pos = save;
                                out.push('\u{FFFD}');
                            } else if let Some(ch) = char::from_u32(first) {
                                out.push(ch);
                            } else {
                                out.push('\u{FFFD}');
                            }
                        }
                        other => {
                            return Err(format!(
                                "位置 {} 处非法转义 \\{}",
                                self.pos - 1,
                                other as char
                            ));
                        }
                    }
                }
                _ => {
                    // UTF-8 多字节：按原始字节收集后统一解码（`"`/`\` 均为 ASCII，不会落入续字节）
                    let start = self.pos - 1;
                    self.pos = start;
                    while let Some(byte) = self.peek() {
                        if byte == b'"' || byte == b'\\' {
                            break;
                        }
                        self.pos += 1;
                    }
                    let chunk = std::str::from_utf8(&self.bytes[start..self.pos])
                        .map_err(|e| format!("位置 {start} 处字符串不是合法 UTF-8：{e}"))?;
                    out.push_str(chunk);
                }
            }
        }
        Ok(out)
    }

    fn parse_hex4(&mut self) -> Result<u32, String> {
        let mut value = 0u32;
        for _ in 0..4 {
            let byte = self.bump()?;
            let digit = (byte as char)
                .to_digit(16)
                .ok_or_else(|| format!("位置 {} 处非法十六进制字符", self.pos - 1))?;
            value = (value << 4) | digit;
        }
        Ok(value)
    }

    fn parse_number(&mut self) -> Result<JsonValue, String> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        while let Some(byte) = self.peek() {
            if byte.is_ascii_digit() || matches!(byte, b'.' | b'e' | b'E' | b'+' | b'-') {
                self.pos += 1;
            } else {
                break;
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|e| format!("数字不是合法 UTF-8：{e}"))?;
        let is_float = text.contains('.') || text.contains('e') || text.contains('E');
        // edition 2021 不支持 let-chain，用 match 守卫避免 collapsible_if
        match text.parse::<i64>() {
            Ok(value) if !is_float => return Ok(JsonValue::Int(value)),
            _ => {}
        }
        text.parse::<f64>()
            .map(JsonValue::Float)
            .map_err(|_| format!("位置 {start} 处数字非法：{text}"))
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

    #[test]
    fn parses_bare_keys_comments_and_trailing_commas() {
        let text = r#"
        // C# SimpleJsonString 风格
        [
            { Id:1, Name:"张三", Age:30 },
            { Id:2, Name:"a\"b", Age:18, },
        ]
        "#;
        let value = parse_lax(text).unwrap();
        let JsonValue::Array(items) = value else {
            panic!("应为数组");
        };
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].get("id"), Some(&JsonValue::Int(1)));
        assert_eq!(items[1].get("Name"), Some(&JsonValue::Text("a\"b".into())));
        assert_eq!(items[0].get("AGE").and_then(JsonValue::to_i64), Some(30));
    }

    #[test]
    fn parses_standard_json_and_surrogate_pairs() {
        let value = parse_lax(r#"{"s":"\uD83D\uDE00","n":[1,2.5,true,null]}"#).unwrap();
        assert_eq!(value.get("s").and_then(JsonValue::as_str), Some("😀"));
        let items = value.get("n").and_then(JsonValue::as_array).unwrap();
        assert_eq!(items[0], JsonValue::Int(1));
        assert_eq!(items[1], JsonValue::Float(2.5));
        assert_eq!(items[2], JsonValue::Bool(true));
        assert_eq!(items[3], JsonValue::Null);
    }

    #[test]
    fn value_helpers_convert_leniently() {
        assert_eq!(JsonValue::Text("42".into()).to_i64(), Some(42));
        assert_eq!(JsonValue::Float(1.5).to_f64(), Some(1.5));
        assert_eq!(JsonValue::Text("yes".into()).to_bool(), Some(true));
        assert_eq!(JsonValue::Int(0).to_bool(), Some(false));
        assert_eq!(JsonValue::Bool(true).to_text().as_deref(), Some("true"));
        assert_eq!(JsonValue::Array(Vec::new()).to_text(), None);
        assert_eq!(JsonValue::Null.to_i64(), None);
    }

    #[test]
    fn write_quoted_matches_csharp_clear_newline() {
        let mut out = String::new();
        write_quoted(&mut out, "a\"b\\c\r\nd\t");
        assert_eq!(out, "\"a\\\"b\\\\cd\"");
    }

    #[test]
    fn format_number_matches_csharp_double_default() {
        assert_eq!(format_number(1.0), "1");
        assert_eq!(format_number(1.5), "1.5");
        assert_eq!(format_number(-0.0), "0");
        assert_eq!(format_number(1e20), "100000000000000000000");
        assert_eq!(format_number(f64::NAN), "NaN");
        assert_eq!(format_number(f64::INFINITY), "Infinity");
        assert_eq!(format_number(f64::NEG_INFINITY), "-Infinity");
    }

    #[test]
    fn bool_text_reuses_shared_parser() {
        assert_eq!(JsonValue::Text("是".into()).to_bool(), Some(true));
        assert_eq!(JsonValue::Text(" 否 ".into()).to_bool(), Some(false));
        assert_eq!(value_to_bool(&json!("OFF")), Some(false));
    }

    #[test]
    fn parse_errors_carry_position() {
        assert!(parse_lax("{ this is not json").is_err());
        assert!(parse_lax("[1,]").is_ok());
        assert!(parse_lax("{\"a\":1} trailing").is_err());
    }
}
