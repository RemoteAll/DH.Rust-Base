//! 配置项，对应 DH.NCore `NewLife.Configuration` / `NewLife.Setting`。
//!
//! - [`Setting`]：核心设置，与 C# `NewLife.Setting` 字段一一对应，默认配置文件为
//!   `Config/Core.config`（XML），也支持 `Config/Core.json`（JSON）。
//! - [`Config`]：通用配置文件，对应 C# `Config<TConfig>`；缺失自动生成、
//!   缺失属性自动补齐回存、损坏备份重建（DH.NCore `Configuration/Config.cs` 语义）。
//! - Rust 读取 C# 写的配置文件、C# 读取 Rust 写的配置文件均保持兼容。
//!
//! # 示例
//!
//! ```no_run
//! use dhrust::config::Setting;
//!
//! // 读取 Config/Core.config（不存在时返回默认值）
//! let mut setting = Setting::load();
//! setting.log_level = "Debug".to_string();
//! setting.save().unwrap();
//! ```

mod file_config;
mod json;
mod setting;
mod xml;

pub use file_config::{save_value, Config, ConfigOptions};
pub use setting::{ConfigError, Setting};
