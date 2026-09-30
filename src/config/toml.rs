//! TOML 配置维护管线：为“现场手工编辑”的配置文件提供保注释的补齐/修正/识别能力。
//!
//! 与本模块 JSON 版 [`Config`](super::Config) 语义对齐、面向 TOML 格式复用：
//! - [`parse_document`]：解析文本为可编辑文档（保留注释与排版）
//! - [`merge_missing_keys`]：以示例模板补齐缺失配置项（连注释与键装饰一起带入）
//! - [`apply_type_fixes`]：把字段级容错（[`coerce_json`](super::coerce_json)）结果回写文档
//! - [`unknown_keys`]：收集无法识别的配置项（键名拼错/放错 `[段]`，支持自由映射段豁免）
//! - [`backup_and_write`]：先备份 `.bak` 再覆盖写入
//!
//! 完整流程编排（加载/热解析/损坏重建/未知项提示）由调用方组合，参考
//! tcp-scanner-server 的 config.rs（本模块的实践来源）。
//!
//! 启用方式：`dhrust = { path = "…", features = ["toml"] }`。

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use serde_json::Value;
use toml_edit::{DocumentMut, Item, Table};

/// 解析 TOML 文本为可编辑文档（保留注释与排版）。
pub fn parse_document(text: &str) -> Result<DocumentMut, String> {
    text.parse::<DocumentMut>()
        .map_err(|e| format!("TOML 语法错误：{e}"))
}

/// 文档 → JSON 值（经 toml::Value 转换，与运行时解析语义一致）。
pub fn document_to_json(doc: &DocumentMut) -> Result<Value, String> {
    let value: toml::Value =
        toml::from_str(&doc.to_string()).map_err(|e| format!("TOML 语法错误：{e}"))?;
    serde_json::to_value(&value).map_err(|e| format!("配置转换失败：{e}"))
}

/// 收集 JSON 中无法识别的配置项（返回形如 `server.heartbeat` 的路径，已排序）。
///
/// - `template`：默认配置序列化（字段名基准）
/// - `free_form_sections`：自由映射段（键名由用户自定义，如设备别名表），不做识别校验
pub fn unknown_keys(json: &Value, template: &Value, free_form_sections: &[&str]) -> Vec<String> {
    let mut out = Vec::new();

    let Some(obj) = json.as_object() else {
        return out;
    };
    let tmpl = template.as_object();

    for (section, value) in obj {
        if let Some(inner) = value.as_object() {
            let Some(known) = tmpl.and_then(|t| t.get(section)).and_then(|v| v.as_object()) else {
                // 整段无法识别（如把 [frame] 拼成 [frames]）
                out.push(section.clone());
                continue;
            };
            if free_form_sections.contains(&section.as_str()) {
                continue;
            }
            for key in inner.keys() {
                if !known.contains_key(key) {
                    out.push(format!("{section}.{key}"));
                }
            }
        } else if tmpl.is_some_and(|t| !t.contains_key(section)) {
            out.push(section.clone());
        }
    }

    out.sort();
    out
}

/// 以示例模板为准补齐缺失配置项（保留模板中的注释与键装饰），返回补齐的键数量。
pub fn merge_missing_keys(target: &mut Table, template: &Table) -> usize {
    let mut added = 0;

    for (key, tmpl_item) in template.iter() {
        match tmpl_item {
            Item::Table(tmpl_table) => {
                // 仅含注释的空表（如示例 [aliases]）不主动补
                if tmpl_table.is_empty() {
                    continue;
                }
                match target.get_mut(key) {
                    Some(Item::Table(user_table)) => {
                        added += merge_missing_keys(user_table, tmpl_table);
                    }
                    // 类型不符的节交给类型容错处理
                    Some(_) => {}
                    None => {
                        if let Some(k) = template.key(key) {
                            added += count_leaves(tmpl_table);
                            target.insert_formatted(k, tmpl_item.clone());
                        }
                    }
                }
            }
            Item::Value(_) if target.get(key).is_none() => {
                if let Some(k) = template.key(key) {
                    added += 1;
                    target.insert_formatted(k, tmpl_item.clone());
                }
            }
            // 数组表等其它形态当前配置未使用
            _ => {}
        }
    }

    added
}

/// 统计表内叶子配置项数量（含嵌套子表）。
pub fn count_leaves(table: &Table) -> usize {
    let mut n = 0;
    for (_, item) in table.iter() {
        match item {
            Item::Table(t) => n += count_leaves(t),
            Item::Value(_) => n += 1,
            _ => {}
        }
    }
    n
}

/// 把类型容错后的值回写到文档（保留原键装饰与行内注释），返回修正的键数量。
pub fn apply_type_fixes(doc: &mut DocumentMut, base: &Value, coerced: &Value) -> usize {
    let mut fixes = Vec::new();
    collect_fixes(base, coerced, &mut Vec::new(), &mut fixes);

    let mut applied = 0;
    for (path, value) in fixes {
        if let Some(new_value) = json_to_toml_edit(&value) {
            if set_at_path(doc, &path, new_value) {
                applied += 1;
            }
        }
    }
    applied
}

/// 对比转换前后的值，收集需要修正的叶子路径。
fn collect_fixes(
    base: &Value,
    coerced: &Value,
    path: &mut Vec<String>,
    out: &mut Vec<(Vec<String>, Value)>,
) {
    match (base, coerced) {
        (Value::Object(b), Value::Object(c)) => {
            for (key, cv) in c {
                if let Some(bv) = b.get(key) {
                    path.push(key.clone());
                    collect_fixes(bv, cv, path, out);
                    path.pop();
                }
            }
        }
        (b, c) => {
            if b != c {
                out.push((path.clone(), c.clone()));
            }
        }
    }
}

/// 按路径替换文档中的值（沿用原值装饰，保留空格与行内注释）。
fn set_at_path(doc: &mut DocumentMut, path: &[String], value: toml_edit::Value) -> bool {
    let Some((first, rest)) = path.split_first() else {
        return false;
    };
    let Some(mut item) = doc.get_mut(first) else {
        return false;
    };

    // 定位到父表（rest 的最后一段是目标键）
    let Some((last, parents)) = rest.split_last() else {
        return false;
    };
    for seg in parents {
        match item.as_table_like_mut().and_then(|t| t.get_mut(seg)) {
            Some(next) => item = next,
            None => return false,
        }
    }

    let Some(table) = item.as_table_like_mut() else {
        return false;
    };
    let Some(old) = table.get_mut(last) else {
        return false;
    };
    match old.as_value() {
        Some(old_value) => {
            let mut new_value = value;
            *new_value.decor_mut() = old_value.decor().clone();
            *old = Item::Value(new_value);
            true
        }
        None => false,
    }
}

/// JSON 值 → toml_edit 值（用于类型修正回写）。
fn json_to_toml_edit(value: &Value) -> Option<toml_edit::Value> {
    use serde_json::Value as Json;
    use toml_edit::Value as Toml;

    Some(match value {
        Json::Null => return None,
        Json::Bool(b) => Toml::from(*b),
        Json::Number(n) => match n.as_i64() {
            Some(i) => Toml::from(i),
            None => Toml::from(n.as_f64()?),
        },
        Json::String(s) => Toml::from(s.as_str()),
        Json::Array(items) => {
            let mut arr = toml_edit::Array::new();
            for it in items {
                arr.push(json_to_toml_edit(it)?);
            }
            Toml::Array(arr)
        }
        Json::Object(map) => {
            let mut t = toml_edit::InlineTable::new();
            for (k, v) in map {
                t.insert(k, json_to_toml_edit(v)?);
            }
            Toml::InlineTable(t)
        }
    })
}

/// 备份文件路径：`xxx.toml` → `xxx.toml.bak`。
pub fn backup_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".bak");
    PathBuf::from(name)
}

/// 先复制为 `.bak` 再覆盖写入，返回备份路径。
pub fn backup_and_write(path: &Path, content: &str) -> io::Result<PathBuf> {
    let bak = backup_path(path);
    fs::copy(path, &bak)?;
    fs::write(path, content)?;
    Ok(bak)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_backfills_with_comments_and_keeps_values() {
        let mut doc: DocumentMut = "[server]\nport = 59090\n".parse().unwrap();
        let example: DocumentMut =
            "[server]\n# 端口\nport = 0\n# 监听地址\nbind = \"0.0.0.0\"\n".parse().unwrap();

        let added = merge_missing_keys(doc.as_table_mut(), example.as_table());
        assert_eq!(added, 1, "只应补齐 bind");

        let text = doc.to_string();
        assert!(text.contains("port = 59090"), "既有值不应被覆盖：{text}");
        assert!(text.contains("# 监听地址"), "注释应一并带入：{text}");
    }

    #[test]
    fn type_fix_rewrites_value_and_keeps_inline_comment() {
        let mut doc: DocumentMut = "[server]\nport = \"59090\" # 端口\n".parse().unwrap();
        let base = serde_json::json!({ "server": { "port": "59090" } });
        let coerced = serde_json::json!({ "server": { "port": 59090 } });

        let fixed = apply_type_fixes(&mut doc, &base, &coerced);
        assert_eq!(fixed, 1);

        let text = doc.to_string();
        assert!(text.contains("port = 59090"), "类型应被修正：{text}");
        assert!(text.contains("# 端口"), "行内注释应保留：{text}");
    }

    #[test]
    fn unknown_keys_reports_and_skips_free_form_sections() {
        let template = serde_json::json!({ "server": { "port": 1 }, "aliases": {} });
        let json = serde_json::json!({
            "server": { "port": 1, "heartbeat": "x" },
            "frames": {},
            "aliases": { "192.168.1.1": "台1" }
        });

        let list = unknown_keys(&json, &template, &["aliases"]);
        assert_eq!(list, vec!["frames".to_string(), "server.heartbeat".to_string()]);
    }

    #[test]
    fn backup_and_write_creates_bak_and_overwrites() {
        let dir = std::env::temp_dir().join(format!("dhrust-toml-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("config.toml");
        fs::write(&file, "old").unwrap();

        let bak = backup_and_write(&file, "new").unwrap();
        assert_eq!(bak, dir.join("config.toml.bak"));
        assert_eq!(fs::read_to_string(&bak).unwrap(), "old");
        assert_eq!(fs::read_to_string(&file).unwrap(), "new");

        let _ = fs::remove_dir_all(&dir);
    }
}
