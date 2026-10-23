//! TOML 配置文件类（`TomlConfig<T>`）—— `Config<T>` 的 TOML 版（`feature = "toml"`）。
//!
//! # 对应关系
//! - `Config<T>`（[`super::Config`]）：JSON 配置，语义对齐 C# `Config<TConfig>`；
//! - `TomlConfig<T>`（本类型）：**同样语义的 TOML 版**——TOML 可写注释，适合需要运维手工维护的配置；
//!   缺失自动生成、缺失键**带注释**补齐、类型容错、损坏备份重建、外部改动热加载。
//!
//! # 与 C# 的差异
//! C# 没有 TOML 配置类；这里的补齐/容错/原子写全部复用本库既有管线
//! （[`super::toml`] + [`super::coerce_json`] + [`super::save_if_changed`]）。
//!
//! # 示例
//! ```no_run
//! use dhrust::config::TomlConfig;
//!
//! const TEMPLATE: &str = "# 示例\nvalue = 7\n";
//! # #[derive(serde::Serialize, serde::Deserialize, Default)]
//! # struct Cfg { value: i64 }
//! let mut cfg = TomlConfig::<Cfg>::load("Config/Demo.toml", TEMPLATE);
//! for note in cfg.take_notes() {
//!     dhrust::info!("{note}");
//! }
//! cfg.value_mut().value = 9;
//! cfg.save().unwrap();
//! ```

use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

use super::file_config::{file_stamp, save_if_changed, ConfigOptions, FileStamp};
use super::setting::ConfigError;
use super::toml as dtoml;

/// TOML 配置文件（`Config<T>` 的 TOML 版）。
pub struct TomlConfig<T> {
    path: PathBuf,
    value: T,
    /// 配置模板（带注释；补齐缺失键与新建文件都以它为准）
    template: String,
    is_new: bool,
    notes: Vec<String>,
    stamp: Option<FileStamp>,
}

impl<T> TomlConfig<T>
where
    T: Serialize + DeserializeOwned + Default,
{
    /// 加载配置（默认选项：缺失生成、损坏备份重建）。
    pub fn load<P: AsRef<Path>>(path: P, template: &str) -> TomlConfig<T> {
        TomlConfig::load_with(path, template, ConfigOptions::default())
    }

    /// 按指定选项加载配置（**不会因读写/解析失败而 panic**：失败落默认值并写入 `notes`）。
    pub fn load_with<P: AsRef<Path>>(
        path: P,
        template: &str,
        options: ConfigOptions,
    ) -> TomlConfig<T> {
        let path = path.as_ref().to_path_buf();
        match Self::try_load_with(&path, template, options) {
            Ok(cfg) => cfg,
            Err(e) => {
                let mut cfg = TomlConfig::from_value(&path, template, T::default());
                cfg.notes.push(format!("配置加载失败（已用默认值）：{e}"));
                cfg
            }
        }
    }

    /// 加载配置；失败返回错误（需要区分「默认值」与「真的坏了」的调用方用这个）。
    pub fn try_load_with<P: AsRef<Path>>(
        path: P,
        template: &str,
        options: ConfigOptions,
    ) -> Result<TomlConfig<T>, ConfigError> {
        let path = path.as_ref().to_path_buf();
        let mut notes: Vec<String> = Vec::new();
        let template_text = template.to_string();

        let current = match std::fs::read_to_string(&path) {
            Ok(text) => Some(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(ConfigError::Io(e)),
        };

        // ① 文件不存在 → 按模板生成
        let Some(text) = current else {
            let value = value_of_template::<T>(&template_text, &mut notes)
                .map_err(ConfigError::Parse)?;
            if options.create_on_missing {
                save_if_changed(&path, |_| Ok(template_text.clone()))?;
                notes.push(format!("已生成默认配置：{}", path.display()));
            }
            return Ok(TomlConfig {
                path: path.clone(),
                value,
                template: template_text,
                is_new: true,
                notes,
                stamp: file_stamp(&path),
            });
        };

        // ② 已有文件：解析 + 补齐/容错
        match load_text::<T>(&template_text, &text, &mut notes) {
            Ok((value, dirty, rendered)) => {
                if dirty {
                    save_if_changed(&path, |_| Ok(rendered))?;
                    notes.push("已按模板补齐/修正并保存".to_string());
                }
                Ok(TomlConfig {
                    path: path.clone(),
                    value,
                    template: template_text,
                    is_new: false,
                    notes,
                    stamp: file_stamp(&path),
                })
            }
            // ③ 解析失败：备份 + 重建（对齐 C# `Config<T>` 的 repair 语义）
            Err(e) => {
                if !options.repair_corrupt {
                    return Err(ConfigError::Parse(e));
                }
                let bak = dtoml::backup_path(&path);
                let _ = std::fs::rename(&path, &bak);
                notes.push(format!(
                    "配置解析失败，已备份为 {} 并重建默认配置：{e}",
                    bak.display()
                ));
                let value = value_of_template::<T>(&template_text, &mut notes)
                    .map_err(ConfigError::Parse)?;
                save_if_changed(&path, |_| Ok(template_text.clone()))?;
                Ok(TomlConfig {
                    path: path.clone(),
                    value,
                    template: template_text,
                    is_new: true,
                    notes,
                    stamp: file_stamp(&path),
                })
            }
        }
    }

    /// 保存（渲染—比对—原子写；保留文件里已有的注释与排版）。
    pub fn save(&mut self) -> Result<(), ConfigError> {
        let template = self.template.clone();
        let want = serde_json::to_value(&self.value)
            .map_err(|e| ConfigError::Parse(format!("序列化配置失败：{e}")))?;
        save_if_changed(&self.path, |current| render(&template, &want, current))?;
        self.stamp = file_stamp(&self.path);
        Ok(())
    }

    /// 文件是否被外部修改（版本戳比较；文件被删除时保持当前值，返回 false）。
    pub fn is_stale(&self) -> bool {
        match file_stamp(&self.path) {
            Some(stamp) => self.stamp != Some(stamp),
            None => false,
        }
    }

    /// 文件被外部修改时热加载（解析失败保持当前值、不动原文件）。
    ///
    /// 返回提示文本（空 = 未发生变化）。
    pub fn reload_if_changed(&mut self) -> Vec<String> {
        let mut notes = Vec::new();
        if !self.is_stale() {
            return notes;
        }
        match std::fs::read_to_string(&self.path) {
            Ok(text) => match load_text::<T>(&self.template, &text, &mut notes) {
                Ok((value, dirty, rendered)) => {
                    self.value = value;
                    if dirty {
                        let _ = save_if_changed(&self.path, |_| Ok(rendered));
                        notes.push("已按模板补齐/修正并保存".to_string());
                    }
                    notes.push("检测到配置文件外部修改，已热加载".to_string());
                }
                Err(e) => notes.push(format!("配置解析失败，保持当前值（未改动原文件）：{e}")),
            },
            Err(e) => notes.push(format!("配置读取失败，保持当前值：{e}")),
        }
        self.stamp = file_stamp(&self.path);
        notes
    }

    /// 加载时文件是否不存在（或损坏后被重建）。
    pub fn is_new(&self) -> bool {
        self.is_new
    }
}

impl<T> TomlConfig<T> {
    /// 用已有值构造（不读盘；版本戳取自当前文件，供 [`TomlConfig::reload_if_changed`] 使用）。
    pub fn from_value<P: AsRef<Path>>(path: P, template: &str, value: T) -> TomlConfig<T> {
        let path = path.as_ref().to_path_buf();
        let stamp = file_stamp(&path);
        TomlConfig {
            path,
            value,
            template: template.to_string(),
            is_new: false,
            notes: Vec::new(),
            stamp,
        }
    }

    /// 当前值。
    pub fn value(&self) -> &T {
        &self.value
    }

    /// 当前值（可变；修改后需自行调用 [`TomlConfig::save`]）。
    pub fn value_mut(&mut self) -> &mut T {
        &mut self.value
    }

    /// 替换整个值（不落盘）。
    pub fn set(&mut self, value: T) {
        self.value = value;
    }

    /// 取走值。
    pub fn into_value(self) -> T {
        self.value
    }

    /// 加载过程中的提示（生成/补齐/备份等），供调用方写日志。
    pub fn notes(&self) -> &[String] {
        &self.notes
    }

    /// 取走提示。
    pub fn take_notes(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notes)
    }

    /// 配置文件路径。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 配置模板（带注释的原文）。
    pub fn template(&self) -> &str {
        &self.template
    }
}

/// 模板 → 值（结构体与模板不一致时落结构体默认值并记提示，不阻断启动）。
fn value_of_template<T>(template: &str, notes: &mut Vec<String>) -> Result<T, String>
where
    T: DeserializeOwned + Default,
{
    let doc = dtoml::parse_document(template)?;
    let json = dtoml::document_to_json(&doc)?;
    match serde_json::from_value::<T>(json) {
        Ok(v) => Ok(v),
        Err(e) => {
            notes.push(format!("配置模板与结构体不匹配（已用结构体默认值）：{e}"));
            Ok(T::default())
        }
    }
}

/// 解析文件文本：以模板补齐缺失键、修正类型，返回 `(值, 是否需要回写, 回写文本)`。
fn load_text<T>(template: &str, text: &str, notes: &mut Vec<String>) -> Result<(T, bool, String), String>
where
    T: DeserializeOwned,
{
    let tpl_doc = dtoml::parse_document(template)?;
    let tpl_json = dtoml::document_to_json(&tpl_doc)?;
    let mut doc = dtoml::parse_document(text)?;

    // 1) 新增字段：按模板补齐（带注释）
    let added = dtoml::merge_missing_keys(doc.as_table_mut(), tpl_doc.as_table());
    if added > 0 {
        notes.push(format!("已按模板补齐 {added} 个缺失配置项"));
    }
    // 2) 类型容错：以模板为类型基准（"32" → 32 等），并把修正值写回文档
    let file_json = dtoml::document_to_json(&doc)?;
    let (coerced, converted) = super::coerce_json(file_json.clone(), &tpl_json);
    let fixed = dtoml::apply_type_fixes(&mut doc, &file_json, &coerced);
    if converted || fixed > 0 {
        notes.push(format!("已修正 {fixed} 个类型不符的配置项"));
    }
    // 3) 未知键：保留并告警（不静默删除运维手写的扩展项）
    let unknown = dtoml::unknown_keys(&file_json, &tpl_json, &[]);
    if !unknown.is_empty() {
        notes.push(format!("存在未识别配置项（已保留）：{}", unknown.join(", ")));
    }

    let value: T = serde_json::from_value(coerced).map_err(|e| e.to_string())?;
    Ok((value, added > 0 || converted || fixed > 0, doc.to_string()))
}

/// 以模板为基准渲染（保留 `current` 里已有的注释与排版）。
fn render(template: &str, want: &Value, current: Option<&str>) -> Result<String, String> {
    let tpl_doc = dtoml::parse_document(template)?;
    let tpl_json = dtoml::document_to_json(&tpl_doc)?;
    let (expected, _) = super::coerce_json(want.clone(), &tpl_json);

    let mut doc = match current {
        Some(text) if !text.trim().is_empty() => dtoml::parse_document(text)?,
        _ => tpl_doc.clone(),
    };
    dtoml::merge_missing_keys(doc.as_table_mut(), tpl_doc.as_table());
    let base = dtoml::document_to_json(&doc)?;
    // `apply_type_fixes` 会写入 base→expected 之间**所有变化**的叶子值（沿用原值装饰/行内注释）
    dtoml::apply_type_fixes(&mut doc, &base, &expected);
    Ok(doc.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TPL: &str = "# 示例配置\n# 数值项\nvalue = 7\nname = \"\"\n";

    #[derive(Debug, Default, PartialEq, Serialize, serde::Deserialize)]
    #[serde(default, rename_all = "camelCase")]
    struct Demo {
        value: i64,
        name: String,
    }

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dhrust-tomlcfg-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建目录");
        dir
    }

    #[test]
    fn missing_file_is_created_from_template() {
        let dir = temp("new");
        let file = dir.join("a.toml");
        let mut cfg = TomlConfig::<Demo>::load(&file, TPL);
        assert!(cfg.is_new());
        assert_eq!(cfg.value(), &Demo { value: 7, name: String::new() });
        let text = std::fs::read_to_string(&file).expect("已生成");
        assert!(text.contains("# 示例配置"), "模板注释应保留：{text}");
        assert!(cfg.take_notes().iter().any(|n| n.contains("生成默认配置")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_keys_and_bad_types_are_repaired_in_place() {
        let dir = temp("repair");
        let file = dir.join("a.toml");
        std::fs::write(&file, "# 手工写的说明\nvalue = \"9\"\n").expect("写");
        let mut cfg = TomlConfig::<Demo>::load(&file, TPL);
        assert_eq!(cfg.value().value, 9);
        let text = std::fs::read_to_string(&file).expect("读");
        assert!(text.contains("# 手工写的说明"), "人工注释不能丢：{text}");
        assert!(text.contains("name = \"\""), "缺失键应补齐：{text}");
        assert!(!text.contains("\"9\""), "类型不符应修正：{text}");
        assert!(cfg.take_notes().iter().any(|n| n.contains("类型不符")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_keeps_comments_and_updates_values() {
        let dir = temp("save");
        let file = dir.join("a.toml");
        std::fs::write(&file, "# 手写注释\nvalue = 1\nname = \"old\"\n").expect("写");
        let mut cfg = TomlConfig::<Demo>::load(&file, TPL);
        cfg.value_mut().value = 5;
        cfg.value_mut().name = "new".into();
        cfg.save().expect("保存");
        let text = std::fs::read_to_string(&file).expect("读");
        assert!(text.contains("# 手写注释"));
        assert!(text.contains("value = 5"));
        assert!(text.contains("name = \"new\""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_file_is_backed_up_and_rebuilt() {
        let dir = temp("corrupt");
        let file = dir.join("a.toml");
        std::fs::write(&file, "value = = broken").expect("写坏文件");
        let mut cfg = TomlConfig::<Demo>::load(&file, TPL);
        assert_eq!(cfg.value().value, 7);
        assert!(cfg.take_notes().iter().any(|n| n.contains("已备份")));
        assert!(dtoml::backup_path(&file).is_file(), "坏文件应留 .bak");
        assert!(file.is_file(), "应重建新配置");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reload_picks_up_external_changes() {
        let dir = temp("reload");
        let file = dir.join("a.toml");
        let mut cfg = TomlConfig::<Demo>::load(&file, TPL);
        // 保证 mtime 变化（部分文件系统秒级精度）
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&file, "value = 42\nname = \"x\"\n").expect("外部修改");
        let notes = cfg.reload_if_changed();
        assert!(notes.iter().any(|n| n.contains("热加载")), "{notes:?}");
        assert_eq!(cfg.value().value, 42);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
