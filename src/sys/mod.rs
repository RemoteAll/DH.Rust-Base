//! 系统信息采集与进程管理（跨平台，零 feature 依赖）。
//!
//! 由 Pek.RAgent / DHDeploy.Agent.Rust 的重复实现提炼（2026-10-01 第一批：
//! 磁盘过滤/网卡流量；2026-10-03 第二批：进程管理/指标采集/机器事实），
//! 口径对齐 C# `DHDeploy.Agent` 的 `DiskUsageJob.IsTemporaryVolume` 与 `ShowMachineInfo`、
//! 以及 C#/宝塔（psutil）的 CPU/内存统计语义。
//!
//! - [`disk`]：挂载点过滤黑名单、`/proc/mounts` 八进制转义还原（`\040` 等）
//! - [`net`]：网卡累计流量（Linux `/proc/net/dev`；Windows `GetIfTable2`）
//! - [`process`]：进程管理（拉起/存活判定/信号与强制停止/资源查询/优先级）
//! - [`monitor`]：实时指标采集（CPU 差分/负载/TCP 连接数/磁盘 IO/进程统计/Top）
//! - [`machine`]：机器事实与设置（主机/系统/CPU/内存/网络接口/磁盘/机器标识/校时）

pub mod disk;
pub mod machine;
pub mod monitor;
pub mod net;
pub mod process;
