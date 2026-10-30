//! 运行资源快照（**整机 + 本进程**），各项目面板「运行资源 / 运行状态」共用一套口径。
//!
//! # 为什么放在这里
//! 这套字段（本进程 CPU/内存/线程/句柄 + 整机 CPU/内存 + 磁盘使用率 + 运行时长）此前在
//! Pek.RAgent、Pek.RPanlServer、Pek.RCard、DH.CentralControl、HlkProductTool、HLZX.EIP
//! 各拼过一份，字段名与统计口径反复漂移（有的用 `procMemoryMB`，有的用 `memoryMB`；
//! 有的把磁盘 `free` 当已用）。统一到这里后，各面板只写一行 `static` + 一次调用。
//!
//! # 口径
//! - **整机 CPU**：`monitor::system_cpu_rate()`（busy / (busy+idle+iowait)，Windows 用 GetSystemTimes）；
//! - **整机内存**：`机器总内存 - 可用内存`（对齐 psutil / 宝塔；不是「已用+缓存」的自定义算法）；
//! - **磁盘**：只列**已就绪且总量 > 0** 的盘（光驱空盘、未挂载卷不出现），已用 = 总量 - 可用；
//! - **本进程 CPU**：两次采样之间的差分（窗口 = 调用间隔），首帧按「自启动以来平均」近似；
//! - **运行时长**：自 `process::mark_started()`（未打点时退化为首次调用时刻）。
//!
//! # 首次调用的 200ms
//! `system_cpu_rate()` 首次需要 200ms 双采样建立基线。**不要在请求路径里首次调用**：
//! 启动时调一次 [`ResourceMonitor::warm_up`]（或在路由注册处预热），之后每次采样都是即时返回。

use serde_json::{json, Value};

use super::{machine, monitor, process};

/// 单个磁盘的使用情况（MB 口径，前端再换算 GB）。
#[derive(Debug, Clone, PartialEq)]
pub struct DiskUsage {
    /// 盘符（`C:\`）或挂载点（`/`、`/data`）
    pub name: String,
    /// 已用（MB）
    pub used_mb: u64,
    /// 总量（MB）
    pub total_mb: u64,
}

impl DiskUsage {
    /// 已用占比（0~100；总量为 0 时返回 0）。
    pub fn used_percent(&self) -> f64 {
        if self.total_mb == 0 {
            0.0
        } else {
            (self.used_mb as f64) * 100.0 / (self.total_mb as f64)
        }
    }

    /// 转 JSON（`usedMB` / `totalMB` / `usedPercent`）。
    pub fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "usedMB": self.used_mb,
            "totalMB": self.total_mb,
            "usedPercent": self.used_percent(),
        })
    }
}

/// 本进程资源。
#[derive(Debug, Clone, PartialEq)]
pub struct ProcessResource {
    /// 进程 ID
    pub pid: u32,
    /// 进程 CPU 占用（0~100，相对**单核**；8 核跑满 8 线程为 800）
    pub cpu_rate: Option<f64>,
    /// 进程内存（常驻/工作集，MB；平台不支持时为 None）
    pub memory_mb: Option<u64>,
    /// 线程数
    pub threads: u32,
    /// 句柄数（Windows 有意义；其它平台为 0）
    pub handles: u32,
    /// 运行时长（秒）
    pub uptime_secs: u64,
    /// 启动时刻（本地时间文本 `YYYY-mm-dd HH:MM:SS`）
    pub started_at: String,
}

/// 整机资源。
#[derive(Debug, Clone, PartialEq)]
pub struct SystemResource {
    /// 整机 CPU 占用（0~100）
    pub cpu_rate: Option<f64>,
    /// 逻辑核数
    pub cpu_count: usize,
    /// 已用内存（MB）
    pub mem_used_mb: u64,
    /// 总内存（MB）
    pub mem_total_mb: u64,
    /// 主机已运行时长（秒）
    pub host_uptime_secs: u64,
    /// 主机名
    pub hostname: String,
    /// 系统描述（如 `Windows 11 (26100)`、`Ubuntu 24.04`）
    pub os: String,
    /// 1/5/15 分钟负载（Windows 无此概念 → None）
    pub load: Option<(f64, f64, f64)>,
}

/// 一次完整的资源快照。
#[derive(Debug, Clone, PartialEq)]
pub struct ResourceSnapshot {
    /// 本进程
    pub process: ProcessResource,
    /// 整机
    pub system: SystemResource,
    /// 磁盘（最多 `disk_limit` 个，默认 4）
    pub disks: Vec<DiskUsage>,
}

impl ResourceSnapshot {
    /// 采样一次（**整机 CPU 首次调用会阻塞约 200ms**；请用 [`ResourceMonitor`] 并先 `warm_up`）。
    pub fn capture(disk_limit: usize) -> Self {
        ResourceSnapshot::capture_with(disk_limit, None)
    }

    /// 采样一次，进程 CPU 用外部采样器给出的值（无采样器时为 None）。
    fn capture_with(disk_limit: usize, proc_cpu: Option<f64>) -> Self {
        let pid = std::process::id();
        let (threads, handles) = monitor::process_stats(pid).unwrap_or((0, 0));
        let (mem_total_mb, mem_used_mb) = match machine::memory_info() {
            Some((total, avail)) => (
                total / 1024 / 1024,
                total.saturating_sub(avail) / 1024 / 1024,
            ),
            None => (0, 0),
        };
        let disks: Vec<DiskUsage> = machine::disk_usages()
            .into_iter()
            .take(disk_limit)
            .map(|(used_mb, total_mb, name)| DiskUsage {
                name,
                used_mb,
                total_mb,
            })
            .collect();
        ResourceSnapshot {
            process: ProcessResource {
                pid,
                cpu_rate: proc_cpu,
                memory_mb: process::memory_mb(pid),
                threads,
                handles,
                uptime_secs: process::uptime().as_secs(),
                started_at: process::start_time_local()
                    .format("%Y-%m-%d %H:%M:%S")
                    .to_string(),
            },
            system: SystemResource {
                cpu_rate: monitor::system_cpu_rate(),
                cpu_count: std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(0),
                mem_used_mb,
                mem_total_mb,
                host_uptime_secs: machine::host_uptime_seconds(),
                hostname: machine::hostname(),
                os: machine::os_description(),
                load: monitor::load_average(),
            },
            disks,
        }
    }

    /// 统一 JSON（**字段名与 Pek.RPanlServer `/api/resources` 一致**，前端可直接复用）。
    ///
    /// 扁平结构便于 `Object.assign` 直接合并进前端状态；`version` 这类与 crate 相关的事实由调用方补。
    pub fn to_json(&self) -> Value {
        let p = &self.process;
        let s = &self.system;
        json!({
            "pid": p.pid,
            "uptimeSecs": p.uptime_secs,
            "startTime": p.started_at,
            "procCpuRate": p.cpu_rate,
            "procMemoryMB": p.memory_mb,
            "procThreads": p.threads,
            "procHandles": p.handles,
            "sysCpuRate": s.cpu_rate,
            "sysMemUsedMB": s.mem_used_mb,
            "sysMemTotalMB": s.mem_total_mb,
            "cpuCount": s.cpu_count,
            "hostUptimeSec": s.host_uptime_secs,
            "hostname": s.hostname,
            "os": s.os,
            "load": match s.load {
                Some((a, b, c)) => json!([a, b, c]),
                None => Value::Null,
            },
            "disks": self.disks.iter().map(|d| d.to_json()).collect::<Vec<_>>(),
        })
    }
}

/// 带**进程 CPU 差分**的采样器：每个进程放一个 `static`，启动时 `warm_up` 一次。
///
/// ```ignore
/// static MON: dhrust::sys::resources::ResourceMonitor =
///     dhrust::sys::resources::ResourceMonitor::new();
/// // 启动时（可选，避免首帧 200ms）
/// MON.warm_up();
/// // 面板每次刷新
/// let v = MON.snapshot_json(4);
/// ```
pub struct ResourceMonitor {
    cpu: monitor::ProcessCpuMeter,
}

impl Default for ResourceMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl ResourceMonitor {
    /// 创建采样器（`const`，可直接放 `static`）。
    pub const fn new() -> Self {
        Self {
            cpu: monitor::ProcessCpuMeter::new(),
        }
    }

    /// 预热：建立整机 CPU 基线（首次约 200ms）与进程 CPU 基线。
    ///
    /// 启动时调用一次即可，之后 [`snapshot`](Self::snapshot) 不再阻塞。
    pub fn warm_up(&self) {
        let _ = monitor::system_cpu_rate();
        let _ = self.cpu.sample();
    }

    /// 采样一次（进程 CPU 窗口 = 与上次采样的间隔；无基线时按自启动以来平均）。
    pub fn snapshot(&self, disk_limit: usize) -> ResourceSnapshot {
        let proc_cpu = Some(self.cpu.sample_since(process::started_instant()));
        ResourceSnapshot::capture_with(disk_limit, proc_cpu)
    }

    /// 采样并输出统一 JSON（见 [`ResourceSnapshot::to_json`]）。
    pub fn snapshot_json(&self, disk_limit: usize) -> Value {
        self.snapshot(disk_limit).to_json()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 快照字段齐备、口径自洽（磁盘已用 ≤ 总量、内存已用 ≤ 总量）。
    #[test]
    fn snapshot_has_consistent_values() {
        let mon = ResourceMonitor::new();
        mon.warm_up();
        let snap = mon.snapshot(4);
        assert!(snap.process.pid > 0);
        assert!(!snap.system.hostname.is_empty());
        assert!(snap.system.mem_used_mb <= snap.system.mem_total_mb);
        assert!(snap.system.cpu_count > 0);
        for d in &snap.disks {
            assert!(d.used_mb <= d.total_mb, "磁盘 {} 已用超总量", d.name);
            assert!(d.used_percent() <= 100.0);
        }
        // 进程 CPU 采样器在预热后必须给得出值（否则面板永远是 —）
        assert!(snap.process.cpu_rate.is_some());
    }

    /// JSON 字段名是各面板的公共契约，改动即为破坏性变更。
    #[test]
    fn json_exposes_stable_field_names() {
        let mon = ResourceMonitor::new();
        mon.warm_up();
        let v = mon.snapshot_json(2);
        for key in [
            "pid",
            "uptimeSecs",
            "startTime",
            "procCpuRate",
            "procMemoryMB",
            "procThreads",
            "procHandles",
            "sysCpuRate",
            "sysMemUsedMB",
            "sysMemTotalMB",
            "cpuCount",
            "hostUptimeSec",
            "hostname",
            "os",
            "load",
            "disks",
        ] {
            assert!(v.get(key).is_some(), "缺少字段 {key}");
        }
        assert!(v["disks"].is_array());
    }

    /// 磁盘已用占比：总量为 0 时不能出现除零/NaN。
    #[test]
    fn disk_percent_handles_zero_total() {
        let d = DiskUsage {
            name: "X".into(),
            used_mb: 0,
            total_mb: 0,
        };
        assert_eq!(d.used_percent(), 0.0);
        let half = DiskUsage {
            name: "C".into(),
            used_mb: 50,
            total_mb: 200,
        };
        assert!((half.used_percent() - 25.0).abs() < 1e-9);
    }
}
