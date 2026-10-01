//! 系统信息采集（跨平台）：磁盘挂载过滤与网卡累计流量。
//!
//! 由 Pek.RAgent 与 DHDeploy.Agent.Rust 的重复实现提炼（2026-10-01），
//! 口径对齐 C# `DHDeploy.Agent` 的 `DiskUsageJob.IsTemporaryVolume` 与 `ShowMachineInfo`。
//!
//! - [`disk`]：挂载点过滤黑名单、`/proc/mounts` 八进制转义还原（`\040` 等）
//! - [`net`]：网卡累计流量（Linux `/proc/net/dev`；Windows `GetIfTable2`）

pub mod disk;
pub mod net;
