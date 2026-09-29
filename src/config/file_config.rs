//! 通用配置文件，对应 C# `NewLife.Configuration.Config<TConfig>`。
//!
//! 语义对齐 DH.NCore `Configuration/Config.cs` 的 `Current` 访问逻辑：
//! - 文件不存在 → 按 `Runtime.CreateConfigOnMissing`（默认 true，可用环境变量
//!   `CreateConfigOnMissing` 关闭）生成默认配置文件；
//! - 文件存在 → 绑定到默认实例后**缺失属性自动补齐并回存**（对齐 `config.Save()`）；
//! - 文件损坏 → 备份为 `.bak` 后重建默认配置（对齐 C# 忽略错误继续运行的语义）；
//! - 保存时内容相同跳过、先写临时文件再原子替换（对齐 `FileConfigProvider.OnWrite`）。
//!
//! JSON 格式（对应 `JsonConfigProvider`）：读取容忍注释与 BOM，写出为缩进 JSON；
//! 类型不符的字段按 C# 绑定语义做容错转换（万能转换），并回写自愈。
//!
//! # 示例
//!
//! ```no_run
//! use dhrust::config::Config;
//! use serde::{Deserialize, Serialize};
//!
//! #[derive(Default, Serialize, Deserialize)]
//! #[serde(default, rename_all = "PascalCase")]
//! struct SiteConfig {
//!     sites: Vec<String>,
//! }
//!
//! // 文件不存在自动生成、缺属性自动补齐回存
//! let mut cfg = Config::<SiteConfig>::load("Settings/CheckSite.json");
//! for note in cfg.take_notes() {
//!     println!("[Config] {note}");
//! }
//! let sites = cfg.value().sites.clone();
//! ```

use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

use super::ConfigError;

/// 配置加载选项（对应 `Runtime.CreateConfigOnMissing`）。
#[derive(Clone, Copy, Debug)]
pub struct ConfigOptions {
    /// 文件不存在时是否生成默认配置文件。默认 true；环境变量 `CreateConfigOnMissing`
    /// 取值 `0/false/off/no` 时关闭（与 C# `Runtime.CreateConfigOnMissing` 一致）。
    pub create_on_missing: bool,
    /// 解析失败时是否备份（`.bak`）并重建默认配置。默认 true（对齐 C#）；
    /// 关闭时保持原文件不动、仅返回默认值（用于运行期热加载，避免误改用户正在编辑的文件）。
    pub repair_corrupt: bool,
}

impl Default for ConfigOptions {
    fn default() -> Self {
        let enabled = std::env::var("CreateConfigOnMissing")
            .ok()
            .map(|v| {
                !matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "0" | "false" | "off" | "no"
                )
            })
            .unwrap_or(true);
        Self {
            create_on_missing: enabled,
            repair_corrupt: true,
        }
    }
}

/// 通用配置文件（`Config<T>`）。
///
/// 一次 [`Config::load`] 即完成 C# `Config<T>.Current` 的全部行为：
/// 生成缺失文件、补齐缺失属性并回存、损坏备份重建；
/// 常驻实例可配合 [`Config::reload_if_changed`] 热加载（对齐 C# 文件监视器语义）。
pub struct Config<T> {
    path: PathBuf,
    value: T,
    is_new: bool,
    notes: Vec<String>,
    /// 文件版本戳（热加载检测；load/save 后刷新）。
    stamp: Option<FileStamp>,
}

impl<T> Config<T>
where
    T: Serialize + DeserializeOwned + Default,
{
    /// 加载配置（默认选项）。
    pub fn load<P: AsRef<Path>>(path: P) -> Config<T> {
        Config::load_with(path, ConfigOptions::default())
    }

    /// 按指定选项加载配置。
    pub fn load_with<P: AsRef<Path>>(path: P, options: ConfigOptions) -> Config<T> {
        let path = path.as_ref().to_path_buf();
        let mut notes = Vec::new();

        let existed = path.exists();
        let mut loaded: Option<T> = None;

        if existed {
            match std::fs::read_to_string(&path) {
                Ok(text) => match parse_and_coerce::<T>(&text) {
                    Ok((model, file_value, converted)) => {
                        // 类型不符已转换 / 缺属性 → 回存规范化（对齐 C# `Current` 的 `Save()`）
                        let canonical = serde_json::to_value(&model).unwrap_or(Value::Null);
                        if converted || !covers(&canonical, &file_value) {
                            let mut cfg = Config {
                                path: path.clone(),
                                value: model,
                                is_new: false,
                                notes,
                                stamp: None,
                            };
                            if cfg.save().is_ok() {
                                cfg.notes.push(if converted {
                                    "已修正类型不符的属性并保存".to_string()
                                } else {
                                    "已补齐缺失属性并保存".to_string()
                                });
                            }
                            return cfg;
                        }
                        loaded = Some(model);
                    }
                    Err(e) => {
                        if options.repair_corrupt {
                            notes.push(format!(
                                "配置解析失败，已备份为 .bak 并重建默认配置: {}",
                                parse_error_detail(&e)
                            ));
                            let _ = std::fs::rename(&path, backup_path(&path));
                        } else {
                            notes.push(format!(
                                "配置解析失败（未改动原文件）: {}",
                                parse_error_detail(&e)
                            ));
                        }
                    }
                },
                Err(e) => notes.push(format!("配置读取失败，使用默认值: {e}")),
            }
        }

        match loaded {
            Some(value) => {
                let stamp = file_stamp(&path);
                Config {
                    path,
                    value,
                    is_new: false,
                    notes,
                    stamp,
                }
            }
            None => {
                let is_new = true;
                let value = T::default();
                // 文件不存在 → 由 create_on_missing 决定是否生成默认文件；
                // 文件存在但解析/读取失败 → 由 repair_corrupt 决定是否重建（关闭时保持原文件不动）。
                let may_write = if existed {
                    options.repair_corrupt
                } else {
                    options.create_on_missing
                };
                if may_write {
                    if let Ok(()) = save_value(&path, &value) {
                        notes.push(if existed {
                            "已重建默认配置".to_string()
                        } else {
                            "已生成默认配置".to_string()
                        });
                    }
                } else if !existed {
                    notes.push("文件不存在（CreateConfigOnMissing=false，跳过生成）".to_string());
                }
                let stamp = file_stamp(&path);
                Config {
                    path,
                    value,
                    is_new,
                    notes,
                    stamp,
                }
            }
        }
    }

    /// 修改并保存。
    pub fn update<F: FnOnce(&mut T)>(&mut self, f: F) -> Result<(), ConfigError> {
        f(&mut self.value);
        self.save()
    }

    /// 保存当前值到文件（内容相同跳过；原子替换），并刷新文件版本戳。
    pub fn save(&mut self) -> Result<(), ConfigError> {
        let r = save_value(&self.path, &self.value);
        self.stamp = file_stamp(&self.path);
        r
    }

    /// 用已有值构造（不读盘；版本戳取自当前文件，供 [`Config::reload_if_changed`] 检测外部修改）。
    pub fn from_value<P: AsRef<Path>>(path: P, value: T) -> Config<T> {
        let path = path.as_ref().to_path_buf();
        let stamp = file_stamp(&path);
        Config {
            path,
            value,
            is_new: false,
            notes: Vec::new(),
            stamp,
        }
    }

    /// 文件是否被外部修改（版本戳比较；文件被删除时保持当前值，返回 false）。
    pub fn is_stale(&self) -> bool {
        match file_stamp(&self.path) {
            Some(stamp) => self.stamp != Some(stamp),
            None => false,
        }
    }

    /// 文件被外部修改时热加载（对齐 C# `FileConfigProvider` 监视器重载）：
    /// - 解析失败 → 保持当前值、不动原文件（对齐 C# `DoRefresh` 捕获异常不改 Root）；
    /// - 加载成功 → 替换当前值，缺失属性回写保存（如有）。
    ///
    /// 返回提示文本（空 = 未发生变化），供调用方写日志。
    pub fn reload_if_changed(&mut self) -> Vec<String> {
        let mut notes = Vec::new();
        if !self.is_stale() {
            return notes;
        }
        match std::fs::read_to_string(&self.path) {
            Ok(text) => match parse_and_coerce::<T>(&text) {
                Ok((model, file_value, converted)) => {
                    let canonical = serde_json::to_value(&model).unwrap_or(Value::Null);
                    self.value = model;
                    if converted || !covers(&canonical, &file_value) {
                        if let Ok(()) = save_value(&self.path, &self.value) {
                            notes.push(if converted {
                                "已修正类型不符的属性并保存".to_string()
                            } else {
                                "已补齐缺失属性并保存".to_string()
                            });
                        }
                    }
                    notes.push("检测到配置文件外部修改，已热加载".to_string());
                }
                Err(e) => {
                    notes.push(format!(
                        "配置解析失败，保持当前值（未改动原文件）: {}",
                        parse_error_detail(&e)
                    ));
                }
            },
            Err(e) => notes.push(format!("配置读取失败，保持当前值: {e}")),
        }
        self.stamp = file_stamp(&self.path);
        notes
    }

    /// 加载时文件是否不存在（对应 C# `IsNew`）。
    pub fn is_new(&self) -> bool {
        self.is_new
    }
}

impl<T> Config<T> {
    /// 当前值。
    pub fn value(&self) -> &T {
        &self.value
    }

    /// 当前值（可变；修改后需自行调用 [`Config::save`]）。
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
}

/// 保存任意配置模型到 JSON 文件（缩进输出；内容相同跳过；先写临时文件再原子替换）。
pub fn save_value<T: Serialize, P: AsRef<Path>>(path: P, value: &T) -> Result<(), ConfigError> {
    let text =
        serde_json::to_string_pretty(value).map_err(|e| ConfigError::Parse(e.to_string()))?;
    super::setting::atomic_write(path.as_ref(), &text)
}

/// 文件版本戳（长度 + 修改时间），用于检测配置文件是否被外部修改。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FileStamp {
    /// 文件长度（字节）
    pub len: u64,
    /// 最后修改时间
    pub modified: Option<std::time::SystemTime>,
}

/// 获取文件版本戳（文件不存在时返回 None）。
pub fn file_stamp<P: AsRef<Path>>(path: P) -> Option<FileStamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some(FileStamp {
        len: meta.len(),
        modified: meta.modified().ok(),
    })
}

/// 解析 JSON 文本（去 BOM、清理注释，对齐 C# `JsonConfigProvider.OnRead`）。
fn parse_value(text: &str) -> Result<Value, ConfigError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let cleaned = super::json::trim_comment(text);
    serde_json::from_str(&cleaned).map_err(|e| ConfigError::Parse(e.to_string()))
}

/// 解析 + 字段级容错转换 + 反序列化。
///
/// 按 [`Default`] 序列化出的“类型模板”对文件值做尽力转换（对齐 C# 绑定的“万能转换”语义：
/// 单个字段类型不符不影响其余字段）：字符串/数字/布尔互转；不可解析的值回落该类型零值。
/// 返回（模型, 转换后的文件值, 是否发生过转换）。
fn parse_and_coerce<T>(text: &str) -> Result<(T, Value, bool), ConfigError>
where
    T: Serialize + DeserializeOwned + Default,
{
    let file_value = parse_value(text)?;
    let template = serde_json::to_value(T::default()).unwrap_or(Value::Null);
    let (coerced, converted) = coerce_value(file_value, &template);
    let model = serde_json::from_value::<T>(coerced.clone())
        .map_err(|e| ConfigError::Parse(e.to_string()))?;
    Ok((model, coerced, converted))
}

/// 按类型模板对配置值做字段级容错转换（对应 C# 配置绑定的“万能转换”语义）。
///
/// 供 JSON 之外的配置格式复用（如 TOML 配置：先转为 JSON 值再调用）：
/// - `template` 通常由配置模型的默认值序列化得到：`serde_json::to_value(T::default())`；
/// - 对象逐键、数组逐项；标量按目标类型转换（字符串/数字/布尔互转，`true/1/yes/y/on` 判真）；
/// - 不可解析的值回落该类型零值；模板为 `Null` 的字段保留原值。
///
/// 返回（转换后的值, 是否发生过转换）。
pub fn coerce_json(file: Value, template: &Value) -> (Value, bool) {
    coerce_value(file, template)
}

/// 按类型模板做字段级容错转换（对象逐键、数组逐项；标量按目标类型转换，失败回落零值）。
///
/// 限制：模板为 `Null` 的字段（如 `Option<T>` 且默认值为 `None`）无法判定目标类型，
/// 保留原值（可能仍导致反序列化失败）。
fn coerce_value(file: Value, template: &Value) -> (Value, bool) {
    match (file, template) {
        (Value::Object(mut file_obj), Value::Object(tmpl_obj)) => {
            let mut changed = false;
            for (key, value) in file_obj.iter_mut() {
                if let Some(tv) = tmpl_obj.get(key) {
                    let (nv, ch) = coerce_value(value.take(), tv);
                    *value = nv;
                    changed |= ch;
                }
            }
            (Value::Object(file_obj), changed)
        }
        (Value::Array(mut file_arr), Value::Array(tmpl_arr)) => {
            let mut changed = false;
            if let Some(first) = tmpl_arr.first() {
                for value in file_arr.iter_mut() {
                    let (nv, ch) = coerce_value(value.take(), first);
                    *value = nv;
                    changed |= ch;
                }
            }
            (Value::Array(file_arr), changed)
        }
        (file, Value::String(_)) => match file {
            file @ Value::String(_) => (file, false),
            Value::Number(n) => (Value::String(n.to_string()), true),
            Value::Bool(b) => (Value::String(b.to_string()), true),
            _ => (template.clone(), true),
        },
        (file, Value::Bool(_)) => match file {
            file @ Value::Bool(_) => (file, false),
            Value::Number(n) => (
                Value::Bool(n.as_f64().map(|x| x != 0.0).unwrap_or(false)),
                true,
            ),
            // 对齐 C# `ToBoolean` 容错：true/1/yes/y/on 为真，其余为假
            Value::String(s) => {
                let low = s.trim().to_ascii_lowercase();
                (
                    Value::Bool(matches!(low.as_str(), "true" | "1" | "yes" | "y" | "on")),
                    true,
                )
            }
            _ => (template.clone(), true),
        },
        (file, Value::Number(tmpl_num)) => match file {
            file @ Value::Number(_) => (file, false),
            Value::Bool(b) => (number_like(tmpl_num, if b { 1 } else { 0 }), true),
            Value::String(s) => {
                let s = s.trim();
                let converted = if tmpl_num.is_i64() || tmpl_num.is_u64() {
                    s.parse::<i64>()
                        .ok()
                        .or_else(|| s.parse::<f64>().ok().map(|f| f.round() as i64))
                        .map(|i| Value::Number(i.into()))
                } else {
                    s.parse::<f64>()
                        .ok()
                        .and_then(serde_json::Number::from_f64)
                        .map(Value::Number)
                };
                // 不可解析 → 零值（对齐 C# `ToInt/ToDouble` 的容错回落）
                (converted.unwrap_or_else(|| number_like(tmpl_num, 0)), true)
            }
            _ => (template.clone(), true),
        },
        (file, _) => {
            // 模板为 Null/其它（类型无法判定）：结构不匹配时回落模板，否则保留原值
            let mismatch = matches!(file, Value::Object(_) | Value::Array(_));
            if mismatch && !template.is_null() {
                (template.clone(), true)
            } else {
                (file, false)
            }
        }
    }
}

/// 生成与模板同“整型/浮点”形态的数字值。
fn number_like(template: &serde_json::Number, value: i64) -> Value {
    if template.is_i64() || template.is_u64() {
        Value::Number(value.into())
    } else {
        Value::Number(
            serde_json::Number::from_f64(value as f64)
                .unwrap_or_else(|| serde_json::Number::from(0)),
        )
    }
}

/// 规范化模型是否覆盖文件内容：模型中的每个键都应在文件中存在（递归；数组逐项对应）。
///
/// 用于判断文件是否缺失属性：模型由文件反序列化而来（缺失字段取默认值），
/// 若模型包含文件没有的键，说明文件存在缺失属性，需回存补齐。
fn covers(canonical: &Value, file: &Value) -> bool {
    match (canonical, file) {
        (Value::Object(c), Value::Object(f)) => c
            .iter()
            .all(|(key, value)| f.get(key).is_some_and(|fv| covers(value, fv))),
        (Value::Array(c), Value::Array(f)) => {
            c.len() == f.len() && c.iter().zip(f.iter()).all(|(a, b)| covers(a, b))
        }
        // 标量：类型已在反序列化阶段校验，只要求存在（对象键存在性在上层判断）
        _ => true,
    }
}

/// 解析失败提示的细节文本（`ConfigError::Display` 已带“配置解析失败: ”前缀，此处去重）。
fn parse_error_detail(e: &ConfigError) -> String {
    let text = e.to_string();
    text.strip_prefix("配置解析失败: ")
        .unwrap_or(&text)
        .to_string()
}

/// 备份文件名：`Xxx.json` → `Xxx.json.bak`。
fn backup_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".bak");
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Default, Serialize, Deserialize, PartialEq, Debug)]
    #[serde(default, rename_all = "PascalCase")]
    struct DemoItem {
        url: String,
        enabled: bool,
    }

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    #[serde(default, rename_all = "PascalCase")]
    struct Demo {
        name: String,
        count: i64,
        items: Vec<DemoItem>,
    }

    impl Default for Demo {
        fn default() -> Self {
            Self {
                name: "默认名称".to_string(),
                count: 0,
                items: vec![DemoItem {
                    url: "http://sample/health".to_string(),
                    enabled: false,
                }],
            }
        }
    }

    fn temp_path(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dhrust-cfg-{tag}-{}", rand_suffix()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("Demo.json")
    }

    fn rand_suffix() -> String {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        (0..8)
            .map(|_| char::from(b'a' + rng.gen_range(0..26)))
            .collect()
    }

    #[test]
    fn missing_file_creates_default() {
        let path = temp_path("create");
        let cfg = Config::<Demo>::load(&path);
        assert!(cfg.is_new());
        assert!(path.exists(), "缺失文件应自动生成");
        assert_eq!(cfg.value().name, "默认名称");
        assert!(cfg.notes().iter().any(|n| n.contains("已生成默认配置")));

        // 生成内容可再次加载且不再补齐
        let cfg2 = Config::<Demo>::load(&path);
        assert!(!cfg2.is_new());
        assert!(cfg2.notes().is_empty(), "完整文件不应产生补齐提示");
    }

    #[test]
    fn missing_properties_backfilled_and_saved() {
        let path = temp_path("backfill");
        std::fs::write(
            &path,
            r#"{"Name":"自定义","Items":[{"Url":"https://x/health"}]}"#,
        )
        .unwrap();

        let cfg = Config::<Demo>::load(&path);
        assert!(cfg.notes().iter().any(|n| n.contains("已补齐缺失属性")));
        assert_eq!(cfg.value().name, "自定义");
        assert_eq!(cfg.value().items.len(), 1);
        assert_eq!(cfg.value().items[0].url, "https://x/health");
        assert!(!cfg.value().items[0].enabled);

        let saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["Name"], "自定义");
        assert_eq!(saved["Items"][0]["Url"], "https://x/health");
        assert!(
            saved["Items"][0].get("Enabled").is_some(),
            "缺属性应补齐保存"
        );

        // 再次加载：文件已完整，不再改写
        let text = std::fs::read_to_string(&path).unwrap();
        let cfg2 = Config::<Demo>::load(&path);
        assert!(cfg2.notes().is_empty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    }

    #[test]
    fn complete_file_untouched() {
        let path = temp_path("complete");
        let text =
            "{\n  \"Name\": \"N\",\n  \"Count\": 3,\n  \"Items\": [{ \"Url\": \"u\", \"Enabled\": true }]\n}";
        std::fs::write(&path, text).unwrap();

        let cfg = Config::<Demo>::load(&path);
        assert!(cfg.notes().is_empty());
        assert!(cfg.value().items[0].enabled);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            text,
            "完整文件不应被重写"
        );
    }

    #[test]
    fn corrupt_file_backed_up_and_rebuilt() {
        let path = temp_path("corrupt");
        std::fs::write(&path, "{ 坏文件").unwrap();

        let cfg = Config::<Demo>::load(&path);
        assert!(cfg.notes().iter().any(|n| n.contains("已备份")));
        assert!(backup_path(&path).exists(), "损坏文件应备份为 .bak");
        let rebuilt: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(rebuilt["Name"], "默认名称");
    }

    #[test]
    fn create_on_missing_disabled() {
        let path = temp_path("nomissing");
        let cfg = Config::<Demo>::load_with(
            &path,
            ConfigOptions {
                create_on_missing: false,
                ..Default::default()
            },
        );
        assert!(!path.exists(), "关闭生成开关时不应写盘");
        assert_eq!(cfg.value().name, "默认名称");
        assert!(cfg
            .notes()
            .iter()
            .any(|n| n.contains("CreateConfigOnMissing")));
    }

    #[test]
    fn repair_corrupt_disabled_keeps_file() {
        let path = temp_path("norepair");
        std::fs::write(&path, "{ 坏文件").unwrap();

        let cfg = Config::<Demo>::load_with(
            &path,
            ConfigOptions {
                create_on_missing: true,
                repair_corrupt: false,
            },
        );
        assert!(cfg.notes().iter().any(|n| n.contains("未改动原文件")));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{ 坏文件",
            "关闭修复时不应改动原文件"
        );
        assert!(!backup_path(&path).exists(), "关闭修复时不应产生 .bak");
    }

    #[test]
    fn reload_if_changed_hot_reload() {
        let path = temp_path("reload");
        let mut cfg = Config::<Demo>::load(&path);
        assert!(!cfg.is_stale());

        // 外部修改 → 热加载
        std::fs::write(
            &path,
            r#"{"Name":"外部","Items":[{"Url":"u","Enabled":true}]}"#,
        )
        .unwrap();
        assert!(cfg.is_stale());
        let notes = cfg.reload_if_changed();
        assert!(notes.iter().any(|n| n.contains("已热加载")));
        assert_eq!(cfg.value().name, "外部");
        assert!(!cfg.is_stale());

        // 自身 save 后不应被当作“外部修改”
        cfg.value_mut().name = "自改".into();
        cfg.save().unwrap();
        assert!(!cfg.is_stale());
        assert!(cfg.reload_if_changed().is_empty());

        // 损坏的外部修改：保持当前值、不动原文件、不产生 .bak
        std::fs::write(&path, "{ 坏文件").unwrap();
        let notes = cfg.reload_if_changed();
        assert!(notes.iter().any(|n| n.contains("保持当前值")));
        assert_eq!(cfg.value().name, "自改");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ 坏文件");
        assert!(!backup_path(&path).exists());
        assert!(!cfg.is_stale(), "失败后戳应同步，避免重复告警");
    }

    #[test]
    fn from_value_tracks_external_change() {
        let path = temp_path("fromval");
        let mut cfg = Config::<Demo>::from_value(
            &path,
            Demo {
                name: "初始".to_string(),
                count: 0,
                items: vec![],
            },
        );
        assert!(!path.exists());
        assert!(!cfg.is_stale(), "文件不存在不算外部修改");

        std::fs::write(&path, r#"{"Name":"后来"}"#).unwrap();
        assert!(cfg.is_stale());
        let notes = cfg.reload_if_changed();
        assert!(notes.iter().any(|n| n.contains("已热加载")));
        assert_eq!(cfg.value().name, "后来");
    }

    #[test]
    fn lenient_type_coercion_and_self_heal() {
        let path = temp_path("coerce");
        // 字符串布尔 / 数字字符串 / 数字变字符串：全部字段级容错（对齐 C# 万能转换）
        std::fs::write(
            &path,
            r#"{"Name":123,"Count":"7","Items":[{"Url":"u","Enabled":"true"}]}"#,
        )
        .unwrap();

        let cfg = Config::<Demo>::load(&path);
        assert_eq!(cfg.value().name, "123");
        assert_eq!(cfg.value().count, 7);
        assert!(cfg.value().items[0].enabled);
        assert!(cfg.notes().iter().any(|n| n.contains("类型")));

        // 自愈：文件已回写为规范类型
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["Name"], "123");
        assert_eq!(saved["Count"], 7);
        assert!(saved["Items"][0]["Enabled"].is_boolean());

        // 不可解析的数字 → 零值（对齐 C# ToInt/ToDouble 容错回落）
        std::fs::write(&path, r#"{"Name":"n","Count":"abc","Items":[]}"#).unwrap();
        let cfg = Config::<Demo>::load(&path);
        assert_eq!(cfg.value().count, 0);
        assert!(cfg.notes().iter().any(|n| n.contains("类型")));
    }

    #[test]
    fn comments_and_extra_keys_tolerated() {
        let path = temp_path("comment");
        // 整行注释 + 额外未知键：可解析、无缺失属性，则原样保留
        let text = "{\n// 注释\n\"Name\":\"N\",\"Count\":5,\"Extra\":123,\"Items\":[{\"Url\":\"u\",\"Enabled\":true}]\n}";
        std::fs::write(&path, text).unwrap();

        let cfg = Config::<Demo>::load(&path);
        assert_eq!(cfg.value().name, "N");
        assert!(cfg.notes().is_empty(), "无缺失属性不应改写");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            text,
            "注释与未知键应保留"
        );
    }

    #[test]
    fn coerce_json_public_contract() {
        // 外部格式（如 TOML）复用的公开入口：按默认值模板做字段级容错
        let template = serde_json::json!({
            "Port": 9100,
            "Enable": true,
            "Name": "x"
        });
        let (coerced, changed) = coerce_json(
            serde_json::json!({ "Port": "9100", "Enable": 1, "Name": 42 }),
            &template,
        );
        assert!(changed);
        assert_eq!(coerced["Port"], 9100);
        assert_eq!(coerced["Enable"], true);
        assert_eq!(coerced["Name"], "42");

        // 类型已正确时不标记转换
        let (_, changed) = coerce_json(serde_json::json!({ "Port": 1 }), &template);
        assert!(!changed);
    }
}
