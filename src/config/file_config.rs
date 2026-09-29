//! 通用配置文件，对应 C# `NewLife.Configuration.Config<TConfig>`。
//!
//! 语义对齐 DH.NCore `Configuration/Config.cs` 的 `Current` 访问逻辑：
//! - 文件不存在 → 按 `Runtime.CreateConfigOnMissing`（默认 true，可用环境变量
//!   `CreateConfigOnMissing` 关闭）生成默认配置文件；
//! - 文件存在 → 绑定到默认实例后**缺失属性自动补齐并回存**（对齐 `config.Save()`）；
//! - 文件损坏 → 备份为 `.bak` 后重建默认配置（对齐 C# 忽略错误继续运行的语义）；
//! - 保存时内容相同跳过、先写临时文件再原子替换（对齐 `FileConfigProvider.OnWrite`）。
//!
//! JSON 格式（对应 `JsonConfigProvider`）：读取容忍注释与 BOM，写出为缩进 JSON。
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
                Ok(text) => match parse_value(&text).and_then(|v| {
                    serde_json::from_value::<T>(v.clone())
                        .map(|model| (model, v))
                        .map_err(|e| ConfigError::Parse(e.to_string()))
                }) {
                    Ok((model, file_value)) => {
                        // 文件属性完整？不完整则回存补齐（对齐 C#：`config.Save()`）
                        let canonical = serde_json::to_value(&model).unwrap_or(Value::Null);
                        if !covers(&canonical, &file_value) {
                            let mut cfg = Config {
                                path: path.clone(),
                                value: model,
                                is_new: false,
                                notes,
                                stamp: None,
                            };
                            if cfg.save().is_ok() {
                                cfg.notes.push("已补齐缺失属性并保存".to_string());
                            }
                            return cfg;
                        }
                        loaded = Some(model);
                    }
                    Err(e) => {
                        if options.repair_corrupt {
                            notes.push(format!("配置解析失败，已备份为 .bak 并重建默认配置: {e}"));
                            let _ = std::fs::rename(&path, backup_path(&path));
                        } else {
                            notes.push(format!("配置解析失败（未改动原文件）: {e}"));
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
            Ok(text) => match parse_value(&text).and_then(|v| {
                serde_json::from_value::<T>(v.clone())
                    .map(|m| (m, v))
                    .map_err(|e| ConfigError::Parse(e.to_string()))
            }) {
                Ok((model, file_value)) => {
                    let canonical = serde_json::to_value(&model).unwrap_or(Value::Null);
                    self.value = model;
                    if !covers(&canonical, &file_value) {
                        if let Ok(()) = save_value(&self.path, &self.value) {
                            notes.push("已补齐缺失属性并保存".to_string());
                        }
                    }
                    notes.push("检测到配置文件外部修改，已热加载".to_string());
                }
                Err(e) => {
                    notes.push(format!("配置解析失败，保持当前值（未改动原文件）: {e}"));
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
        items: Vec<DemoItem>,
    }

    impl Default for Demo {
        fn default() -> Self {
            Self {
                name: "默认名称".to_string(),
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
        let text = "{\n  \"Name\": \"N\",\n  \"Items\": [{ \"Url\": \"u\", \"Enabled\": true }]\n}";
        std::fs::write(&path, text).unwrap();

        let cfg = Config::<Demo>::load(&path);
        assert!(cfg.notes().is_empty());
        assert_eq!(cfg.value().items[0].enabled, true);
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
    fn comments_and_extra_keys_tolerated() {
        let path = temp_path("comment");
        // 整行注释 + 额外未知键：可解析、无缺失属性，则原样保留
        let text = "{\n// 注释\n\"Name\":\"N\",\"Extra\":123,\"Items\":[{\"Url\":\"u\",\"Enabled\":true}]\n}";
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
}
