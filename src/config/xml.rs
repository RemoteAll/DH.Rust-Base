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

/// 读取任意嵌套 XML 为 JSON 值（元素→对象、属性→键、重复同名子元素→数组、文本→字符串）。
///
/// 用于导入 C# NewLife 生成的配置（如 `StarAgent.config`：
/// `<Services><ServiceInfo Name=".." FileName=".." /></Services>`）。
/// 无属性无子元素的空元素映射为 `""`；有属性或子元素且带文本时，文本放入 `#text` 键；
/// 返回值以根元素名作为顶层键（如 `{"StarAgent": {...}}`）；无根元素时返回 `Null`。
pub fn read_to_json(text: &str) -> Result<serde_json::Value, ConfigError> {
    use serde_json::{Map, Value};

    let text = text.trim_start_matches('\u{feff}');

    struct Frame {
        name: String,
        attrs: Map<String, Value>,
        children: Map<String, Value>,
        text: String,
    }

    fn collect_attrs(e: &quick_xml::events::BytesStart<'_>) -> Map<String, Value> {
        let mut attrs = Map::new();
        for attr in e.attributes().flatten() {
            let key = String::from_utf8_lossy(attr.key.as_ref()).to_string();
            let value = attr
                .unescape_value()
                .map(|v| v.to_string())
                .unwrap_or_default();
            attrs.insert(key, Value::String(value));
        }
        attrs
    }

    /// 同名重复元素在父级容器中提升为数组（与 C# 列表序列化形态对应）。
    fn insert_child(container: &mut Map<String, Value>, name: String, value: Value) {
        match container.get_mut(&name) {
            Some(Value::Array(list)) => list.push(value),
            Some(existing) => {
                let prev = existing.take();
                *existing = Value::Array(vec![prev, value]);
            }
            None => {
                container.insert(name, value);
            }
        }
    }

    /// 收帧：构造元素值并挂到父容器。
    fn finish_frame(frame: Frame, parent: &mut Map<String, Value>) {
        let text = frame.text.trim();
        let value = if frame.children.is_empty() && frame.attrs.is_empty() {
            Value::String(text.to_string())
        } else {
            let mut object = frame.children;
            for (key, attr) in frame.attrs {
                object.insert(key, attr);
            }
            if !text.is_empty() {
                object.insert("#text".to_string(), Value::String(text.to_string()));
            }
            Value::Object(object)
        };
        insert_child(parent, frame.name, value);
    }

    let mut reader = Reader::from_str(text);
    reader.config_mut().trim_text(false);

    let mut stack: Vec<Frame> = Vec::new();
    let mut root: Option<(String, Value)> = None;

    loop {
        match reader.read_event() {
            Err(e) => return Err(ConfigError::Parse(format!("XML 解析失败: {e}"))),
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => {
                let name = name_of(&e);
                let attrs = collect_attrs(&e);
                stack.push(Frame {
                    name,
                    attrs,
                    children: Map::new(),
                    text: String::new(),
                });
            }
            Ok(Event::Empty(e)) => {
                let name = name_of(&e);
                let attrs = collect_attrs(&e);
                let value = if attrs.is_empty() {
                    Value::String(String::new())
                } else {
                    Value::Object(attrs)
                };
                match stack.last_mut() {
                    Some(parent) => insert_child(&mut parent.children, name, value),
                    None => {
                        if root.is_none() {
                            root = Some((name, value));
                        }
                    }
                }
            }
            Ok(Event::Text(t)) => {
                if let Some(frame) = stack.last_mut() {
                    if let Ok(unescaped) = t.unescape() {
                        frame.text.push_str(&unescaped);
                    }
                }
            }
            Ok(Event::End(_)) => {
                let Some(frame) = stack.pop() else { continue };
                match stack.last_mut() {
                    Some(parent) => finish_frame(frame, &mut parent.children),
                    None => {
                        if root.is_none() {
                            let name = frame.name.clone();
                            let mut holder = Map::new();
                            finish_frame(frame, &mut holder);
                            root = holder.remove(&name).map(|v| (name, v));
                        }
                    }
                }
            }
            Ok(_) => {}
        }
    }

    match root {
        Some((name, value)) => {
            let mut object = Map::new();
            object.insert(name, value);
            Ok(Value::Object(object))
        }
        None => Ok(Value::Null),
    }
}

/// 根下子元素键值 upsert（保留注释/属性/排版）：
///
/// - 已存在的元素只替换其文本值（原注释、缩进、其它元素原样保留）；空元素 `<Key/>` 在值非空时展开，
///   值为空串时清空元素文本（`<Key></Key>`，支持“清空字段”保存），`<Key/>` 形态保持自闭合；
/// - 缺失的键在根元素结束前插入（形如 `  <!--注释-->` + `  <Key>值</Key>`，注释为空则不插说明行）；
/// - 未在 `items` 中的元素（如 C# 特有字段）完全不动。
///
/// `items` 为（键名, 值, 注释）三元组，用于 NewLife 风格 ` <Key>value</Key> ` 配置的保注释维护。
pub fn upsert_root_values(
    text: &str,
    items: &[(String, String, String)],
) -> Result<String, ConfigError> {
    use std::collections::{HashMap, HashSet};

    use quick_xml::events::{BytesEnd, BytesStart, BytesText};
    use quick_xml::Writer;

    let text = text.trim_start_matches('\u{feff}');

    let lookup: HashMap<&str, (&str, &str)> = items
        .iter()
        .map(|(k, v, c)| (k.as_str(), (v.as_str(), c.as_str())))
        .collect();
    let mut seen: HashSet<&str> = HashSet::new();

    let mut reader = Reader::from_str(text);
    reader.config_mut().trim_text(false);
    let mut writer = Writer::new(Vec::new());
    let write_err = |e: quick_xml::Error| ConfigError::Parse(format!("XML 写入失败: {e}"));

    // 输入文本的根结束标签前是否粘接（缺换行）；重写时统一规范化
    let needs_nl = needs_trailing_newline(text);

    let mut stack: Vec<String> = Vec::new();
    let mut pending: Option<(&str, bool)> = None; // （待替换的键, 是否已写入值）

    loop {
        match reader.read_event() {
            Err(e) => return Err(ConfigError::Parse(format!("XML 解析失败: {e}"))),
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => {
                let name = name_of(&e);
                stack.push(name.clone());
                if stack.len() == 2 {
                    if let Some((key, _)) = lookup.get_key_value(name.as_str()) {
                        pending = Some((key, false));
                        seen.insert(*key);
                    }
                }
                writer.write_event(Event::Start(e)).map_err(write_err)?;
            }
            Ok(Event::Empty(e)) => {
                let name = name_of(&e);
                if stack.len() == 1 {
                    if let Some((key, (value, _))) = lookup.get_key_value(name.as_str()) {
                        seen.insert(*key);
                        if !value.is_empty() {
                            writer
                                .write_event(Event::Start(BytesStart::new(name.as_str())))
                                .map_err(write_err)?;
                            writer
                                .write_event(Event::Text(BytesText::new(value)))
                                .map_err(write_err)?;
                            writer
                                .write_event(Event::End(BytesEnd::new(name.as_str())))
                                .map_err(write_err)?;
                            continue;
                        }
                    }
                }
                writer.write_event(Event::Empty(e)).map_err(write_err)?;
            }
            Ok(Event::Text(t)) => {
                let mut replaced = false;
                if let Some((key, false)) = pending {
                    let value = lookup[key].0;
                    // 空值同样视为“已替换”：丢弃旧文本（清空元素），支持“清空字段”保存
                    if !value.is_empty() {
                        writer
                            .write_event(Event::Text(BytesText::new(value)))
                            .map_err(write_err)?;
                    }
                    replaced = true;
                }
                if replaced {
                    if let Some((_, written)) = pending.as_mut() {
                        *written = true;
                    }
                } else {
                    writer.write_event(Event::Text(t)).map_err(write_err)?;
                }
            }
            Ok(Event::End(e)) => {
                let name = name_of_end(&e);
                if let Some((key, written)) = pending.take() {
                    if key == name && !written {
                        let value = lookup[key].0;
                        if !value.is_empty() {
                            writer
                                .write_event(Event::Text(BytesText::new(value)))
                                .map_err(write_err)?;
                        }
                    } else if key != name {
                        pending = Some((key, written));
                    }
                }
                stack.pop();

                // 根结束前：插入缺失的键
                if stack.is_empty() {
                    let mut inserted = false;
                    for (key, value, comment) in items {
                        if seen.contains(key.as_str()) {
                            continue;
                        }
                        inserted = true;
                        writer
                            .write_event(Event::Text(BytesText::from_escaped("\n  ")))
                            .map_err(write_err)?;
                        if !comment.is_empty() {
                            writer
                                .write_event(Event::Comment(BytesText::new(comment)))
                                .map_err(write_err)?;
                            writer
                                .write_event(Event::Text(BytesText::from_escaped("\n  ")))
                                .map_err(write_err)?;
                        }
                        writer
                            .write_event(Event::Start(BytesStart::new(key.as_str())))
                            .map_err(write_err)?;
                        writer
                            .write_event(Event::Text(BytesText::new(value)))
                            .map_err(write_err)?;
                        writer
                            .write_event(Event::End(BytesEnd::new(key.as_str())))
                            .map_err(write_err)?;
                    }
                    // 插入内容与根结束标签之间补换行（与元素块收尾一致）；
                    // 输入本来粘接时也顺带规范化
                    if inserted || needs_nl {
                        writer
                            .write_event(Event::Text(BytesText::from_escaped("\n")))
                            .map_err(write_err)?;
                    }
                }
                writer.write_event(Event::End(e)).map_err(write_err)?;
            }
            Ok(other) => {
                writer.write_event(other).map_err(write_err)?;
            }
        }
    }

    String::from_utf8(writer.into_inner())
        .map_err(|e| ConfigError::Parse(format!("XML 输出编码错误: {e}")))
}

/// 替换根下指定子元素的整段内容（保留节前注释；节内原内容被替换，结束标签保持原缩进）。
///
/// 未找到该节时在根元素结束前插入。`inner` 为节内完整内容（含缩进与行尾换行，
/// 如 `    <ServiceInfo Name="a" />\n`）；`inner` 为空时输出空元素 `<name></name>`。
pub fn replace_root_section(text: &str, name: &str, inner: &str) -> Result<String, ConfigError> {
    use quick_xml::events::{BytesEnd, BytesStart, BytesText};
    use quick_xml::Writer;

    let text = text.trim_start_matches('\u{feff}');

    let mut reader = Reader::from_str(text);
    reader.config_mut().trim_text(false);
    let mut writer = Writer::new(Vec::new());
    let write_err = |e: quick_xml::Error| ConfigError::Parse(format!("XML 写入失败: {e}"));

    fn write_section(
        writer: &mut Writer<Vec<u8>>,
        name: &str,
        inner: &str,
        indent: &str,
    ) -> Result<(), ConfigError> {
        let err = |e: quick_xml::Error| ConfigError::Parse(format!("XML 写入失败: {e}"));
        writer
            .write_event(Event::Start(BytesStart::new(name)))
            .map_err(err)?;
        if !inner.is_empty() {
            writer
                .write_event(Event::Text(BytesText::from_escaped("\n")))
                .map_err(err)?;
            writer
                .write_event(Event::Text(BytesText::from_escaped(inner)))
                .map_err(err)?;
            if !indent.is_empty() {
                writer
                    .write_event(Event::Text(BytesText::from_escaped(indent)))
                    .map_err(err)?;
            }
        }
        writer
            .write_event(Event::End(BytesEnd::new(name)))
            .map_err(err)?;
        Ok(())
    }

    let mut stack: Vec<String> = Vec::new();
    let mut skip: Option<usize> = None; // 跳过同名嵌套层级计数
    let mut replaced = false;
    // 最近一个文本事件中最后一个换行后的空白（即下一元素的行首缩进）
    let mut last_indent = String::new();
    // 输入文本的根结束标签前是否粘接（缺换行）；重写时统一规范化
    let needs_nl = needs_trailing_newline(text);

    loop {
        match reader.read_event() {
            Err(e) => return Err(ConfigError::Parse(format!("XML 解析失败: {e}"))),
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => {
                let n = name_of(&e);
                if let Some(level) = &mut skip {
                    if n == name {
                        *level += 1;
                    }
                    continue;
                }
                if stack.len() == 1 && n == name {
                    write_section(&mut writer, name, inner, &last_indent)?;
                    replaced = true;
                    skip = Some(1);
                    continue;
                }
                stack.push(n);
                writer.write_event(Event::Start(e)).map_err(write_err)?;
            }
            Ok(Event::Empty(e)) => {
                let n = name_of(&e);
                if skip.is_some() {
                    continue;
                }
                if stack.len() == 1 && n == name {
                    write_section(&mut writer, name, inner, &last_indent)?;
                    replaced = true;
                    continue;
                }
                writer.write_event(Event::Empty(e)).map_err(write_err)?;
            }
            Ok(Event::End(e)) => {
                let n = name_of_end(&e);
                if let Some(level) = &mut skip {
                    if n == name {
                        *level -= 1;
                        if *level == 0 {
                            skip = None;
                        }
                    }
                    continue;
                }
                stack.pop();
                if stack.is_empty() {
                    if !replaced {
                        // 根结束前：插入新节；结束标签后补换行（与既有节排版一致，
                        // 避免 `</Section></Root>` 粘接导致渲染输出不幂等）
                        writer
                            .write_event(Event::Text(BytesText::from_escaped("\n  ")))
                            .map_err(write_err)?;
                        write_section(&mut writer, name, inner, "  ")?;
                        writer
                            .write_event(Event::Text(BytesText::from_escaped("\n")))
                            .map_err(write_err)?;
                        replaced = true;
                    } else if needs_nl {
                        // 输入本来粘接时顺带规范化
                        writer
                            .write_event(Event::Text(BytesText::from_escaped("\n")))
                            .map_err(write_err)?;
                    }
                }
                writer.write_event(Event::End(e)).map_err(write_err)?;
            }
            Ok(Event::Text(t)) => {
                if skip.is_none() {
                    let raw = String::from_utf8_lossy(t.as_ref());
                    if let Some(pos) = raw.rfind('\n') {
                        let tail = raw[pos + 1..].to_string();
                        if tail.chars().all(|c| c == ' ' || c == '\t') {
                            last_indent = tail;
                        } else {
                            last_indent.clear();
                        }
                    }
                    writer.write_event(Event::Text(t)).map_err(write_err)?;
                }
            }
            Ok(other) => {
                if skip.is_none() {
                    writer.write_event(other).map_err(write_err)?;
                }
            }
        }
    }

    String::from_utf8(writer.into_inner())
        .map_err(|e| ConfigError::Parse(format!("XML 输出编码错误: {e}")))
}

fn name_of(e: &quick_xml::events::BytesStart<'_>) -> String {
    String::from_utf8_lossy(e.name().as_ref()).to_string()
}

/// 文本的根结束标签（最后一个 `</`）之前是否缺换行（存在 `...</Key></Root>` 粘接格式）。
fn needs_trailing_newline(text: &str) -> bool {
    match text.rfind("</") {
        Some(i) => !text[..i].trim_end_matches([' ', '\t', '\r']).ends_with('\n'),
        None => false,
    }
}

fn name_of_end(e: &quick_xml::events::BytesEnd<'_>) -> String {
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

    #[test]
    fn read_nested_xml_to_json_matches_csharp_shape() {
        let text = r#"<?xml version="1.0" encoding="utf-8"?>
<StarAgent>
  <!--调试开关。默认true-->
  <Debug>true</Debug>
  <LocalPort>5500</LocalPort>
  <Services>
    <ServiceInfo Name="StarServer" FileName="dotnet" Arguments="StarServer.dll" Enable="true" />
    <ServiceInfo Name="StarWeb" FileName="StarWeb.zip" Arguments="urls=http://*:6680" Enable="true" />
  </Services>
</StarAgent>"#;

        let json = read_to_json(text).unwrap();
        assert_eq!(json["StarAgent"]["Debug"], "true");
        assert_eq!(json["StarAgent"]["LocalPort"], "5500");
        let services = json["StarAgent"]["Services"]["ServiceInfo"]
            .as_array()
            .expect("重复 ServiceInfo 应提升为数组");
        assert_eq!(services.len(), 2);
        assert_eq!(services[0]["Name"], "StarServer");
        assert_eq!(services[1]["FileName"], "StarWeb.zip");
    }

    #[test]
    fn read_to_json_strips_utf8_bom() {
        // C# XmlSerializer 保存的文件带 UTF-8 BOM；读取与重写时都应剥离
        let text = "\u{feff}<?xml version=\"1.0\"?>\n<StarAgent><LocalPort>5500</LocalPort></StarAgent>";
        let json = read_to_json(text).unwrap();
        assert_eq!(json["StarAgent"]["LocalPort"], "5500");

        let items = vec![("LocalPort".to_string(), "5600".to_string(), String::new())];
        let out = upsert_root_values(text, &items).unwrap();
        assert!(!out.starts_with('\u{feff}'), "输出不应带 BOM: {out:?}");
        assert!(out.contains("<LocalPort>5600</LocalPort>"), "{out}");

        let out = replace_root_section(text, "Services", "").unwrap();
        assert!(!out.starts_with('\u{feff}'), "输出不应带 BOM: {out:?}");
        assert!(out.contains("<Services></Services>"), "{out}");
    }

    #[test]
    fn upsert_root_values_replaces_and_inserts_keeping_comments() {
        let text = r#"<?xml version="1.0" encoding="utf-8"?>
<StarAgent>
  <!--调试开关。默认true-->
  <Debug>true</Debug>
  <!--本地端口。默认5500-->
  <LocalPort>5500</LocalPort>
</StarAgent>"#;
        let items = vec![
            ("Debug".to_string(), "false".to_string(), String::new()),
            ("LocalPort".to_string(), "5600".to_string(), String::new()),
            (
                "WebUserName".to_string(),
                "admin".to_string(),
                "面板用户名".to_string(),
            ),
        ];

        let out = upsert_root_values(text, &items).unwrap();
        assert!(out.contains("<Debug>false</Debug>"), "{out}");
        assert!(out.contains("<LocalPort>5600</LocalPort>"), "{out}");
        assert!(out.contains("<!--调试开关。默认true-->"), "原注释应保留: {out}");
        assert!(out.contains("<!--面板用户名-->"), "插入应带注释: {out}");
        assert!(out.contains("<WebUserName>admin</WebUserName>"), "{out}");
        assert!(!out.contains("<Debug>true</Debug>"), "旧值应被替换: {out}");
        assert!(
            out.contains("</WebUserName>\n</StarAgent>"),
            "插入内容与根结束标签之间应有换行: {out}"
        );
    }

    #[test]
    fn upsert_root_values_clears_text_with_empty_value() {
        // 空值应清空元素文本（支持“清空字段”保存，如 WebLogs/PortTrafficPorts）
        let text = "<StarAgent>\n  <WebLogs>demo=/tmp/a.log</WebLogs>\n  <Server>x</Server>\n</StarAgent>";
        let items = vec![
            ("WebLogs".to_string(), String::new(), String::new()),
            ("Server".to_string(), "y".to_string(), String::new()),
        ];
        let out = upsert_root_values(text, &items).unwrap();
        assert!(
            out.contains("<WebLogs></WebLogs>"),
            "空值应清空元素文本: {out}"
        );
        assert!(!out.contains("demo="), "旧值应被清除: {out}");
        assert!(out.contains("<Server>y</Server>"), "非空值正常替换: {out}");

        // 自闭合形态：空值时保持自闭合
        let text = "<StarAgent>\n  <WebLogs/>\n</StarAgent>";
        let items = vec![("WebLogs".to_string(), String::new(), String::new())];
        let out = upsert_root_values(text, &items).unwrap();
        assert!(out.contains("<WebLogs/>"), "自闭合应保持: {out}");
    }

    #[test]
    fn glued_root_end_gets_normalized() {
        // 历史文件可能形如 `...</Key></Root>`（根结束标签与上一元素粘接），重写时应补换行
        let text = "<StarAgent>\n  <LocalPort>5500</LocalPort></StarAgent>";
        let items = vec![("LocalPort".to_string(), "5600".to_string(), String::new())];
        let out = upsert_root_values(text, &items).unwrap();
        assert!(
            out.contains("<LocalPort>5600</LocalPort>\n</StarAgent>"),
            "upsert 应补换行: {out}"
        );

        let text = "<StarAgent>\n  <Services></Services></StarAgent>";
        let inner = "    <ServiceInfo Name=\"a\" />\n";
        let out = replace_root_section(text, "Services", inner).unwrap();
        assert!(
            out.contains("  </Services>\n</StarAgent>"),
            "replace 应补换行: {out}"
        );
    }

    #[test]
    fn replace_root_section_insert_normalizes_trailing_newline() {
        // 节不存在时插入：结束标签后应补换行（此前输出 `</Services></StarAgent>` 粘接，
        // 导致“新增插入”与“整段替换”两条路径输出不一致、渲染不幂等）
        let text = "<StarAgent>\n  <A>1</A>\n</StarAgent>";
        let inner = "    <ServiceInfo Name=\"a\" />\n";
        let out = replace_root_section(text, "Services", inner).unwrap();
        assert!(out.contains("</Services>\n</StarAgent>"), "插入节应补换行: {out}");
        assert!(!out.contains("</Services></StarAgent>"), "不应粘接: {out}");

        // 幂等：插入后再执行（走整段替换路径）输出字节一致
        let out2 = replace_root_section(&out, "Services", inner).unwrap();
        assert_eq!(out2, out, "两条路径应输出一致（幂等）");
    }

    #[test]
    fn replace_root_section_replaces_and_keeps_surroundings() {
        let text = r#"<StarAgent>
  <Delay>3000</Delay>
  <!--应用服务集合-->
  <Services>
    <ServiceInfo Name="old" FileName="old.zip" />
  </Services>
</StarAgent>"#;
        let inner = "    <ServiceInfo Name=\"a\" FileName=\"a.zip\" />\n";

        let out = replace_root_section(text, "Services", inner).unwrap();
        assert!(out.contains("<!--应用服务集合-->"), "节前注释应保留: {out}");
        assert!(out.contains("Name=\"a\""), "{out}");
        assert!(!out.contains("Name=\"old\""), "旧内容应被替换: {out}");
        assert!(out.contains("<Delay>3000</Delay>"), "{out}");
        assert!(out.contains("  </Services>"), "结束标签应保持原缩进: {out}");

        // 空 inner：输出空元素
        let out = replace_root_section(text, "Services", "").unwrap();
        assert!(out.contains("<Services></Services>"), "空节应为空元素: {out}");
    }
}
