//! 核心设置，对应 DH.NCore `NewLife.Setting`。
//!
//! 字段与 C# 属性一一对应（JSON/XML 均使用 PascalCase 名称），默认配置文件为
//! `Config/Core.config`（XML，C# 默认 provider），也支持 `Config/Core.json`。
//!
//! 与 C# 的差异：
//! - C# 中 `Config<T>.Current` 在配置文件缺失时会按 `Runtime.CreateConfigOnMissing`
//!   自动落盘；Rust 的 [`Setting::load`] 只读不写，需要落盘请显式调用 [`Setting::save`]。
//! - C# `Setting` 含 .NET 专属的 `AssemblyResolve` 属性（程序集解析），与 Redis 数据
//!   无关，Rust 结构体中不保留；写入的配置文件不含该键，C# 读取时自动保持默认值。

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::xml;

/// 默认配置文件（XML，与 C# 默认 provider 一致）。
pub const DEFAULT_CONFIG_FILE: &str = "Config/Core.config";

/// 备用 JSON 配置文件。
pub const DEFAULT_CONFIG_FILE_JSON: &str = "Config/Core.json";

/// 配置字段及其注释。顺序与 C# `Setting` 属性声明顺序一致，
/// 注释文本与 C# `[Description]` 特性一致，保证生成文件与 C# 原生文件风格相同。
const FIELD_COMMENTS: &[(&str, &str)] = &[
    ("Debug", "启用全局调试。XTrace.Debug"),
    (
        "LogLevel",
        "日志等级。只输出大于等于该级别的日志，All/Debug/Info/Warn/Error/Fatal，默认Info",
    ),
    ("LogPath", "文件日志目录。默认Log子目录"),
    (
        "LogFileMaxBytes",
        "日志文件上限（MB）。超过上限后拆分新日志文件，默认10MB，0表示不限制大小",
    ),
    (
        "LogFileBackups",
        "日志文件备份。超过备份数后，最旧的文件将被删除，网络安全法要求至少保存6个月日志，默认200，0表示不限制个数",
    ),
    (
        "LogFileFormat",
        "日志文件格式。默认{0:yyyy_MM_dd}.log，支持日志等级如 {1}_{0:yyyy_MM_dd}.log",
    ),
    (
        "LogLineFormat",
        "日志行格式。默认Time|ThreadId|Kind|Name|Message，还支持Level",
    ),
    (
        "NetworkLog",
        "网络日志。本地子网日志广播udp://255.255.255.255:514，或者http://xxx:80/log",
    ),
    ("DataPath", "数据目录。本地数据库目录，默认Data子目录"),
    ("BackupPath", "备份目录。备份数据库时存放的目录，默认Backup子目录"),
    ("PluginPath", "插件目录。本地插件存放目录，默认Plugins子目录"),
    (
        "PluginServer",
        "插件服务器。将从该网页上根据关键字分析链接并下载插件，部分嵌入式设备不支持https",
    ),
    (
        "ServiceAddress",
        "服务地址。用户访问的外网地址，反向代理之外，用于内部构造其它Url（如SSO），或者向注册中心登记，多地址逗号隔开",
    ),
];

/// 核心设置。对应 C# `NewLife.Setting`。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Setting {
    /// 是否启用全局调试。默认启用
    pub debug: bool,
    /// 日志等级，只输出大于等于该级别的日志，All/Debug/Info/Warn/Error/Fatal，默认 Info
    pub log_level: String,
    /// 文件日志目录。默认 Log 子目录
    pub log_path: String,
    /// 日志文件上限。超过上限后拆分新日志文件，默认 10MB，0 表示不限制大小
    pub log_file_max_bytes: i32,
    /// 日志文件备份。超过备份数后，最旧的文件将被删除，默认 200，0 表示不限制个数
    pub log_file_backups: i32,
    /// 日志文件格式。默认 `{0:yyyy_MM_dd}.log`
    pub log_file_format: String,
    /// 日志行格式。默认 `Time|ThreadId|Kind|Name|Message`
    pub log_line_format: String,
    /// 网络日志。本地子网日志广播 `udp://255.255.255.255:514`，或者 `http://xxx:80/log`
    pub network_log: String,
    /// 数据目录。本地数据库目录，默认 Data 子目录
    pub data_path: String,
    /// 备份目录。备份数据库时存放的目录，默认 Backup 子目录
    pub backup_path: String,
    /// 插件目录。本地插件存放目录，默认 Plugins 子目录
    pub plugin_path: String,
    /// 插件服务器
    pub plugin_server: String,
    /// 服务地址。用户访问的外网地址，多地址逗号隔开
    pub service_address: String,
    /// 加载来源文件，不参与序列化
    #[serde(skip)]
    pub(crate) source: Option<PathBuf>,
}

impl Default for Setting {
    fn default() -> Self {
        Setting {
            debug: true,
            log_level: "Info".to_string(),
            log_path: String::new(),
            log_file_max_bytes: 10,
            log_file_backups: 200,
            log_file_format: "{0:yyyy_MM_dd}.log".to_string(),
            log_line_format: "Time|ThreadId|Kind|Name|Message".to_string(),
            network_log: String::new(),
            data_path: String::new(),
            backup_path: String::new(),
            plugin_path: String::new(),
            plugin_server: "http://x.newlifex.com/".to_string(),
            service_address: String::new(),
            source: None,
        }
    }
}

impl Setting {
    /// 读取默认配置文件。
    ///
    /// 依次尝试 `Config/Core.config` 与 `Config/Core.json`；文件不存在或解析失败时
    /// 返回默认值（对应 C# “配置文件损坏时忽略错误，保证系统正常运行”的语义）。
    /// 读取成功后自动执行 [`Setting::apply_defaults`] 等价的目录补全逻辑。
    pub fn load() -> Setting {
        for candidate in [DEFAULT_CONFIG_FILE, DEFAULT_CONFIG_FILE_JSON] {
            let path = Path::new(candidate);
            if path.exists() {
                if let Ok(setting) = Setting::load_from(path) {
                    return setting;
                }
            }
        }

        let mut setting = Setting::default();
        setting.apply_defaults_from_env();
        setting
    }

    /// 从指定文件读取配置（XML 或 JSON，按扩展名区分）。解析失败返回错误。
    pub fn load_from<P: AsRef<Path>>(path: P) -> Result<Setting, ConfigError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(ConfigError::Io)?;
        // 自动去除 BOM，对齐 C# File.ReadAllText
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);

        let mut setting = if is_json(path) {
            super::json::from_json(text)?
        } else {
            setting_from_xml(text)?
        };

        setting.source = Some(path.to_path_buf());
        setting.apply_defaults_from_env();

        Ok(setting)
    }

    /// 保存到加载来源文件；若未指定来源，则保存到默认文件 `Config/Core.config`。
    pub fn save(&self) -> Result<(), ConfigError> {
        match &self.source {
            Some(path) => self.save_to(path),
            None => self.save_to(DEFAULT_CONFIG_FILE),
        }
    }

    /// 保存到指定文件（XML 或 JSON，按扩展名区分）。
    ///
    /// 与 C# `FileConfigProvider.OnWrite` 一致：
    /// - 序列化结果为空时不覆盖目标文件；
    /// - 去掉首尾空白后内容相同则跳过写入；
    /// - 先写临时文件再原子替换，避免中途崩溃产生半截文件。
    pub fn save_to<P: AsRef<Path>>(&self, path: P) -> Result<(), ConfigError> {
        let path = path.as_ref();
        let text = if is_json(path) {
            super::json::to_json(self)?
        } else {
            let root = path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "Core".to_string());
            xml::write_fields(&root, &self.fields())
        };

        atomic_write(path, &text)?;
        Ok(())
    }

    /// 按 C# `Setting.OnLoaded` 的逻辑补全目录默认值。
    ///
    /// - 程序位于 `netcoreapp*`/`net2`~`net10` 目录时把基础目录向上提升一级；
    /// - 应用名以 `Web`/`Api`/`Server`/`Service`/`Job` 结尾时，日志目录使用
    ///   `{root}{dir}Log`，数据/备份/插件目录共享上级目录；否则使用当前目录下的
    ///   `Log`/`Data`/`Backup`/`Plugins`。
    pub fn apply_defaults(&mut self, cwd: &Path, app_name: Option<&str>) {
        // 多应用项目，需要把基础目录向上提升一级
        let mut root = "../";
        let mut di = cwd.to_path_buf();
        let di_name = dir_name(cwd);
        if starts_with_ignore_case(
            &di_name,
            &[
                "netcoreapp",
                "net2",
                "net4",
                "net5",
                "net6",
                "net7",
                "net8",
                "net9",
                "net10",
            ],
        ) && cwd.parent().is_some()
        {
            root = "../../";
            di = cwd.parent().expect("checked parent").to_path_buf();
        }

        let dir_name = dir_name(&di);
        let name = match app_name {
            Some(name) if !name.is_empty() => name.to_string(),
            _ => dir_name.clone(),
        };

        if ends_with_ignore_case(&name, &["Web", "Api", "Server", "Service", "Job"]) {
            // 日志目录分开，其它目录共用
            if self.log_path.is_empty() {
                self.log_path = format!("{root}{dir_name}Log");
            }
            if self.data_path.is_empty() {
                self.data_path = format!("{root}Data");
            }
            if self.backup_path.is_empty() {
                self.backup_path = format!("{root}Backup");
            }
            if self.plugin_path.is_empty() {
                self.plugin_path = format!("{root}Plugins");
            }
        } else {
            if self.log_path.is_empty() {
                self.log_path = "Log".to_string();
            }
            if self.data_path.is_empty() {
                self.data_path = "Data".to_string();
            }
            if self.backup_path.is_empty() {
                self.backup_path = "Backup".to_string();
            }
            if self.plugin_path.is_empty() {
                self.plugin_path = "Plugins".to_string();
            }
        }
        if self.log_file_format.is_empty() {
            self.log_file_format = "{0:yyyy_MM_dd}.log".to_string();
        }

        if self.plugin_server.trim().is_empty() {
            self.plugin_server = "http://x.newlifex.com/".to_string();
        }
    }

    /// 使用进程环境（当前目录、可执行文件名）执行默认值补全。
    fn apply_defaults_from_env(&mut self) {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let app_name = std::env::current_exe()
            .ok()
            .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().to_string()));
        self.apply_defaults(&cwd, app_name.as_deref());
    }

    /// 加载来源文件。仅当通过 [`Setting::load_from`] 加载或 [`Setting::save_to`] 后存在。
    pub fn source(&self) -> Option<&Path> {
        self.source.as_deref()
    }

    /// 字段值列表（按声明顺序附带注释），供 XML 写入使用。
    fn fields(&self) -> Vec<(&'static str, &'static str, String)> {
        let value = serde_json::to_value(self).expect("serialize Setting");
        let map = value.as_object().expect("Setting 序列化为对象");

        FIELD_COMMENTS
            .iter()
            .map(|(name, comment)| {
                let text = match map.get(*name) {
                    Some(serde_json::Value::Bool(b)) => b.to_string(),
                    Some(serde_json::Value::Number(n)) => n.to_string(),
                    Some(serde_json::Value::String(s)) => s.clone(),
                    _ => String::new(),
                };
                (*name, *comment, text)
            })
            .collect()
    }
}

impl fmt::Display for Setting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Setting(Debug={}, LogLevel={})",
            self.debug, self.log_level
        )
    }
}

/// 归一化配置对象：按字段目标类型转换值。
///
/// - 布尔与整数同时接受原生类型与字符串（C# `JsonConfigProvider` 落盘均为字符串）；
/// - 字符串字段即使内容形如数字也保持字符串，不做启发式猜测；
/// - 无法转换的值直接丢弃，对应字段回落到结构体默认值，与 C# 绑定
///   “单个字段损坏不影响其余字段”的容错一致。
pub(crate) fn normalize_setting_value(value: serde_json::Value) -> serde_json::Value {
    let serde_json::Value::Object(mut map) = value else {
        return value;
    };

    let mut remove = Vec::new();
    for (key, item) in map.iter_mut() {
        let coerced = match key.as_str() {
            "Debug" => coerce_bool(item),
            "LogFileMaxBytes" | "LogFileBackups" => coerce_i32(item),
            // 其余均为字符串字段
            _ => coerce_string(item),
        };

        match coerced {
            Some(v) => *item = v,
            None => remove.push(key.clone()),
        }
    }
    for key in remove {
        map.remove(&key);
    }

    serde_json::Value::Object(map)
}

fn coerce_bool(value: &serde_json::Value) -> Option<serde_json::Value> {
    match value {
        serde_json::Value::Bool(b) => Some(serde_json::Value::Bool(*b)),
        serde_json::Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" => Some(serde_json::Value::Bool(true)),
            "false" => Some(serde_json::Value::Bool(false)),
            _ => None,
        },
        _ => None,
    }
}

fn coerce_i32(value: &serde_json::Value) -> Option<serde_json::Value> {
    let n = match value {
        serde_json::Value::Number(n) => n.as_i64()?,
        serde_json::Value::String(s) => s.trim().parse::<i64>().ok()?,
        _ => return None,
    };
    i32::try_from(n)
        .ok()
        .map(|v| serde_json::Value::Number(v.into()))
}

fn coerce_string(value: &serde_json::Value) -> Option<serde_json::Value> {
    match value {
        serde_json::Value::String(s) => Some(serde_json::Value::String(s.clone())),
        serde_json::Value::Bool(b) => Some(serde_json::Value::String(b.to_string())),
        serde_json::Value::Number(n) => Some(serde_json::Value::String(n.to_string())),
        _ => None,
    }
}

/// 从 XML 文本解析配置。
fn setting_from_xml(text: &str) -> Result<Setting, ConfigError> {
    let fields = xml::read_fields(text)?;

    // XML 为纯文本格式：先按字符串收集，再统一做按字段类型的归一化转换
    let mut map = serde_json::Map::new();
    for (name, value) in fields {
        // 名称按不区分大小写匹配已知字段并规范为 PascalCase，与 C# 绑定行为一致
        let canonical = FIELD_COMMENTS
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(&name))
            .map(|(n, _)| n.to_string())
            .unwrap_or(name);
        map.insert(canonical, serde_json::Value::String(value));
    }

    let value = normalize_setting_value(serde_json::Value::Object(map));
    serde_json::from_value(value).map_err(|e| ConfigError::Parse(e.to_string()))
}

/// 原子写入：先写临时文件，再重命名替换。对应 C# `FileConfigProvider.OnWrite`。
fn atomic_write(path: &Path, text: &str) -> Result<(), ConfigError> {
    // 空内容防御：序列化异常退化为空时，不修改目标文件
    if text.is_empty() {
        return Ok(());
    }

    // 双边 trim 比较，避免末尾换行差异导致的无谓重写
    if let Ok(old) = std::fs::read_to_string(path) {
        if old.trim() == text.trim() {
            return Ok(());
        }
    }

    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir).map_err(ConfigError::Io)?;
        }
    }

    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);

    std::fs::write(&tmp, text).map_err(ConfigError::Io)?;
    std::fs::rename(&tmp, path).map_err(ConfigError::Io)?;

    Ok(())
}

fn is_json(path: &Path) -> bool {
    path.extension()
        .map(|e| e.eq_ignore_ascii_case("json"))
        .unwrap_or(false)
}

fn dir_name(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default()
}

fn starts_with_ignore_case(text: &str, prefixes: &[&str]) -> bool {
    let lower = text.to_lowercase();
    prefixes
        .iter()
        .any(|p| lower.starts_with(&p.to_lowercase()))
}

fn ends_with_ignore_case(text: &str, suffixes: &[&str]) -> bool {
    let lower = text.to_lowercase();
    suffixes.iter().any(|s| lower.ends_with(&s.to_lowercase()))
}

/// 配置读写错误。
#[derive(Debug)]
pub enum ConfigError {
    /// 文件读写错误
    Io(std::io::Error),
    /// 内容解析错误
    Parse(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io(e) => write!(f, "配置读写失败: {e}"),
            ConfigError::Parse(e) => write!(f, "配置解析失败: {e}"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl From<std::io::Error> for ConfigError {
    fn from(e: std::io::Error) -> Self {
        ConfigError::Io(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn temp_dir(name: &str) -> PathBuf {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let seq = SEQ.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "dhrust-setting-{}-{}-{}",
            std::process::id(),
            name,
            seq
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn defaults_match_csharp() {
        let setting = Setting::default();
        assert!(setting.debug);
        assert_eq!(setting.log_level, "Info");
        assert_eq!(setting.log_path, "");
        assert_eq!(setting.log_file_max_bytes, 10);
        assert_eq!(setting.log_file_backups, 200);
        assert_eq!(setting.log_file_format, "{0:yyyy_MM_dd}.log");
        assert_eq!(setting.log_line_format, "Time|ThreadId|Kind|Name|Message");
        assert_eq!(setting.plugin_server, "http://x.newlifex.com/");
    }

    #[test]
    fn apply_defaults_plain_app() {
        let mut setting = Setting::default();
        setting.apply_defaults(Path::new("/srv/myapp"), Some("MyApp"));
        assert_eq!(setting.log_path, "Log");
        assert_eq!(setting.data_path, "Data");
        assert_eq!(setting.backup_path, "Backup");
        assert_eq!(setting.plugin_path, "Plugins");
    }

    #[test]
    fn apply_defaults_web_app() {
        let mut setting = Setting::default();
        setting.apply_defaults(Path::new("/srv/Portal"), Some("PortalWeb"));
        assert_eq!(setting.log_path, "../PortalLog");
        assert_eq!(setting.data_path, "../Data");
        assert_eq!(setting.backup_path, "../Backup");
        assert_eq!(setting.plugin_path, "../Plugins");
    }

    #[test]
    fn apply_defaults_netcoreapp_dir() {
        let mut setting = Setting::default();
        setting.apply_defaults(Path::new("/srv/Portal/netcoreapp3.1"), Some("PortalWeb"));
        assert_eq!(setting.log_path, "../../PortalLog");
        assert_eq!(setting.data_path, "../../Data");
    }

    #[test]
    fn apply_defaults_keeps_existing_values() {
        let mut setting = Setting {
            log_path: "MyLogs".to_string(),
            data_path: "/data".to_string(),
            ..Default::default()
        };
        setting.apply_defaults(Path::new("/srv/app"), Some("MyWeb"));
        assert_eq!(setting.log_path, "MyLogs");
        assert_eq!(setting.data_path, "/data");
        assert_eq!(setting.backup_path, "../Backup");
    }

    #[test]
    fn xml_roundtrip() {
        let dir = temp_dir("xml");
        let file = dir.join("Core.config");

        let setting = Setting {
            log_level: "Warn".to_string(),
            log_path: "Logs".to_string(),
            log_file_max_bytes: 20,
            service_address: "http://x/?a=1&b=2".to_string(),
            ..Default::default()
        };
        setting.save_to(&file).unwrap();

        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.starts_with("<?xml version=\"1.0\" encoding=\"utf-8\"?>"));
        assert!(text.contains("<Core>"));
        assert!(text.contains("<!--启用全局调试。XTrace.Debug-->"));
        assert!(text.contains("<Debug>true</Debug>"));
        assert!(text.contains("<LogLevel>Warn</LogLevel>"));
        assert!(text.contains("<ServiceAddress>http://x/?a=1&amp;b=2</ServiceAddress>"));

        let loaded = Setting::load_from(&file).unwrap();
        assert_eq!(loaded.log_level, "Warn");
        assert_eq!(loaded.log_path, "Logs");
        assert_eq!(loaded.log_file_max_bytes, 20);
        assert_eq!(loaded.service_address, "http://x/?a=1&b=2");
        assert_eq!(loaded.source(), Some(file.as_path()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_csharp_style_xml() {
        // 模拟 C# XmlConfigProvider 生成的文件（含注释、空元素、大小写差异）
        let text = r#"<?xml version="1.0" encoding="utf-8"?>
<Core>
  <!--启用全局调试。XTrace.Debug-->
  <Debug>True</Debug>
  <!--日志等级。只输出大于等于该级别的日志，All/Debug/Info/Warn/Error/Fatal，默认Info-->
  <LogLevel>Error</LogLevel>
  <!--文件日志目录。默认Log子目录-->
  <LogPath />
  <LogFileMaxBytes>30</LogFileMaxBytes>
  <UnknownKey>ignored</UnknownKey>
</Core>"#;

        let setting = setting_from_xml(text).unwrap();
        assert!(setting.debug);
        assert_eq!(setting.log_level, "Error");
        assert_eq!(setting.log_path, "");
        assert_eq!(setting.log_file_max_bytes, 30);
        assert_eq!(setting.log_file_backups, 200); // 缺省字段保持默认
    }

    #[test]
    fn json_roundtrip_and_source() {
        let dir = temp_dir("json");
        let file = dir.join("Core.json");

        let setting = Setting {
            debug: false,
            log_level: "Debug".to_string(),
            ..Default::default()
        };
        setting.save_to(&file).unwrap();

        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("\"Debug\": false"));
        assert!(text.contains("\"LogLevel\": \"Debug\""));

        let loaded = Setting::load_from(&file).unwrap();
        assert_eq!(loaded, {
            let mut expected = setting.clone();
            expected.source = Some(file.clone());
            // apply_defaults 会补全空目录
            expected.apply_defaults(
                &std::env::current_dir().unwrap(),
                std::env::current_exe()
                    .ok()
                    .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().to_string()))
                    .as_deref(),
            );
            expected
        });

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_skips_unchanged_content() {
        let dir = temp_dir("skip");
        let file = dir.join("Core.config");

        let setting = Setting::default();
        setting.save_to(&file).unwrap();
        let first = std::fs::read_to_string(&file).unwrap();

        // 内容相同（去掉首尾空白）时不应重写
        setting.save_to(&file).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), first);

        // 修改后应重写
        let mut changed = setting.clone();
        changed.log_path = "NewLogs".to_string();
        changed.save_to(&file).unwrap();
        let second = std::fs::read_to_string(&file).unwrap();
        assert_ne!(first, second);
        assert!(second.contains("NewLogs"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_missing_file_returns_defaults() {
        let setting = Setting::load_from("Z:/not-exist/definitely-not-there.config");
        assert!(setting.is_err());
    }

    #[test]
    fn case_insensitive_field_names() {
        let text = "<Core><debug>true</debug><logfilemaxbytes>15</logfilemaxbytes><logpath>X</logpath></Core>";
        let setting = setting_from_xml(text).unwrap();
        assert!(setting.debug);
        assert_eq!(setting.log_file_max_bytes, 15);
        assert_eq!(setting.log_path, "X");
    }

    #[test]
    fn numeric_like_string_fields_stay_strings() {
        // 字符串字段内容形如数字时不能被强转为数字，否则反序列化会失败
        let text = "<Core><LogPath>123</LogPath><ServiceAddress>456789</ServiceAddress></Core>";
        let setting = setting_from_xml(text).unwrap();
        assert_eq!(setting.log_path, "123");
        assert_eq!(setting.service_address, "456789");
    }

    #[test]
    fn invalid_values_fall_back_to_defaults() {
        // 单个字段损坏时保持默认值（对应 C# 绑定容错），不影响其它字段
        let text = "<Core><Debug>not-a-bool</Debug><LogFileMaxBytes>abc</LogFileMaxBytes><LogPath>Logs</LogPath></Core>";
        let setting = setting_from_xml(text).unwrap();
        assert!(setting.debug); // 默认 true
        assert_eq!(setting.log_file_max_bytes, 10); // 默认 10
        assert_eq!(setting.log_path, "Logs");
    }

    #[test]
    fn json_from_csharp_string_values() {
        // C# JsonConfigProvider 落盘时所有值均为字符串，需要弹性解析
        let text = r#"{
  "Debug": "false",
  "LogLevel": "Warn",
  "LogFileMaxBytes": "20",
  "LogFileBackups": "5",
  "LogPath": "Logs",
  "Unknown": "ignored"
}"#;
        let setting = crate::config::json::from_json(text).unwrap();
        assert!(!setting.debug);
        assert_eq!(setting.log_level, "Warn");
        assert_eq!(setting.log_file_max_bytes, 20);
        assert_eq!(setting.log_file_backups, 5);
        assert_eq!(setting.log_path, "Logs");
    }
}
