//! 配置文件变更检测（轮询式，无运行时依赖）。
//!
//! 与 [`file_stamp`](super::file_stamp) 配套：调用方按自身节奏（如每秒定时器）
//! 调用 [`FileWatcher::poll`]，有变化时返回事件；无变化返回 `None`。
//!
//! 失败保护语义：先推进版本戳再返回结果——编辑中的半成品文件不会反复触发变更事件，
//! 调用方应根据事件类型决定“保持当前配置/继续使用旧值”。

use std::path::{Path, PathBuf};

use super::{file_config::FileStamp, file_stamp};

/// 文件变更事件。
#[derive(Debug)]
pub enum FileChange {
    /// 文件内容有变化，携带新文本
    Changed(String),
    /// 文件被删除
    Removed,
    /// 检测到变化但读取失败（编辑中半成品/权限等），调用方应保持当前配置
    Error(String),
}

/// 配置文件变更检测器（轮询式）。
pub struct FileWatcher {
    path: PathBuf,
    last: Option<FileStamp>,
}

impl FileWatcher {
    /// 创建监视器并记录当前版本戳作为基线。
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let last = file_stamp(&path);
        Self { path, last }
    }

    /// 当前监视的文件路径。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 轮询变更：有变化时返回事件并推进基线；无变化返回 `None`。
    pub fn poll(&mut self) -> Option<FileChange> {
        let now = file_stamp(&self.path);
        if now == self.last {
            return None;
        }
        // 无论结果如何都先推进版本戳：避免半成品文件反复触发告警
        self.last = now;

        match now {
            None => Some(FileChange::Removed),
            Some(_) => match crate::io::read_all_text(&self.path) {
                Ok(text) => Some(FileChange::Changed(text)),
                Err(e) => Some(FileChange::Error(e.to_string())),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn detects_change_then_none_then_removed() {
        let dir = std::env::temp_dir().join(format!("dhrust-watch-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("config.toml");
        fs::write(&file, "a").unwrap();

        let mut watcher = FileWatcher::new(&file);
        assert!(watcher.poll().is_none(), "初始无变化");

        // 使用不同长度内容确保版本戳（长度+时间）必然变化
        fs::write(&file, "bb").unwrap();
        match watcher.poll() {
            Some(FileChange::Changed(text)) => assert_eq!(text, "bb"),
            other => panic!("应检测到内容变化：{other:?}"),
        }
        assert!(watcher.poll().is_none(), "同一版本只上报一次");

        fs::remove_file(&file).unwrap();
        assert!(
            matches!(watcher.poll(), Some(FileChange::Removed)),
            "删除应上报 Removed"
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
