//! 配置文件存储（`TomlConfigStore<T>`，`feature = "toml"`）。
//!
//! `TomlConfig<T>` 负责“单个文件句柄”的加载/保存/热加载；本类型在它之上补上
//! **共享快照 + 文件句柄** 的常用编排，供服务/库的 Facade 直接复用：
//!
//! - 打开（或按模板生成）配置文件，并把当前值放入 `Arc<RwLock<T>>` 供多线程读取；
//! - 保存时同时更新文件与共享快照；
//! - 外部修改时热加载并刷新共享快照；
//! - 纯内存模式（`in_memory`）不读写磁盘，行为与文件模式一致。
//!
//! 典型消费方：`Pek.RMail::Mailer`、`Pek.RSms::Smser`（此前各自维护一份
//! `Option<Mutex<SettingsFile>> + Arc<RwLock<Settings>>` 的样板代码）。
//!
//! # 示例
//! ```no_run
//! use dhrust::config::TomlConfigStore;
//!
//! const TEMPLATE: &str = "# 示例\nvalue = 7\n";
//! # #[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
//! # struct Cfg { value: i64 }
//! let store = TomlConfigStore::<Cfg>::open("Config/Demo.toml", TEMPLATE);
//! let shared = store.shared();
//! println!("{}", store.value().value);
//! store.save(Cfg { value: 9 }, &[]).unwrap();
//! assert_eq!(shared.read().unwrap().value, 9);
//! ```

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

use super::setting::ConfigError;
use super::toml_config::TomlConfig;

/// TOML 配置存储：文件句柄 + 共享快照。
///
/// - `open` / `open_in`：文件模式（缺失按模板生成、损坏备份重建、支持热加载）；
/// - `in_memory`：纯内存模式（不读写磁盘）。
pub struct TomlConfigStore<T> {
    path: Option<PathBuf>,
    file: Option<Mutex<TomlConfig<T>>>,
    shared: Arc<RwLock<T>>,
}

impl<T> TomlConfigStore<T>
where
    T: Serialize + DeserializeOwned + Default + Clone + Send + Sync,
{
    /// 打开指定路径（不存在时按模板生成）。
    pub fn open<P: AsRef<Path>>(path: P, template: &str) -> Self {
        let path = path.as_ref().to_path_buf();
        let config = TomlConfig::<T>::load(&path, template);
        let shared = Arc::new(RwLock::new(config.value().clone()));
        Self {
            path: Some(path),
            file: Some(Mutex::new(config)),
            shared,
        }
    }

    /// 从基目录 + 相对路径打开（如 `open_in(".", "Config/Demo.toml", TEMPLATE)`）。
    pub fn open_in<P: AsRef<Path>>(base: P, relative: &str, template: &str) -> Self {
        Self::open(base.as_ref().join(relative), template)
    }

    /// 纯内存模式（测试或动态配置；不读写磁盘）。
    pub fn in_memory(value: T) -> Self {
        Self {
            path: None,
            file: None,
            shared: Arc::new(RwLock::new(value)),
        }
    }

    /// 是否文件模式（`false` = 纯内存）。
    pub fn is_persistent(&self) -> bool {
        self.file.is_some()
    }

    /// 配置文件路径（纯内存模式返回 `None`）。
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// 当前配置快照（克隆；多线程安全）。
    pub fn value(&self) -> T {
        self.shared
            .read()
            .map(|value| value.clone())
            .unwrap_or_else(|poisoned| poisoned.into_inner().clone())
    }

    /// 共享配置快照（供发送器/客户端等长期持有）。
    pub fn shared(&self) -> Arc<RwLock<T>> {
        Arc::clone(&self.shared)
    }

    /// 是否首次生成（或损坏后重建）；纯内存模式返回 `false`。
    pub fn is_new(&self) -> bool {
        self.file
            .as_ref()
            .map(|file| {
                file.lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .is_new()
            })
            .unwrap_or(false)
    }

    /// 取走加载/保存期间的提示（生成/补齐/迁移等）；纯内存模式为空。
    pub fn take_notes(&self) -> Vec<String> {
        self.file
            .as_ref()
            .map(|file| {
                file.lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take_notes()
            })
            .unwrap_or_default()
    }

    /// 外部修改时热加载，并刷新共享快照；返回提示文本（空 = 未变化）。
    pub fn reload_if_changed(&self) -> Vec<String> {
        let Some(file) = self.file.as_ref() else {
            return Vec::new();
        };
        let mut config = file.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let notes = config.reload_if_changed();
        if !notes.is_empty() {
            // 解析失败时 `TomlConfig` 保持旧值，这里同步旧值也是安全的。
            self.replace_shared(config.value().clone());
        }
        notes
    }

    /// 保存配置（标量 + 可选数组表），成功后同时刷新共享快照。
    ///
    /// `arrays` 的每个元素为 `(数组表名, 条目列表)`，语义同
    /// [`TomlConfig::save_with_arrays`]；纯内存模式只更新共享快照。
    pub fn save(&self, value: T, arrays: &[(&str, &[Value])]) -> Result<(), ConfigError> {
        if let Some(file) = self.file.as_ref() {
            let mut config = file.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let previous = config.value().clone();
            *config.value_mut() = value.clone();
            if let Err(error) = config.save_with_arrays(arrays) {
                // 保存失败时回滚文件句柄里的值，避免“内存改了、磁盘没改”的不一致。
                *config.value_mut() = previous;
                return Err(error);
            }
        }
        self.replace_shared(value);
        Ok(())
    }

    /// 用共享快照的当前值保存（文件模式写盘；纯内存模式空操作）。
    pub fn save_current(&self, arrays: &[(&str, &[Value])]) -> Result<(), ConfigError> {
        self.save(self.value(), arrays)
    }

    /// 只更新共享快照（不写盘；需要持久化时再调用 [`Self::save`] 或 [`Self::save_current`]）。
    pub fn set(&self, value: T) {
        self.replace_shared(value);
    }

    fn replace_shared(&self, value: T) {
        match self.shared.write() {
            Ok(mut guard) => *guard = value,
            Err(poisoned) => *poisoned.into_inner() = value,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    const TEMPLATE: &str = "# 示例\nvalue = 7\nname = \"\"\n\n[[items]]\nname = \"a\"\n";

    #[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
    #[serde(default, rename_all = "camelCase")]
    struct Demo {
        value: i64,
        name: String,
        items: Vec<Item>,
    }

    #[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
    #[serde(default, rename_all = "camelCase")]
    struct Item {
        name: String,
    }

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dhrust-store-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建目录");
        dir
    }

    #[test]
    fn open_creates_file_and_exposes_shared_snapshot() {
        let dir = temp("open");
        let file = dir.join("Demo.toml");
        let store = TomlConfigStore::<Demo>::open(&file, TEMPLATE);
        assert!(store.is_persistent());
        assert!(store.is_new());
        assert_eq!(store.path(), Some(file.as_path()));
        assert_eq!(store.value().value, 7);
        assert_eq!(store.shared().read().unwrap().value, 7);
        assert!(file.is_file());
        assert!(store
            .take_notes()
            .iter()
            .any(|note| note.contains("生成默认配置")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_updates_file_and_shared_snapshot() {
        let dir = temp("save");
        let file = dir.join("Demo.toml");
        let store = TomlConfigStore::<Demo>::open(&file, TEMPLATE);
        let value = Demo {
            value: 9,
            name: "new".into(),
            items: vec![Item { name: "b".into() }],
        };
        let entries = vec![serde_json::json!({ "name": "b" })];
        let arrays: [(&str, &[Value]); 1] = [("items", entries.as_slice())];
        store.save(value.clone(), &arrays).expect("保存");
        assert_eq!(store.value(), value);
        assert_eq!(store.shared().read().unwrap().clone(), value);
        let text = std::fs::read_to_string(&file).expect("读");
        assert!(text.contains("# 示例"), "模板注释应保留：{text}");
        assert!(text.contains("value = 9"));
        assert!(text.contains("[[items]]"));
        assert!(text.contains("name = \"b\""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn in_memory_store_does_not_touch_disk() {
        let store = TomlConfigStore::<Demo>::in_memory(Demo {
            value: 3,
            ..Demo::default()
        });
        assert!(!store.is_persistent());
        assert_eq!(store.path(), None);
        assert_eq!(store.value().value, 3);
        store.set(Demo {
            value: 4,
            ..Demo::default()
        });
        assert_eq!(store.value().value, 4);
        store.save_current(&[]).expect("纯内存保存应成功");
        assert_eq!(store.value().value, 4);
    }

    #[test]
    fn reload_if_changed_refreshes_shared_snapshot() {
        let dir = temp("reload");
        let file = dir.join("Demo.toml");
        let store = TomlConfigStore::<Demo>::open(&file, TEMPLATE);
        std::fs::write(&file, "value = 42\nname = \"external\"\n").expect("写外部修改");
        let notes = store.reload_if_changed();
        assert!(
            notes.iter().any(|note| note.contains("热加载")),
            "{notes:?}"
        );
        assert_eq!(store.value().value, 42);
        assert_eq!(store.value().name, "external");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
