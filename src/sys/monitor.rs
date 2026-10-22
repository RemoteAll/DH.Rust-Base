//! 实时指标采集（跨平台）：CPU 使用率（差分）/负载/TCP 连接数/磁盘 IO/进程统计/Top。
//!
//! 由 Pek.RAgent 下沉（2026-10-03 第二批）：Web 面板与采样器共用的“整机视角”指标。
//! 口径对齐 psutil / 宝塔：
//! - CPU：`busy = Δ总时 − Δ(idle + iowait)`（Linux 总时扣除 guest/guest_nice 防重复）；
//! - 采样窗口 = 与上次调用间隔（面板 3 秒刷新 → 3 秒窗口均值），首调 200ms 双采样建基线。
//!
//! 进程 CPU 时间为“内核 + 用户”累计值（与 C# `TotalProcessorTime` 语义一致，
//! Linux ticks 按 100Hz 换算）。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 进程条目（Web 面板 Top 列表；字段口径对齐 htop / 云监控「进程监控」）。
#[derive(Clone)]
pub struct ProcItem {
    /// 进程名（不含 .exe 后缀）
    pub name: String,
    /// 进程 ID
    pub pid: u32,
    /// 内存（MB；RSS / 工作集）
    pub memory_mb: u64,
    /// 线程数
    pub threads: u32,
    /// CPU 时间（秒；内核 + 用户，累计值，与 C# `TotalProcessorTime` 语义一致）
    pub cpu_seconds: f64,
    /// 瞬时 CPU（占**单核**百分比，可 > 100，与 htop / 云监控一致；首次采样为 0）
    pub cpu_percent: f32,
    /// 运行账户（Linux 由 `/proc/{pid}/status` 的 Uid 反查；Windows 暂留空）
    pub user: String,
    /// 可执行文件完整路径（「文件目录」）
    pub exe: String,
    /// 完整命令行（Linux `cmdline`；Windows 用可执行文件路径代替）
    pub command: String,
    /// 工作目录（Linux `cwd`；Windows 留空）
    pub cwd: String,
    /// 句柄 / 打开文件数（Windows 句柄数；Linux `/proc/{pid}/fd` 条目数）
    pub handles: u32,
    /// 启动时间（Unix 秒；0 = 取不到）
    pub start_time: i64,
    /// 本进程监听的 TCP 端口（升序去重；不监听则为空）。
    ///
    /// 由 [`listen_ports_map`] 的统一快照填充 —— 面板上能直接看出
    /// 「dotnet 这个宿主到底在服务哪个端口」，也便于与假死探活目标对照。
    pub ports: Vec<u16>,
}

/// 进程统计（线程数、句柄数）。取不到返回 None。
pub fn process_stats(pid: u32) -> Option<(u32, u32)> {
    #[cfg(windows)]
    {
        use std::mem::zeroed;
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
            TH32CS_SNAPPROCESS,
        };
        use windows_sys::Win32::System::Threading::{
            GetProcessHandleCount, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };

        unsafe {
            let mut threads = 0u32;
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snapshot != INVALID_HANDLE_VALUE && !snapshot.is_null() {
                let mut entry: PROCESSENTRY32W = zeroed();
                entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
                if Process32FirstW(snapshot, &mut entry) != 0 {
                    loop {
                        if entry.th32ProcessID == pid {
                            threads = entry.cntThreads;
                            break;
                        }
                        if Process32NextW(snapshot, &mut entry) == 0 {
                            break;
                        }
                    }
                }
                CloseHandle(snapshot);
            }

            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return Some((threads, 0));
            }
            let mut handles = 0u32;
            let ok = GetProcessHandleCount(h, &mut handles);
            CloseHandle(h);

            Some((threads, if ok != 0 { handles } else { 0 }))
        }
    }

    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        let threads = status
            .lines()
            .find_map(|l| l.strip_prefix("Threads:"))
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(0);
        let handles = std::fs::read_dir(format!("/proc/{pid}/fd"))
            .map(|it| it.count() as u32)
            .unwrap_or(0);
        Some((threads, handles))
    }

    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = pid;
        None
    }
}

/// 上次 CPU 采样 `(空转+等待 ticks, 总 ticks, 上次速率)`——请求间差分。
static CPU_LAST: Mutex<Option<(u64, u64, f64)>> = Mutex::new(None);

/// 由两次采样差值计算使用率（0~100）。
///
/// 口径对齐 psutil/宝塔：`busy = Δ总时 − Δ(idle + iowait)`；字段回退（负增量）按 0 处理
/// （与 top/psutil 一致）；窗口内总时无变化（同 tick 内重复调用）返回 None。
pub fn cpu_rate_from_delta(prev: (u64, u64), cur: (u64, u64)) -> Option<f64> {
    let dtotal = cur.1.saturating_sub(prev.1);
    if dtotal == 0 {
        return None;
    }
    let didle = cur.0.saturating_sub(prev.0);
    Some(((1.0 - didle as f64 / dtotal as f64) * 100.0).clamp(0.0, 100.0))
}

/// 解析 `/proc/stat` 首行，返回 `(idle + iowait, 总时)`（ticks）。
///
/// guest/guest_nice 已计入 user/nice，总时需扣除（psutil、htop 同口径）。
#[cfg(any(target_os = "linux", test))]
pub fn parse_proc_stat_first_cpu(text: &str) -> Option<(u64, u64)> {
    let line = text.lines().find(|l| l.starts_with("cpu "))?;
    let values: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|v| v.parse().ok())
        .collect();
    if values.len() < 4 {
        return None;
    }
    let idle = values[3] + values.get(4).copied().unwrap_or(0);
    let guest = values.get(8).copied().unwrap_or(0) + values.get(9).copied().unwrap_or(0);
    let total = values.iter().sum::<u64>().saturating_sub(guest);
    Some((idle, total))
}

/// 系统 CPU 使用率（0~100）。
///
/// 口径对齐 psutil/宝塔：`使用率 = busy / (busy + idle + iowait)`（Linux 扣除 guest 重复计数）；
/// 采样窗口 = 与上次调用之间（面板 3 秒刷新 → 3 秒窗口均值），首次调用以 200ms 双采样建立基线。
pub fn system_cpu_rate() -> Option<f64> {
    #[cfg(windows)]
    let sample = || -> Option<(u64, u64)> {
        use windows_sys::Win32::Foundation::FILETIME;
        use windows_sys::Win32::System::Threading::GetSystemTimes;

        fn value(t: FILETIME) -> u64 {
            ((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64
        }

        // 返回 (空闲 100ns, 总忙 100ns)；Windows 的内核时间已包含空闲时间
        unsafe {
            let (mut idle, mut kernel, mut user): (FILETIME, FILETIME, FILETIME) =
                (std::mem::zeroed(), std::mem::zeroed(), std::mem::zeroed());
            if GetSystemTimes(&mut idle, &mut kernel, &mut user) == 0 {
                return None;
            }
            Some((value(idle), value(kernel) + value(user)))
        }
    };

    #[cfg(target_os = "linux")]
    let sample = || -> Option<(u64, u64)> {
        let text = std::fs::read_to_string("/proc/stat").ok()?;
        parse_proc_stat_first_cpu(&text)
    };

    #[cfg(not(any(windows, target_os = "linux")))]
    let sample = || -> Option<(u64, u64)> { None };

    let mut slot = CPU_LAST.lock().unwrap();
    if let Some((prev_idle, prev_total, prev_rate)) = *slot {
        let (idle, total) = sample()?;
        let rate = cpu_rate_from_delta((prev_idle, prev_total), (idle, total)).unwrap_or(prev_rate);
        *slot = Some((idle, total, rate));
        return Some(rate);
    }

    // 首次调用：以 200ms 双采样建立基线，后续按请求间隔差分
    let first = sample()?;
    std::thread::sleep(Duration::from_millis(200));
    let second = sample()?;
    let rate = cpu_rate_from_delta(first, second).unwrap_or(0.0);
    *slot = Some((second.0, second.1, rate));
    Some(rate)
}

/// 系统负载（1/5/15 分钟平均值）。Windows 无此概念，返回 `None`。
#[cfg(target_os = "linux")]
pub fn load_average() -> Option<(f64, f64, f64)> {
    let text = std::fs::read_to_string("/proc/loadavg").ok()?;
    let mut it = text.split_whitespace();
    let l1 = it.next()?.parse().ok()?;
    let l5 = it.next()?.parse().ok()?;
    let l15 = it.next()?.parse().ok()?;
    Some((l1, l5, l15))
}

/// 系统负载（1/5/15 分钟平均值）。Windows 无此概念，返回 `None`。
#[cfg(not(target_os = "linux"))]
pub fn load_average() -> Option<(f64, f64, f64)> {
    None
}

/// 是否存在指定进程名的进程（大小写不敏感、忽略 `.exe` 后缀；看门狗用）。
pub fn is_process_running(name: &str) -> bool {
    let name = name.trim().trim_end_matches(".exe");
    if name.is_empty() {
        return false;
    }

    #[cfg(windows)]
    {
        let mut found = false;
        for_each_process(|pname, _pid, _threads| {
            if pname.eq_ignore_ascii_case(name) {
                found = true;
            }
        });
        found
    }

    #[cfg(target_os = "linux")]
    {
        let Ok(dir) = std::fs::read_dir("/proc") else {
            return false;
        };
        for entry in dir.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Ok(comm) = std::fs::read_to_string(path.join("comm")) else {
                continue;
            };
            if comm.trim().eq_ignore_ascii_case(name) {
                return true;
            }
        }
        false
    }

    #[cfg(not(any(windows, target_os = "linux")))]
    {
        false
    }
}

/// 按进程名查找 PID（名称不区分大小写、可带或不带 `.exe`）。
///
/// 用途：IIS 的 w3wp 归属判定（再按命令行 `-ap` 匹配应用池），
/// 避免依赖 `appcmd list wps` 的「空结果也返回非 0」语义。
pub fn pids_by_name(name: &str) -> Vec<u32> {
    let want = name.trim().trim_end_matches(".exe");
    #[allow(unused_mut)]
    let mut out: Vec<u32> = Vec::new();
    if want.is_empty() {
        return out;
    }
    #[cfg(windows)]
    {
        for_each_process(|pname, pid, _threads| {
            if pname.trim_end_matches(".exe").eq_ignore_ascii_case(want) {
                out.push(pid);
            }
        });
    }
    #[cfg(target_os = "linux")]
    {
        if let Ok(dir) = std::fs::read_dir("/proc") {
            for entry in dir.flatten() {
                let path = entry.path();
                let Some(file) = path.file_name().and_then(|s| s.to_str()) else {
                    continue;
                };
                let Ok(pid) = file.parse::<u32>() else {
                    continue;
                };
                if let Ok(comm) = std::fs::read_to_string(path.join("comm")) {
                    if comm.trim().eq_ignore_ascii_case(want) {
                        out.push(pid);
                    }
                }
            }
        }
    }
    out
}

/// 上一次 CPU 采样（用于差分计算瞬时 CPU%）。
struct CpuSnapshot {
    at: Instant,
    cpu: HashMap<u32, f64>,
    /// 上次算出的百分比（Δt 过短时直接复用，避免连续调用全部归零）
    percent: HashMap<u32, f32>,
}

/// 进程 CPU 采样基线（进程表每次遍历结束后更新）。
static LAST_CPU: Mutex<Option<CpuSnapshot>> = Mutex::new(None);

/// Top 进程列表（按内存或瞬时 CPU 降序；取前 `count` 个）。
///
/// - `cpu_percent` 由**两次调用的累计 CPU 时间差分**得到（`Δt < 200ms` 时不计算，
///   首次调用全为 0），口径为「占单核百分比」，可 > 100（与 htop / 云监控一致）；
/// - 只有返回的前 `count` 个进程才补全重字段（命令行 / 路径 / 工作目录 / 句柄），
///   避免每次遍历都读全量 `/proc/{pid}/cmdline` 等小文件；
/// - 平台取不到的重字段留空（Windows 无 PEB 读取，`command` 用 exe 路径、`cwd` 空）。
pub fn top_processes(count: usize, sort_by_cpu: bool) -> Vec<ProcItem> {
    let mut items = collect_light();
    update_cpu_percent(&mut items);
    if sort_by_cpu {
        items.sort_by(|a, b| {
            b.cpu_percent
                .total_cmp(&a.cpu_percent)
                .then(b.cpu_seconds.total_cmp(&a.cpu_seconds))
        });
    } else {
        items.sort_by_key(|p| std::cmp::Reverse(p.memory_mb));
    }
    items.truncate(count);
    finalize(&mut items);
    items
}

/// 「CPU Top N ∪ 内存 Top N」——**只扫一遍系统**。
///
/// 面板同时需要两个榜单（前端可切换排序）：分别调两次 [`top_processes`] 会把
/// 进程表快照与逐进程查询（Windows `OpenProcess` / Linux `/proc` 小文件）做两遍。
/// 本函数收集一次、排序两次、取并集，并只给并集补重字段与监听端口。
///
/// 返回顺序：CPU 榜在前，其余为内存榜补充项（前端自行再排序）。
pub fn top_processes_union(cpu_count: usize, mem_count: usize) -> Vec<ProcItem> {
    let mut items = collect_light();
    update_cpu_percent(&mut items);

    // 用索引排序：两榜共用同一份数据，不复制 ProcItem
    let mut idx_cpu: Vec<usize> = (0..items.len()).collect();
    idx_cpu.sort_by(|a, b| {
        let (x, y) = (&items[*a], &items[*b]);
        y.cpu_percent
            .total_cmp(&x.cpu_percent)
            .then(y.cpu_seconds.total_cmp(&x.cpu_seconds))
    });
    idx_cpu.truncate(cpu_count);

    let mut idx_mem: Vec<usize> = (0..items.len()).collect();
    idx_mem.sort_by(|a, b| items[*b].memory_mb.cmp(&items[*a].memory_mb));
    idx_mem.truncate(mem_count);

    let mut seen: HashMap<u32, ()> = HashMap::with_capacity(idx_cpu.len() + idx_mem.len());
    let mut picked: Vec<usize> = Vec::with_capacity(idx_cpu.len() + idx_mem.len());
    for i in idx_cpu.into_iter().chain(idx_mem) {
        if seen.insert(items[i].pid, ()).is_none() {
            picked.push(i);
        }
    }

    let mut out: Vec<ProcItem> = picked.into_iter().map(|i| items[i].clone()).collect();
    finalize(&mut out);
    out
}

/// 收集全系统进程的**轻字段**（名称 / PID / 内存 / 线程 / 累计 CPU / 启动时间）。
#[allow(unused_mut)]
fn collect_light() -> Vec<ProcItem> {
    let mut items: Vec<ProcItem> = Vec::new();

    #[cfg(windows)]
    {
        for_each_process(|pname, pid, threads| {
            items.push(ProcItem {
                name: pname.to_string(),
                pid,
                memory_mb: super::process::memory_mb(pid).unwrap_or(0),
                threads,
                cpu_seconds: process_cpu_split(pid)
                    .map(|(total, _, _)| total)
                    .unwrap_or(0.0),
                cpu_percent: 0.0,
                user: String::new(),
                exe: String::new(),
                command: String::new(),
                cwd: String::new(),
                handles: 0,
                start_time: 0,
                ports: Vec::new(),
            });
        });
    }

    #[cfg(target_os = "linux")]
    {
        use std::sync::OnceLock;
        static PAGE_SIZE: OnceLock<u64> = OnceLock::new();
        let page = *PAGE_SIZE.get_or_init(|| {
            let mut size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
            if size <= 0 {
                size = 4096;
            }
            size as u64
        });
        let hz = clock_ticks();
        let boot = linux_boot_time();

        if let Ok(dir) = std::fs::read_dir("/proc") {
            for entry in dir.flatten() {
                let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
                    continue;
                };
                // /proc/{pid}/stat：comm 位于括号内，其后 utime/stime/num_threads/starttime
                // 为第 14/15/20/22 字段（close 后索引 11/12/17/19）
                let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
                    continue;
                };
                let Some(open) = stat.find('(') else { continue };
                let Some(close) = stat.rfind(')') else {
                    continue;
                };
                let name = stat[open + 1..close].to_string();
                let rest: Vec<&str> = stat[close + 1..].split_whitespace().collect();
                let utime: u64 = rest.get(11).and_then(|v| v.parse().ok()).unwrap_or(0);
                let stime: u64 = rest.get(12).and_then(|v| v.parse().ok()).unwrap_or(0);
                let threads: u32 = rest.get(17).and_then(|v| v.parse().ok()).unwrap_or(0);
                let start_ticks: i64 = rest.get(19).and_then(|v| v.parse().ok()).unwrap_or(0);

                let memory_mb = std::fs::read_to_string(format!("/proc/{pid}/statm"))
                    .ok()
                    .and_then(|t| t.split_whitespace().nth(1)?.parse::<u64>().ok())
                    .map(|pages| pages * page / 1024 / 1024)
                    .unwrap_or(0);

                items.push(ProcItem {
                    name,
                    pid,
                    memory_mb,
                    threads,
                    cpu_seconds: (utime + stime) as f64 / 100.0,
                    cpu_percent: 0.0,
                    user: String::new(),
                    exe: String::new(),
                    command: String::new(),
                    cwd: String::new(),
                    handles: 0,
                    start_time: if boot > 0 && start_ticks > 0 {
                        boot + start_ticks / hz.max(1)
                    } else {
                        0
                    },
                    ports: Vec::new(),
                });
            }
        }
    }

    items
}

/// 瞬时 CPU%：与上次**有效**采样做差分。
///
/// 关键约束（2026-10-20 修复「CPU 全是 0」）：间隔不足 200ms 的调用**复用上次结果，
/// 且不推进采样基线**。否则任何高频调用方（面板一次刷新会打多个接口）都会把基线
/// 一直顶到 now，导致每次差分窗口都 < 200ms → 永远算不出真实值，甚至把已经为 0 的
/// 结果自我复用锁死在 0。
fn update_cpu_percent(items: &mut [ProcItem]) {
    let now = Instant::now();
    let mut slot = LAST_CPU.lock().unwrap();
    let prev = slot.take();
    match prev {
        None => {
            // 首次：只建立基线（首帧 CPU% 为 0）
            *slot = Some(CpuSnapshot {
                at: now,
                cpu: items.iter().map(|i| (i.pid, i.cpu_seconds)).collect(),
                percent: items.iter().map(|i| (i.pid, i.cpu_percent)).collect(),
            });
        }
        Some(prev) => {
            let dt = now.duration_since(prev.at).as_secs_f64();
            if dt >= 0.2 {
                for item in items.iter_mut() {
                    if let Some(&before) = prev.cpu.get(&item.pid) {
                        let delta = (item.cpu_seconds - before).max(0.0);
                        // 占单核百分比：可超过 100（多线程进程）
                        item.cpu_percent = (delta / dt * 100.0).min(100.0 * 64.0) as f32;
                    }
                }
                // 只有真正算过，才推进基线
                *slot = Some(CpuSnapshot {
                    at: now,
                    cpu: items.iter().map(|i| (i.pid, i.cpu_seconds)).collect(),
                    percent: items.iter().map(|i| (i.pid, i.cpu_percent)).collect(),
                });
            } else {
                // 间隔太短（同一次刷新内的第二个接口）：复用上次结果，
                // 基线**原样放回**，让下一次 >=200ms 的调用仍有完整窗口
                for item in items.iter_mut() {
                    if let Some(&percent) = prev.percent.get(&item.pid) {
                        item.cpu_percent = percent;
                    }
                }
                *slot = Some(prev);
            }
        }
    }
}

/// 只给榜单里的进程补重字段（命令行 / 路径 / 工作目录 / 句柄 / 监听端口）。
///
/// 端口只针对这批进程查（不遍历全系统），且快照自带 2 秒缓存。
fn finalize(items: &mut [ProcItem]) {
    let pids: Vec<u32> = items.iter().map(|i| i.pid).collect();
    let ports = listen_ports_many(&pids);
    for item in items.iter_mut() {
        fill_details(item);
        if let Some(list) = ports.get(&item.pid) {
            item.ports = list.clone();
        }
    }
}

// ───────────────────────────── 监听端口 ─────────────────────────────

/// 端口快照有效期：面板 3 秒刷新一次，2 秒内复用刚扫过的结果。
const PORTS_TTL: Duration = Duration::from_secs(2);

/// 端口快照缓存槽：`(采样时间, 已扫描的 pid 列表（升序）, pid → 端口)`。
type PortsCacheSlot = Option<(Instant, Vec<u32>, HashMap<u32, Vec<u16>>)>;

/// 上一次端口快照。
static PORTS_CACHE: Mutex<PortsCacheSlot> = Mutex::new(None);

/// 指定这批进程的 TCP 监听端口（升序去重）。**只扫传入的 pid**，不遍历全系统。
///
/// - Linux：读 `/proc/net/tcp{,6}` 得 LISTEN 的 `socket inode → 端口`，
///   再只为这批 pid 读 `/proc/{pid}/fd` 反查 socket inode；
/// - Windows：`GetExtendedTcpTable(TCP_TABLE_OWNER_PID_ALL)` 一次取全表
///   （系统调用本就只需一次），再按 pid 过滤，IPv4 + IPv6 都涵盖；
/// - 结果缓存 [`PORTS_TTL`]：缓存已覆盖请求的 pid 时直接复用（进程表刷新会
///   连续两次调用：CPU Top 与内存 Top）。
///
/// 只统计**监听**（LISTEN）端口，不含出站连接 —— 与应用「对外提供什么服务」的口径一致。
pub fn listen_ports_many(pids: &[u32]) -> HashMap<u32, Vec<u16>> {
    if pids.is_empty() {
        return HashMap::new();
    }
    let mut wanted = pids.to_vec();
    wanted.sort_unstable();
    wanted.dedup();
    {
        let slot = PORTS_CACHE.lock().unwrap();
        if let Some((at, scanned, map)) = slot.as_ref() {
            if at.elapsed() < PORTS_TTL && wanted.iter().all(|p| scanned.binary_search(p).is_ok()) {
                return map.clone();
            }
        }
    }
    let map = listen_ports_many_impl(&wanted);
    if let Ok(mut slot) = PORTS_CACHE.lock() {
        *slot = Some((Instant::now(), wanted, map.clone()));
    }
    map
}

/// 指定进程（含其子进程树）正在监听的 TCP 端口（升序去重）。
///
/// 用于「未配置 `HealthCheck`」时的自动探活：由上层找出子服务的监听口，再发起探活请求。
/// 子进程树的意义：应用常由包装脚本 / 服务宿主再 fork 出真正监听者
/// （`dotnet` → 子进程、`java -jar` → `sh` 包装）。取不到（无监听 / 权限不足）返回空。
pub fn listen_ports_of(pid: u32) -> Vec<u16> {
    let tree = process_tree(pid);
    let map = listen_ports_many(&tree);
    let mut ports = Vec::new();
    for p in tree {
        if let Some(list) = map.get(&p) {
            ports.extend(list.iter().copied());
        }
    }
    ports.sort_unstable();
    ports.dedup();
    ports
}

/// 进程自身 + 全部后代 PID（Linux 读 `/proc/{pid}/stat` 的 ppid 建树；其它平台只有自身）。
#[cfg(target_os = "linux")]
fn process_tree(pid: u32) -> Vec<u32> {
    use std::collections::{HashMap, HashSet};

    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    if let Ok(dir) = std::fs::read_dir("/proc") {
        for entry in dir.flatten() {
            let Ok(p) = entry.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            let Ok(stat) = std::fs::read_to_string(format!("/proc/{p}/stat")) else {
                continue;
            };
            let Some(close) = stat.rfind(')') else {
                continue;
            };
            let rest: Vec<&str> = stat[close + 1..].split_whitespace().collect();
            if let Some(ppid) = rest.get(1).and_then(|v| v.parse::<u32>().ok()) {
                children.entry(ppid).or_default().push(p);
            }
        }
    }

    let mut pending = vec![pid];
    let mut seen: HashSet<u32> = HashSet::new();
    let mut pids = Vec::new();
    while let Some(p) = pending.pop() {
        if !seen.insert(p) {
            continue;
        }
        pids.push(p);
        if let Some(list) = children.get(&p) {
            pending.extend(list.iter().copied());
        }
    }
    pids
}

#[cfg(not(target_os = "linux"))]
fn process_tree(pid: u32) -> Vec<u32> {
    vec![pid]
}

/// Linux：读 `/proc/net/tcp{,6}` 的 LISTEN 行得 `socket inode → 端口`，
/// 再**只为传入的 pid** 读 `/proc/{pid}/fd` 反查归属（不遍历全系统）。
#[cfg(target_os = "linux")]
fn listen_ports_many_impl(pids: &[u32]) -> HashMap<u32, Vec<u16>> {
    // 1) socket inode → 监听端口（/proc/net/tcp 第 2 列地址含端口，第 4 列状态 0A=LISTEN，
    //    第 10 列是 socket inode）
    let mut inode_port: HashMap<u64, u16> = HashMap::new();
    for path in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        for line in text.lines().skip(1) {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() < 10 || cols[3] != "0A" {
                continue; // 仅 TCP LISTEN
            }
            let Ok(port) = u16::from_str_radix(cols[1].rsplit(':').next().unwrap_or("0"), 16)
            else {
                continue;
            };
            let Ok(inode) = cols[9].parse::<u64>() else {
                continue;
            };
            if port != 0 {
                inode_port.insert(inode, port);
            }
        }
    }
    if inode_port.is_empty() {
        return HashMap::new();
    }

    // 2) 只看这批进程打开的 fd 里有没有 socket:[inode]（命中即归属该进程）
    let mut map: HashMap<u32, Vec<u16>> = HashMap::new();
    for &pid in pids {
        let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            let Ok(target) = std::fs::read_link(fd.path()) else {
                continue;
            };
            let Some(text) = target.to_str() else {
                continue;
            };
            let Some(inode) = text
                .strip_prefix("socket:[")
                .and_then(|v| v.strip_suffix(']'))
                .and_then(|v| v.parse::<u64>().ok())
            else {
                continue;
            };
            if let Some(&port) = inode_port.get(&inode) {
                map.entry(pid).or_default().push(port);
            }
        }
    }
    for list in map.values_mut() {
        list.sort_unstable();
        list.dedup();
    }
    map
}

/// Windows：`GetExtendedTcpTable` 取一次全表（系统调用本就只需一次），再按 pid 过滤，
/// IPv4 + IPv6 都涵盖。
#[cfg(windows)]
fn listen_ports_many_impl(pids: &[u32]) -> HashMap<u32, Vec<u16>> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, MIB_TCP6TABLE_OWNER_PID, MIB_TCPTABLE_OWNER_PID,
        TCP_TABLE_OWNER_PID_ALL,
    };
    use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6};

    let mut map: HashMap<u32, Vec<u16>> = HashMap::new();

    // 通用取表：返回 (缓冲区, 行数)
    fn fetch(family: u32) -> Option<(Vec<u8>, usize)> {
        unsafe {
            let mut size: u32 = 0;
            GetExtendedTcpTable(
                std::ptr::null_mut(),
                &mut size,
                0,
                family,
                TCP_TABLE_OWNER_PID_ALL,
                0,
            );
            if size == 0 {
                return None;
            }
            let mut buf = vec![0u8; size as usize];
            let ret = GetExtendedTcpTable(
                buf.as_mut_ptr() as *mut core::ffi::c_void,
                &mut size,
                0,
                family,
                TCP_TABLE_OWNER_PID_ALL,
                0,
            );
            if ret != 0 {
                return None;
            }
            let count = *(buf.as_ptr() as *const u32) as usize;
            Some((buf, count))
        }
    }

    unsafe {
        if let Some((buf, count)) = fetch(AF_INET as u32) {
            let table = buf.as_ptr() as *const MIB_TCPTABLE_OWNER_PID;
            let rows = (*table).table.as_ptr();
            for i in 0..count {
                let row = rows.add(i);
                if (*row).dwState != 2 {
                    continue; // MIB_TCP_STATE_LISTEN
                }
                let port = (((*row).dwLocalPort & 0xffff) as u16).swap_bytes();
                let pid = (*row).dwOwningPid;
                if port != 0 && pids.binary_search(&pid).is_ok() {
                    map.entry(pid).or_default().push(port);
                }
            }
        }

        if let Some((buf, count)) = fetch(AF_INET6 as u32) {
            let table = buf.as_ptr() as *const MIB_TCP6TABLE_OWNER_PID;
            let rows = (*table).table.as_ptr();
            for i in 0..count {
                let row = rows.add(i);
                if (*row).dwState != 2 {
                    continue;
                }
                let port = (((*row).dwLocalPort & 0xffff) as u16).swap_bytes();
                let pid = (*row).dwOwningPid;
                if port != 0 && pids.binary_search(&pid).is_ok() {
                    map.entry(pid).or_default().push(port);
                }
            }
        }
    }

    for list in map.values_mut() {
        list.sort_unstable();
        list.dedup();
    }
    map
}

#[cfg(not(any(windows, target_os = "linux")))]
fn listen_ports_many_impl(_pids: &[u32]) -> HashMap<u32, Vec<u16>> {
    HashMap::new()
}

/// 进程重字段（平台实现见下方）。
/// Windows：exe 路径 + 句柄数 + 启动时间（命令行/工作目录需要读 PEB，未实现）。
#[cfg(windows)]
fn fill_details(item: &mut ProcItem) {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Threading::{
        GetProcessHandleCount, GetProcessTimes, OpenProcess, QueryFullProcessImageNameW,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };

    item.user = windows_process_user(item.pid);

    fn filetime_secs(t: FILETIME) -> i64 {
        let ticks = ((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64;
        if ticks == 0 {
            0
        } else {
            (ticks / 10_000_000) as i64 - 11_644_473_600
        }
    }

    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, item.pid);
        if h.is_null() {
            return;
        }
        let mut buf = [0u16; 1024];
        let mut size = buf.len() as u32;
        if QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut size) != 0 {
            let path = String::from_utf16_lossy(&buf[..size as usize]);
            if item.command.is_empty() {
                item.command = path.clone();
            }
            item.exe = path;
        }
        let mut handles = 0u32;
        if GetProcessHandleCount(h, &mut handles) != 0 {
            item.handles = handles;
        }
        let (mut creation, mut exit, mut kernel, mut user): (
            FILETIME,
            FILETIME,
            FILETIME,
            FILETIME,
        ) = (
            std::mem::zeroed(),
            std::mem::zeroed(),
            std::mem::zeroed(),
            std::mem::zeroed(),
        );
        if GetProcessTimes(h, &mut creation, &mut exit, &mut kernel, &mut user) != 0 {
            item.start_time = filetime_secs(creation);
        }
        CloseHandle(h);
    }
}

/// Linux：用户 / 命令行 / 可执行文件 / 工作目录 / 打开文件数（`/proc`）。
#[cfg(target_os = "linux")]
fn fill_details(item: &mut ProcItem) {
    let pid = item.pid;
    item.user = linux_user_of(pid);
    if let Ok(path) = std::fs::read_link(format!("/proc/{pid}/exe")) {
        item.exe = path.to_string_lossy().into_owned();
    }
    if let Ok(path) = std::fs::read_link(format!("/proc/{pid}/cwd")) {
        item.cwd = path.to_string_lossy().into_owned();
    }
    if let Ok(bytes) = std::fs::read(format!("/proc/{pid}/cmdline")) {
        let text = String::from_utf8_lossy(&bytes).replace('\0', " ");
        item.command = text.trim().to_string();
    }
    if item.command.is_empty() {
        item.command = item.exe.clone();
    }
    item.handles = std::fs::read_dir(format!("/proc/{pid}/fd"))
        .map(|d| d.count() as u32)
        .unwrap_or(0);
}

#[cfg(not(any(windows, target_os = "linux")))]
fn fill_details(_item: &mut ProcItem) {}

/// Windows：进程运行账户名（`OpenProcessToken` + `LookupAccountSidW`；失败返回空）。
#[cfg(windows)]
fn windows_process_user(pid: u32) -> String {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{
        GetTokenInformation, LookupAccountSidW, TokenUser, TOKEN_QUERY, TOKEN_USER,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return String::new();
        }
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(h, TOKEN_QUERY, &mut token) == 0 {
            CloseHandle(h);
            return String::new();
        }
        let mut size = 0u32;
        GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut size);
        let mut buffer = vec![0u8; size.max(64) as usize];
        let mut name = String::new();
        if GetTokenInformation(
            token,
            TokenUser,
            buffer.as_mut_ptr() as *mut core::ffi::c_void,
            size,
            &mut size,
        ) != 0
        {
            let user = &*(buffer.as_ptr() as *const TOKEN_USER);
            let mut name_buf = [0u16; 256];
            let mut name_len = name_buf.len() as u32;
            let mut domain_buf = [0u16; 256];
            let mut domain_len = domain_buf.len() as u32;
            let mut use_type = 0i32;
            if LookupAccountSidW(
                std::ptr::null_mut(),
                user.User.Sid,
                name_buf.as_mut_ptr(),
                &mut name_len,
                domain_buf.as_mut_ptr(),
                &mut domain_len,
                &mut use_type,
            ) != 0
            {
                name = String::from_utf16_lossy(&name_buf[..name_len as usize]);
            }
        }
        CloseHandle(token);
        CloseHandle(h);
        name
    }
}

/// Linux：`/proc/{pid}/status` 的 Uid → 用户名（`/etc/passwd` 缓存）。
#[cfg(target_os = "linux")]
fn linux_user_of(pid: u32) -> String {
    use std::sync::OnceLock;
    static USERS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

    let Some(uid) = std::fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()
        .and_then(|text| {
            text.lines()
                .find(|line| line.starts_with("Uid:"))
                .and_then(|line| line.split_whitespace().nth(1))
                .map(str::to_string)
        })
    else {
        return String::new();
    };
    let cache = USERS.get_or_init(|| {
        let mut map = HashMap::new();
        if let Ok(passwd) = std::fs::read_to_string("/etc/passwd") {
            for line in passwd.lines() {
                let cols: Vec<&str> = line.split(':').collect();
                if cols.len() >= 3 {
                    map.insert(cols[2].to_string(), cols[0].to_string());
                }
            }
        }
        Mutex::new(map)
    });
    let guard = cache.lock().unwrap();
    guard.get(&uid).cloned().unwrap_or(uid)
}

/// Linux：开机墙钟（`/proc/stat` 的 `btime`，Unix 秒；0 = 未知）。
#[cfg(target_os = "linux")]
fn linux_boot_time() -> i64 {
    use std::sync::OnceLock;
    static BTIME: OnceLock<i64> = OnceLock::new();
    *BTIME.get_or_init(|| {
        std::fs::read_to_string("/proc/stat")
            .ok()
            .and_then(|text| {
                text.lines()
                    .find(|line| line.starts_with("btime"))
                    .and_then(|line| line.split_whitespace().nth(1))
                    .and_then(|value| value.parse::<i64>().ok())
            })
            .unwrap_or(0)
    })
}

/// Linux：每秒时钟滴答数（`_SC_CLK_TCK`，通常 100）。
#[cfg(target_os = "linux")]
fn clock_ticks() -> i64 {
    use std::sync::OnceLock;
    static HZ: OnceLock<i64> = OnceLock::new();
    *HZ.get_or_init(|| {
        let value = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        if value > 0 {
            value as i64
        } else {
            100
        }
    })
}

/// 系统 TCP 连接计数（已建立 / TIME_WAIT / CLOSE_WAIT）。
pub fn tcp_counts() -> (u32, u32, u32) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::NetworkManagement::IpHelper::{
            GetExtendedTcpTable, MIB_TCPROW_OWNER_PID, MIB_TCPTABLE_OWNER_PID,
            TCP_TABLE_OWNER_PID_ALL,
        };
        use windows_sys::Win32::Networking::WinSock::AF_INET;

        // MIB_TCP_STATE：5=ESTABLISHED 8=CLOSE_WAIT 11=TIME_WAIT（与 .NET TcpState 数值一致）
        const ESTABLISHED: u32 = 5;
        const CLOSE_WAIT: u32 = 8;
        const TIME_WAIT: u32 = 11;

        unsafe {
            let mut size: u32 = 0;
            GetExtendedTcpTable(
                std::ptr::null_mut(),
                &mut size,
                0,
                AF_INET as u32,
                TCP_TABLE_OWNER_PID_ALL,
                0,
            );
            if size == 0 {
                return (0, 0, 0);
            }

            let mut buf = vec![0u8; size as usize];
            let ret = GetExtendedTcpTable(
                buf.as_mut_ptr() as *mut core::ffi::c_void,
                &mut size,
                0,
                AF_INET as u32,
                TCP_TABLE_OWNER_PID_ALL,
                0,
            );
            if ret != 0 {
                return (0, 0, 0);
            }

            let table = buf.as_ptr() as *const MIB_TCPTABLE_OWNER_PID;
            let count = (*table).dwNumEntries as usize;
            let rows = (*table).table.as_ptr();
            let (mut estab, mut close_wait, mut time_wait) = (0u32, 0u32, 0u32);
            for i in 0..count {
                let row: *const MIB_TCPROW_OWNER_PID = rows.add(i);
                match (*row).dwState {
                    ESTABLISHED => estab += 1,
                    CLOSE_WAIT => close_wait += 1,
                    TIME_WAIT => time_wait += 1,
                    _ => {}
                }
            }
            (estab, time_wait, close_wait)
        }
    }

    #[cfg(target_os = "linux")]
    {
        // /proc/net/tcp：st 列（hex）：01=ESTABLISHED 06=TIME_WAIT 08=CLOSE_WAIT
        let Ok(text) = std::fs::read_to_string("/proc/net/tcp") else {
            return (0, 0, 0);
        };
        let (mut estab, mut time_wait, mut close_wait) = (0u32, 0u32, 0u32);
        // 只取第 4 列（状态码）做字节比较：连接多时避免每行两次堆分配（Vec + 大写 String）
        for line in text.lines().skip(1) {
            let Some(state) = line.split_whitespace().nth(3) else {
                continue;
            };
            match state.as_bytes() {
                b"01" => estab += 1,
                b"06" => time_wait += 1,
                b"08" => close_wait += 1,
                _ => {}
            }
        }
        (estab, time_wait, close_wait)
    }

    #[cfg(not(any(windows, target_os = "linux")))]
    {
        (0, 0, 0)
    }
}

/// 当前进程 CPU 时间（总秒、内核秒、用户秒）。
pub fn process_cpu_seconds() -> (f64, f64, f64) {
    process_cpu_split(std::process::id()).unwrap_or((0.0, 0.0, 0.0))
}

/// 进程 CPU 占用百分比（占整机口径：CPU 时间 / 经过时间 / 逻辑核数 × 100，钳制 0~100）。
///
/// `elapsed_secs` ≤ 0 或 `cores` 为 0 时返回 0（Pek.RAgent 采样器与 Pek.RPanlServer
/// 概览页共用；2026-10-07 自两处重复实现下沉）。
pub fn process_cpu_percent(cpu_seconds: f64, elapsed_secs: f64, cores: usize) -> f64 {
    if elapsed_secs <= 0.0 || cores == 0 {
        return 0.0;
    }
    ((cpu_seconds / elapsed_secs) / cores as f64 * 100.0).clamp(0.0, 100.0)
}

/// 进程 CPU 采样器（请求间差分：取两次调用的间隔为采样窗口；面板关闭时零开销）。
///
/// 使用：进程内放一个 `static` 实例，每次请求/采样周期调用一次：
/// ```ignore
/// static METER: ProcessCpuMeter = ProcessCpuMeter::new();
/// let rate = METER.sample_since(std::time::Instant::now()); // 首帧为“平均占用”，之后为窗口差值
/// ```
pub struct ProcessCpuMeter {
    last: std::sync::Mutex<Option<(std::time::Instant, f64)>>,
}

impl Default for ProcessCpuMeter {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessCpuMeter {
    /// 创建空采样器（进程内静态量场景用 `const` 构造）。
    pub const fn new() -> Self {
        Self {
            last: std::sync::Mutex::new(None),
        }
    }

    /// 采样一次（窗口 = 与上次采样的间隔）。
    ///
    /// - 首帧（无基线）返回 `None`；
    /// - 间隔 < 50ms 时返回 `None`（窗口过小无意义），但仍刷新基线。
    pub fn sample(&self) -> Option<f64> {
        let total = process_cpu_seconds().0;
        let now = std::time::Instant::now();
        let mut slot = self.last.lock().ok()?;
        let rate = match *slot {
            Some((t0, c0)) => {
                let dt = now.duration_since(t0).as_secs_f64();
                if dt >= 0.05 {
                    Some(process_cpu_percent((total - c0).max(0.0), dt, cpu_count()))
                } else {
                    None
                }
            }
            None => None,
        };
        *slot = Some((now, total));
        rate
    }

    /// 采样一次；无基线（首帧）时以 `start` 为起点计算平均占用（窗口至少按 0.5 秒，
    /// 便于启动初期也能给出近似值——Pek.RAgent 采样器语义）。始终返回数值。
    pub fn sample_since(&self, start: std::time::Instant) -> f64 {
        let total = process_cpu_seconds().0;
        let now = std::time::Instant::now();
        let cores = cpu_count();
        let mut slot = self.last.lock().unwrap_or_else(|e| e.into_inner());
        let rate = match *slot {
            Some((t0, c0)) => process_cpu_percent(
                (total - c0).max(0.0),
                now.duration_since(t0).as_secs_f64(),
                cores,
            ),
            None => process_cpu_percent(
                total,
                now.duration_since(start).as_secs_f64().max(0.5),
                cores,
            ),
        };
        *slot = Some((now, total));
        rate
    }
}

/// 逻辑核数（获取失败按 1）。
fn cpu_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

#[cfg(test)]
mod process_cpu_meter_tests {
    use super::*;

    #[test]
    fn process_cpu_percent_clamps_to_range() {
        assert_eq!(process_cpu_percent(0.5, 1.0, 4), 12.5);
        assert_eq!(process_cpu_percent(-1.0, 1.0, 4), 0.0);
        assert_eq!(
            process_cpu_percent(100.0, 1.0, 4),
            100.0,
            "整机口径钳制到 100"
        );
        assert_eq!(process_cpu_percent(0.5, 0.0, 4), 0.0, "零窗口返回 0");
        assert_eq!(process_cpu_percent(0.5, 1.0, 0), 0.0, "零核数返回 0");
    }

    #[test]
    fn meter_first_frame_none_then_value() {
        let meter = ProcessCpuMeter::new();
        assert!(meter.sample().is_none(), "首帧无基线");
        // 忙等约 60ms 制造窗口（保证 ≥50ms）
        let t = std::time::Instant::now();
        while t.elapsed().as_millis() < 60 {}
        let rate = meter.sample().expect("次帧应有值");
        assert!((0.0..=100.0).contains(&rate), "{rate}");
    }

    #[test]
    fn meter_sample_since_first_frame_value() {
        let meter = ProcessCpuMeter::new();
        let start = std::time::Instant::now();
        let rate = meter.sample_since(start);
        assert!((0.0..=100.0).contains(&rate), "{rate}");
    }
}

/// 指定进程 CPU 时间（总秒、内核秒、用户秒）。
#[cfg(windows)]
pub fn process_cpu_split(pid: u32) -> Option<(f64, f64, f64)> {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    fn value(t: FILETIME) -> f64 {
        (((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64) as f64 / 10_000_000.0
    }

    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return None;
        }
        let (mut creation, mut exit, mut kernel, mut user): (
            FILETIME,
            FILETIME,
            FILETIME,
            FILETIME,
        ) = (
            std::mem::zeroed(),
            std::mem::zeroed(),
            std::mem::zeroed(),
            std::mem::zeroed(),
        );
        let ok = GetProcessTimes(h, &mut creation, &mut exit, &mut kernel, &mut user);
        CloseHandle(h);
        if ok == 0 {
            return None;
        }
        let kernel = value(kernel);
        let user = value(user);
        Some((kernel + user, kernel, user))
    }
}

/// 指定进程 CPU 时间（总秒、内核秒、用户秒）。
#[cfg(target_os = "linux")]
pub fn process_cpu_split(pid: u32) -> Option<(f64, f64, f64)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = stat.rfind(')')?;
    let rest: Vec<&str> = stat[close + 1..].split_whitespace().collect();
    let utime: u64 = rest.get(11).and_then(|v| v.parse().ok()).unwrap_or(0);
    let stime: u64 = rest.get(12).and_then(|v| v.parse().ok()).unwrap_or(0);
    let user = utime as f64 / 100.0;
    let kernel = stime as f64 / 100.0;
    Some((user + kernel, kernel, user))
}

/// 指定进程 CPU 时间（总秒、内核秒、用户秒）。
#[cfg(not(any(windows, target_os = "linux")))]
pub fn process_cpu_split(_pid: u32) -> Option<(f64, f64, f64)> {
    None
}

/// 遍历当前所有进程（名称、PID、线程数）。名称已去除 `.exe` 后缀。
#[cfg(windows)]
fn for_each_process(mut f: impl FnMut(&str, u32, u32)) {
    use std::mem::zeroed;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE || snapshot.is_null() {
            return;
        }

        let mut entry: PROCESSENTRY32W = zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                let end = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                let raw = String::from_utf16_lossy(&entry.szExeFile[..end]);
                let name = raw.trim_end_matches(".exe");
                if !name.is_empty() {
                    f(name, entry.th32ProcessID, entry.cntThreads);
                }
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
    }
}

/// 磁盘 IO 累计统计（读/写完成次数与字节数）。
#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub struct DiskIo {
    /// 读完成次数
    pub reads: u64,
    /// 写完成次数
    pub writes: u64,
    /// 读字节数
    pub read_bytes: u64,
    /// 写字节数
    pub write_bytes: u64,
    /// IO 累计耗时（毫秒；Linux 为 diskstats 的 ms 字段，Windows 由 100ns 换算）
    pub ms_total: u64,
}

/// 解析 `/proc/diskstats`，汇总“整盘”（不做分区/映射层重复计数）的读写统计。
///
/// 规则：次设备号 `% 16 == 0` 视为整盘（兼容 sda/sdb/vda 多块盘；分区均为非 0）；
/// 跳过 `dm-`（LVM）与 `md`（软 RAID）映射，其 IO 已体现在底层物理盘。
#[cfg(any(target_os = "linux", test))]
pub fn parse_diskstats(text: &str) -> DiskIo {
    let mut io = DiskIo::default();
    for line in text.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 11 {
            continue;
        }
        let name = cols[2];
        if name.starts_with("dm-") || name.starts_with("md") {
            continue;
        }
        let minor = cols[1].parse::<u64>().unwrap_or(1);
        if minor % 16 != 0 {
            continue;
        }
        io.reads += cols[3].parse::<u64>().unwrap_or(0);
        io.read_bytes += cols[5].parse::<u64>().unwrap_or(0) * 512; // 扇区 = 512 字节
        io.writes += cols[7].parse::<u64>().unwrap_or(0);
        io.write_bytes += cols[9].parse::<u64>().unwrap_or(0) * 512;
        io.ms_total += cols[6].parse::<u64>().unwrap_or(0) + cols[10].parse::<u64>().unwrap_or(0);
    }
    io
}

/// 磁盘 IO 累计统计。Web 面板差分计算 IOPS 与读/写速率用。
/// Linux 读 `/proc/diskstats`；Windows 汇总物理磁盘（`IOCTL_DISK_PERFORMANCE`）。
pub fn disk_io() -> Option<DiskIo> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/diskstats")
            .ok()
            .map(|t| parse_diskstats(&t))
    }

    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        };
        use windows_sys::Win32::System::IO::DeviceIoControl;

        /// `DISK_PERFORMANCE`（winioctl.h）头部：仅取读写统计所需字段，`_rest` 保持布局。
        #[repr(C)]
        #[derive(Default, Clone, Copy)]
        struct DiskPerformance {
            bytes_read: i64,
            bytes_written: i64,
            read_time: i64,
            write_time: i64,
            idle_time: i64,
            read_count: u32,
            write_count: u32,
            queue_depth: u32,
            split_count: u32,
            _rest: [u8; 32],
        }

        const IOCTL_DISK_PERFORMANCE: u32 = 0x0007_0020;

        let mut io = DiskIo::default();
        let mut available = false;
        for index in 0..16u32 {
            let path = format!("\\\\.\\PhysicalDrive{index}");
            let path_w: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
            let handle = unsafe {
                CreateFileW(
                    path_w.as_ptr(),
                    0,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    0,
                    std::ptr::null_mut(),
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                continue;
            }

            let mut perf = DiskPerformance::default();
            let mut returned = 0u32;
            let ok = unsafe {
                DeviceIoControl(
                    handle,
                    IOCTL_DISK_PERFORMANCE,
                    std::ptr::null(),
                    0,
                    &mut perf as *mut DiskPerformance as *mut core::ffi::c_void,
                    std::mem::size_of::<DiskPerformance>() as u32,
                    &mut returned,
                    std::ptr::null_mut(),
                )
            };
            unsafe { CloseHandle(handle) };

            if ok != 0 {
                available = true;
                io.reads += perf.read_count as u64;
                io.writes += perf.write_count as u64;
                io.read_bytes += perf.bytes_read.max(0) as u64;
                io.write_bytes += perf.bytes_written.max(0) as u64;
                // 时间字段单位为 100ns（换算为毫秒）
                io.ms_total += ((perf.read_time.max(0) + perf.write_time.max(0)) as u64) / 10_000;
            }
        }

        if available {
            Some(io)
        } else {
            None
        }
    }

    #[cfg(not(any(target_os = "linux", windows)))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_proc_stat_first_cpu_matches_psutil() {
        // 10 字段（含 guest/guest_nice）：guest 已计入 user/nice，总时须扣除
        let text = "cpu  100 20 30 400 50 5 5 0 15 5\ncpu0 10 2 3 40 5 0 0 0 1 0\n";
        let (idle, total) = parse_proc_stat_first_cpu(text).unwrap();
        assert_eq!(idle, 400 + 50, "空转=idle+iowait");
        // 全部字段和 630，扣除 guest(15)+guest_nice(5)
        assert_eq!(total, 630 - 20);
        // 仅 4 字段的旧内核
        let legacy = "cpu  1 2 3 4\n";
        assert_eq!(parse_proc_stat_first_cpu(legacy), Some((4, 10)));
    }

    #[test]
    fn cpu_rate_from_delta_matches_psutil_formula() {
        // Δ总 100、Δ(idle+iowait) 20 → 80%
        assert_eq!(cpu_rate_from_delta((10, 100), (30, 200)), Some(80.0));
        // 字段回退（负增量）按 0 处理 → 全忙
        assert_eq!(cpu_rate_from_delta((50, 100), (40, 150)), Some(100.0));
        // 窗口内无 tick 变化：沿用上次速率
        assert_eq!(cpu_rate_from_delta((0, 100), (0, 100)), None);
    }

    #[test]
    fn parse_diskstats_skips_partitions_and_mappers() {
        let sample = "\
   8       0 sda 100 0 1000 250 200 0 2000 500 0 0 0
   8       1 sda1 50 0 500 100 60 0 600 200 0 0 0
   8      16 sdb 7 0 70 5 8 0 80 6 0 0 0
 259       0 nvme0n1 10 0 100 30 20 0 200 40 0 0 0
 253       0 dm-0 90 0 900 300 180 0 1800 600 0 0 0
 252       0 md0 5 0 50 10 5 0 50 20 0 0 0
";
        let io = parse_diskstats(sample);
        // 次数：sda(100+200) + sdb(7+8) + nvme0n1(10+20)；分区/映射层跳过
        assert_eq!(io.reads + io.writes, 345);
        // 字节 = 扇区×512：(1000+70+100)、（2000+80+200）
        assert_eq!(io.read_bytes, 1170 * 512);
        assert_eq!(io.write_bytes, 2280 * 512);
        // IO 耗时毫秒：sda(250+500) + sdb(5+6) + nvme0n1(30+40)
        assert_eq!(io.ms_total, 831);
    }

    #[test]
    fn load_average_matches_platform() {
        #[cfg(target_os = "linux")]
        assert!(load_average().is_some(), "Linux 应提供负载数据");
        #[cfg(not(target_os = "linux"))]
        assert!(load_average().is_none(), "非 Linux 平台无负载数据");
    }

    #[test]
    fn current_process_cpu_and_stats_are_readable() {
        // 当前进程的 CPU 时间与线程数应可读（值不敏感，只验证不 panic/不误报 0 线程）
        let (total, kernel, user) = process_cpu_seconds();
        assert!(total >= 0.0 && kernel >= 0.0 && user >= 0.0);
        let pid = std::process::id();
        if let Some((threads, _handles)) = process_stats(pid) {
            assert!(threads >= 1, "至少应有一个线程");
        }
    }

    #[test]
    fn short_interval_call_does_not_advance_cpu_baseline() {
        // 面板一次刷新会打多个接口；短间隔调用若推进基线，
        // 差分窗口就永远 < 200ms → CPU% 恒为 0（2026-10-20 实测踩到）
        *LAST_CPU.lock().unwrap() = None;
        let _ = top_processes(5, true); // 建基线
        std::thread::sleep(Duration::from_millis(300));
        let _ = top_processes(5, true); // dt≈300ms → 计算并推进基线
        let at_before = LAST_CPU.lock().unwrap().as_ref().map(|s| s.at);
        let _ = top_processes(5, true); // dt≈0 → 复用，不得推进
        let at_after = LAST_CPU.lock().unwrap().as_ref().map(|s| s.at);
        assert_eq!(at_before, at_after, "短间隔调用不应推进 CPU 采样基线");
    }

    #[test]
    fn listen_ports_many_only_reports_requested_pids() {
        // 空列表不做事；只返回请求过的 pid（保护「不遍历全系统」的约束）
        assert!(listen_ports_many(&[]).is_empty());
        let map = listen_ports_many(&[std::process::id()]);
        assert!(
            map.keys().all(|p| *p == std::process::id()),
            "只应返回请求过的 pid"
        );
    }

    /// 手动基准：`cargo test bench_process_top_with_ports -- --ignored --nocapture`
    ///
    /// 目标：面板每 3 秒刷新一次，`top_processes(20)` ×2（CPU / 内存）+ 端口扫描
    /// 必须保持在毫秒级，避免 RAgent 自己成为资源消耗源。
    #[test]
    #[ignore]
    fn bench_process_top_with_ports() {
        let round = |label: &str, f: &dyn Fn() -> u64| {
            let t = Instant::now();
            let mut n = 0u64;
            for _ in 0..10 {
                n += f();
            }
            let per = t.elapsed().as_secs_f64() * 100.0;
            println!("{label}: {n} 项 / 10 次，单次 ≈ {per:.2} ms");
        };
        round("top_processes(20, cpu)", &|| {
            top_processes(20, true).len() as u64
        });
        round("top_processes(20, mem)", &|| {
            top_processes(20, false).len() as u64
        });
        round("top_processes_union(20,20)", &|| {
            top_processes_union(20, 20).len() as u64
        });
        round("listen_ports_many(TOP20)", &|| {
            let pids: Vec<u32> = top_processes(20, false).iter().map(|i| i.pid).collect();
            listen_ports_many(&pids).len() as u64
        });

        // CPU% 正确性冒烟：上面的高频调用不得把基线顶到 now，
        // 隔 1.2 秒再取一次，应能算出一批非 0 的进程（面板刷新同理）
        std::thread::sleep(Duration::from_millis(1_200));
        let after = top_processes_union(20, 20);
        let top: Vec<(String, f32)> = after
            .iter()
            .take(5)
            .map(|p| (p.name.clone(), p.cpu_percent))
            .collect();
        println!("CPU% top5 after 1.2s: {top:?}");
        assert!(
            after.iter().any(|p| p.cpu_percent > 0.0),
            "间隔足够长时应有进程 CPU% 非 0（否则差分基线被错误推进）"
        );
    }
}
