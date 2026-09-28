//! XML 配置读写，与 C# `XmlConfigProvider` 的扁平 XML 格式互通。
//!
//! C# 生成的 `Config/Core.config` 形如：
//!
//! ```xml
//! <?xml version="1.0" encoding="utf-8"?>
//! <Core>
//!   <!--启用全局调试。XTrace.Debug-->
//!   <Debug>true</Debug>
//!   <LogPath />
//! </Core>
//! ```
//!
//! 本模块只支持扁平结构（根节点下的子元素为键值），与 `Setting` 模型一致；
//! 遇到嵌套节点会返回解析错误。

use quick_xml::events::Event;
use quick_xml::Reader;

use super::ConfigError;

/// 读取扁平 XML，返回 (名称, 文本值) 列表。注释、声明与空白被忽略。
pub(crate) fn read_fields(text: &str) -> Result<Vec<(String, String)>, ConfigError> {
    let mut reader = Reader::from_str(text);
    reader.config_mut().trim_text(true);

    let mut fields = Vec::new();
    let mut depth = 0usize;
    let mut current: Option<String> = None;
    let mut value = String::new();

    loop {
        match reader.read_event() {
            Err(e) => return Err(ConfigError::Parse(format!("XML 解析失败: {e}"))),
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => {
                depth += 1;
                if depth == 2 {
                    current = Some(name_of(&e));
                    value.clear();
                } else if depth > 2 {
                    return Err(ConfigError::Parse(format!(
                        "不支持的嵌套配置节点: {}",
                        name_of(&e)
                    )));
                }
            }
            Ok(Event::Empty(e)) => {
                // 空元素，如 <LogPath />
                if depth == 1 {
                    fields.push((name_of(&e), String::new()));
                }
            }
            Ok(Event::Text(t)) => {
                if depth == 2 {
                    let unescaped = t
                        .unescape()
                        .map_err(|e| ConfigError::Parse(format!("XML 文本解析失败: {e}")))?;
                    value.push_str(&unescaped);
                }
            }
            Ok(Event::CData(t)) => {
                if depth == 2 {
                    value.push_str(&String::from_utf8_lossy(t.as_ref()));
                }
            }
            Ok(Event::End(_)) => {
                if depth >= 1 {
                    depth -= 1;
                }
                if depth == 1 {
                    if let Some(name) = current.take() {
                        fields.push((name, value.trim().to_string()));
                    }
                    value.clear();
                }
            }
            // 声明、注释、处理指令等一律忽略
            _ => {}
        }
    }

    Ok(fields)
}

/// 写出扁平 XML。`root` 为根节点名称（C# 使用文件名去掉扩展名）。
pub(crate) fn write_fields(root: &str, fields: &[(&str, &str, String)]) -> String {
    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
    out.push_str(&format!("<{root}>\n"));
    for (name, comment, value) in fields {
        if !comment.is_empty() {
            out.push_str("  <!--");
            out.push_str(comment);
            out.push_str("-->\n");
        }
        if value.is_empty() {
            out.push_str(&format!("  <{name} />\n"));
        } else {
            out.push_str(&format!("  <{name}>{}</{name}>\n", escape_text(value)));
        }
    }
    out.push_str(&format!("</{root}>\n"));
    out
}

fn name_of(e: &quick_xml::events::BytesStart<'_>) -> String {
    String::from_utf8_lossy(e.name().as_ref()).to_string()
}

fn escape_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_flat_xml() {
        let text = r#"<?xml version="1.0" encoding="utf-8"?>
<Core>
  <!--启用全局调试。XTrace.Debug-->
  <Debug>true</Debug>
  <!--文件日志目录。默认Log子目录-->
  <LogPath />
  <LogFileMaxBytes>10</LogFileMaxBytes>
  <ServiceAddress>http://a&amp;b/</ServiceAddress>
</Core>"#;

        let fields = read_fields(text).unwrap();
        assert_eq!(fields.len(), 4);
        assert_eq!(fields[0], ("Debug".to_string(), "true".to_string()));
        assert_eq!(fields[1], ("LogPath".to_string(), String::new()));
        assert_eq!(fields[2], ("LogFileMaxBytes".to_string(), "10".to_string()));
        assert_eq!(
            fields[3],
            ("ServiceAddress".to_string(), "http://a&b/".to_string())
        );
    }

    #[test]
    fn reject_nested_xml() {
        let text = "<Core><Parent><Child>1</Child></Parent></Core>";
        assert!(read_fields(text).is_err());
    }

    #[test]
    fn write_and_read_roundtrip() {
        let fields = vec![
            ("Debug", "启用全局调试。XTrace.Debug", "true".to_string()),
            ("LogPath", "文件日志目录。默认Log子目录", String::new()),
            ("ServiceAddress", "", "http://x/?a=1&b=2".to_string()),
        ];
        let text = write_fields("Core", &fields);
        assert!(text.contains("<Debug>true</Debug>"));
        assert!(text.contains("<!--启用全局调试。XTrace.Debug-->"));
        assert!(text.contains("<LogPath />"));
        assert!(text.contains("<ServiceAddress>http://x/?a=1&amp;b=2</ServiceAddress>"));

        let parsed = read_fields(&text).unwrap();
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].0, "Debug");
        assert_eq!(parsed[2].1, "http://x/?a=1&b=2");
    }
}
