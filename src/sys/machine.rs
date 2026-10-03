//! 机器事实与设置（跨平台）：主机/系统/CPU/内存/网络接口/磁盘/机器标识/系统校时。
//!
//! 由 Pek.RAgent 下沉（2026-10-03 第二批）：`-ShowMachineInfo`、Web 面板“本机详情”
//! 与机器标识（注册/心跳）共用的底层采集，口径对齐 C# `ShowMachineInfo` / .NET。
//!
//! 口径说明：
//! - 内存“可用”对齐 psutil/宝塔：`free + buffers + cached(+SReclaimable)`（宽松口径；
//!   数值失真时退化为纯 free），因此 `已用 = 总 − 可用` 与宝塔一致；
//! - Windows 系统描述与 `dhrust::logs` 同源（RtlGetVersion）。

#[cfg(target_os = "macos")]
use super::process::run_capture;

/// 机器唯一标识（Windows 注册表 `MachineGuid`；Linux `/etc/machine-id`）。
pub fn machine_guid() -> Option<String> {
    static CACHE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    CACHE.get_or_init(detect_machine_guid).clone()
}

/// 检测机器唯一标识（进程生命周期内不变，结果缓存）。
fn detect_machine_guid() -> Option<String> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::NO_ERROR;
        use windows_sys::Win32::System::Registry::{
            HKEY, HKEY_LOCAL_MACHINE, KEY_READ, RegCloseKey, RegOpenKeyExW, RegQueryValueExW,
        };

        fn wide(text: &str) -> Vec<u16> {
            text.encode_utf16().chain(std::iter::once(0)).collect()
        }

        let sub = wide("SOFTWARE\\Microsoft\\Cryptography");
        let value = wide("MachineGuid");
        unsafe {
            let mut hkey: HKEY = std::ptr::null_mut();
            if RegOpenKeyExW(HKEY_LOCAL_MACHINE, sub.as_ptr(), 0, KEY_READ, &mut hkey) != NO_ERROR {
                return None;
            }

            let mut size = 0u32;
            let mut kind = 0u32;
            let mut guid = None;
            if RegQueryValueExW(
                hkey,
                value.as_ptr(),
                std::ptr::null(),
                &mut kind,
                std::ptr::null_mut(),
                &mut size,
            ) == NO_ERROR
                && size > 2
            {
                let mut buf = vec![0u8; size as usize];
                if RegQueryValueExW(
                    hkey,
                    value.as_ptr(),
                    std::ptr::null(),
                    &mut kind,
                    buf.as_mut_ptr(),
                    &mut size,
                ) == NO_ERROR
                {
                    let wide_text: &[u16] =
                        std::slice::from_raw_parts(buf.as_ptr() as *const u16, size as usize / 2);
                    let end = wide_text
                        .iter()
                        .position(|&c| c == 0)
                        .unwrap_or(wide_text.len());
                    let text = String::from_utf16_lossy(&wide_text[..end]);
                    if !text.trim().is_empty() {
                        guid = Some(text.trim().to_string());
                    }
                }
            }
            RegCloseKey(hkey);
            guid
        }
    }

    #[cfg(target_os = "linux")]
    {
        let text = std::fs::read_to_string("/etc/machine-id").ok()?;
        let text = text.trim();
        if text.is_empty() {
            None
        } else {
            Some(text.to_string())
        }
    }

    #[cfg(not(any(windows, target_os = "linux")))]
    {
        None
    }
}

/// 系统运行时长（秒）。
pub fn host_uptime_seconds() -> u64 {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::SystemInformation::GetTickCount64;
        unsafe { GetTickCount64() / 1000 }
    }

    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/uptime")
            .ok()
            .and_then(|t| t.split_whitespace().next()?.parse::<f64>().ok())
            .map(|v| v as u64)
            .unwrap_or(0)
    }

    #[cfg(not(any(windows, target_os = "linux")))]
    {
        0
    }
}

/// 拆分毫秒时间戳为（整秒、纳秒）；负数按欧几里得取整（用于 Unix `timespec`）。
pub fn split_epoch_ms(epoch_ms: i64) -> (i64, u32) {
    (
        epoch_ms.div_euclid(1000),
        (epoch_ms.rem_euclid(1000) * 1_000_000) as u32,
    )
}

/// 设置系统 UTC 时间（毫秒时间戳；Web 面板“同步时间”按钮，以浏览器时间为准）。
///
/// 只校正时钟、不改时区；需要相应权限：
/// - Unix：root（`clock_settime(CLOCK_REALTIME)`，非 root 返回明确提示）；
/// - Windows：服务账户/管理员（`SetSystemTime` 需要 `SeSystemtimePrivilege`，此处临时启用）。
pub fn set_system_time(epoch_ms: i64) -> Result<(), String> {
    #[cfg(unix)]
    {
        let (secs, nanos) = split_epoch_ms(epoch_ms);
        // 64 位目标上 `time_t` 恒为 i64（= c_long）：直接赋 i64，
        // 避免引用 musl 目标上已被标记弃用的 `libc::time_t` 别名
        let ts = libc::timespec {
            tv_sec: secs,
            tv_nsec: nanos as libc::c_long,
        };
        let rc = unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &ts) };
        if rc != 0 {
            let error = std::io::Error::last_os_error();
            return Err(match error.raw_os_error() {
                Some(libc::EPERM) => {
                    "权限不足：同步系统时间需要 root（请以 root 运行代理）".to_string()
                }
                Some(libc::EINVAL) => "时间超出内核允许范围".to_string(),
                _ => format!("设置系统时间失败：{error}"),
            });
        }
        Ok(())
    }

    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::HANDLE;
        use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_NOT_ALL_ASSIGNED};
        use windows_sys::Win32::Security::{
            AdjustTokenPrivileges, LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED,
            TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
        };
        use windows_sys::Win32::System::SystemInformation::SetSystemTime;
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

        // SetSystemTime 依赖 SE_SYSTEMTIME_NAME 特权（管理员/系统账户持有但默认禁用）
        unsafe {
            let mut token: HANDLE = std::ptr::null_mut();
            if OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
                &mut token,
            ) == 0
            {
                return Err(format!(
                    "打开进程令牌失败：{}",
                    std::io::Error::last_os_error()
                ));
            }

            let privilege: Vec<u16> = "SeSystemtimePrivilege\0".encode_utf16().collect();
            let mut luid = windows_sys::Win32::Foundation::LUID {
                LowPart: 0,
                HighPart: 0,
            };
            let looked_up =
                LookupPrivilegeValueW(std::ptr::null(), privilege.as_ptr(), &mut luid) != 0;

            let mut state = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [windows_sys::Win32::Security::LUID_AND_ATTRIBUTES {
                    Luid: luid,
                    Attributes: SE_PRIVILEGE_ENABLED,
                }],
            };
            let adjusted = looked_up
                && AdjustTokenPrivileges(
                    token,
                    0,
                    &mut state,
                    0,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                ) != 0;
            // AdjustTokenPrivileges 即便返回成功，也可能因未持有特权而什么都未启用
            let last = GetLastError();
            CloseHandle(token);

            if !adjusted || last == ERROR_NOT_ALL_ASSIGNED {
                return Err("权限不足：同步系统时间需要管理员/服务账户权限".to_string());
            }
        }

        let Some(system_time) = epoch_ms_to_systemtime(epoch_ms) else {
            return Err("时间戳超出可表示范围".to_string());
        };
        if unsafe { SetSystemTime(&system_time) } == 0 {
            return Err(format!(
                "设置系统时间失败：{}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = epoch_ms;
        Err("当前平台不支持同步系统时间".to_string())
    }
}

/// 毫秒时间戳 → UTC `SYSTEMTIME`（Windows `SetSystemTime` 入参）。
#[cfg(windows)]
fn epoch_ms_to_systemtime(epoch_ms: i64) -> Option<windows_sys::Win32::Foundation::SYSTEMTIME> {
    use chrono::{Datelike, Timelike};
    use windows_sys::Win32::Foundation::SYSTEMTIME;

    let (secs, nanos) = split_epoch_ms(epoch_ms);
    let dt = chrono::DateTime::from_timestamp(secs, nanos)?;
    Some(SYSTEMTIME {
        wYear: dt.year() as u16,
        wMonth: dt.month() as u16,
        wDayOfWeek: dt.weekday().num_days_from_sunday() as u16,
        wDay: dt.day() as u16,
        wHour: dt.hour() as u16,
        wMinute: dt.minute() as u16,
        wSecond: dt.second() as u16,
        wMilliseconds: dt.timestamp_subsec_millis() as u16,
    })
}

/// 主机名。
///
/// 进程生命周期内视为不变，缓存避免每次面板请求重复读取环境变量 / `/etc/hostname`。
pub fn hostname() -> String {
    static CACHE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    CACHE.get_or_init(detect_hostname).clone()
}

fn detect_hostname() -> String {
    #[cfg(windows)]
    {
        std::env::var("COMPUTERNAME").unwrap_or_default()
    }

    #[cfg(not(windows))]
    {
        if let Ok(name) = std::env::var("HOSTNAME") {
            if !name.trim().is_empty() {
                return name.trim().to_string();
            }
        }
        std::fs::read_to_string("/etc/hostname")
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    }
}

/// 当前用户名。
pub fn user_name() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_default()
}

/// 操作系统描述（Windows 形如 `Microsoft Windows NT 10.0.26200.0`，与 .NET `OSDescription` 一致）。
///
/// 进程生命周期内不变，缓存避免每次面板请求重复读 `/etc/os-release` / 调用 RtlGetVersion。
pub fn os_description() -> String {
    static CACHE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    CACHE.get_or_init(detect_os_description).clone()
}

fn detect_os_description() -> String {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::SystemInformation::OSVERSIONINFOW;

        #[link(name = "ntdll")]
        unsafe extern "system" {
            fn RtlGetVersion(version_info: *mut OSVERSIONINFOW) -> i32;
        }

        unsafe {
            let mut info: OSVERSIONINFOW = std::mem::zeroed();
            info.dwOSVersionInfoSize = std::mem::size_of::<OSVERSIONINFOW>() as u32;
            if RtlGetVersion(&mut info) == 0 {
                return format!(
                    "Microsoft Windows NT {}.{}.{}.0",
                    info.dwMajorVersion, info.dwMinorVersion, info.dwBuildNumber
                );
            }
        }
        "Windows".to_string()
    }

    #[cfg(target_os = "linux")]
    {
        if let Ok(text) = std::fs::read_to_string("/etc/os-release") {
            for line in text.lines() {
                if let Some(value) = line.strip_prefix("PRETTY_NAME=") {
                    return value.trim().trim_matches('"').to_string();
                }
            }
        }
        "Linux".to_string()
    }

    #[cfg(target_os = "macos")]
    {
        "macOS".to_string()
    }

    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        std::env::consts::OS.to_string()
    }
}

/// CPU 型号（Windows 读注册表 ProcessorNameString；Linux 读 /proc/cpuinfo；macOS 读 sysctl）。
///
/// 型号在进程生命周期内不变，缓存避免每次面板请求重复读注册表 / 解析 `/proc/cpuinfo`。
pub fn cpu_model() -> Option<String> {
    static CACHE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    CACHE.get_or_init(detect_cpu_model).clone()
}

fn detect_cpu_model() -> Option<String> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::NO_ERROR;
        use windows_sys::Win32::System::Registry::{
            HKEY, HKEY_LOCAL_MACHINE, KEY_READ, RegCloseKey, RegOpenKeyExW, RegQueryValueExW,
        };

        fn wide(text: &str) -> Vec<u16> {
            text.encode_utf16().chain(std::iter::once(0)).collect()
        }

        let sub = wide("HARDWARE\\DESCRIPTION\\System\\CentralProcessor\\0");
        let value = wide("ProcessorNameString");
        unsafe {
            let mut hkey: HKEY = std::ptr::null_mut();
            if RegOpenKeyExW(HKEY_LOCAL_MACHINE, sub.as_ptr(), 0, KEY_READ, &mut hkey) != NO_ERROR {
                return None;
            }

            let mut size = 0u32;
            let mut kind = 0u32;
            let mut model = None;
            if RegQueryValueExW(
                hkey,
                value.as_ptr(),
                std::ptr::null(),
                &mut kind,
                std::ptr::null_mut(),
                &mut size,
            ) == NO_ERROR
                && size > 2
            {
                let mut buf = vec![0u8; size as usize];
                if RegQueryValueExW(
                    hkey,
                    value.as_ptr(),
                    std::ptr::null(),
                    &mut kind,
                    buf.as_mut_ptr(),
                    &mut size,
                ) == NO_ERROR
                {
                    let wide_text: &[u16] =
                        std::slice::from_raw_parts(buf.as_ptr() as *const u16, size as usize / 2);
                    let end = wide_text
                        .iter()
                        .position(|&c| c == 0)
                        .unwrap_or(wide_text.len());
                    let text = String::from_utf16_lossy(&wide_text[..end]);
                    if !text.trim().is_empty() {
                        model = Some(text.trim().to_string());
                    }
                }
            }
            RegCloseKey(hkey);
            model
        }
    }

    #[cfg(target_os = "linux")]
    {
        let text = std::fs::read_to_string("/proc/cpuinfo").ok()?;
        for line in text.lines() {
            for key in ["model name", "Hardware", "Model"] {
                if let Some(rest) = line.strip_prefix(key) {
                    if let Some((_, value)) = rest.split_once(':') {
                        let value = value.trim();
                        if !value.is_empty() {
                            return Some(value.to_string());
                        }
                    }
                }
            }
        }
        None
    }

    #[cfg(target_os = "macos")]
    {
        run_capture("sysctl", &["-n", "machdep.cpu.brand_string"])
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }
}

/// 解析 `/proc/meminfo`，返回 `(总内存, MemFree, Buffers, Cached + SReclaimable)`（字节）。
///
/// `cached` 口径对齐 psutil/宝塔与 `free` 命令：`Cached` 与 `SReclaimable` 相加。
#[cfg(any(target_os = "linux", test))]
pub fn parse_meminfo_bytes(text: &str) -> Option<(u64, u64, u64, u64)> {
    let mut total = None;
    let mut free = None;
    let mut buffers = None;
    let mut cached = None;
    let mut sreclaimable = 0u64;
    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let Some(kb) = rest
            .split_whitespace()
            .next()
            .and_then(|v| v.parse::<u64>().ok())
        else {
            continue;
        };
        match key {
            "MemTotal" => total = Some(kb * 1024),
            "MemFree" => free = Some(kb * 1024),
            "Buffers" => buffers = Some(kb * 1024),
            "Cached" => cached = Some(kb * 1024),
            "SReclaimable" => sreclaimable = kb * 1024,
            _ => {}
        }
    }
    let total = total?;
    let free = free?;
    let buffers = buffers.unwrap_or(0);
    let cached = cached.unwrap_or(0) + sreclaimable;
    Some((total, free, buffers, cached))
}

/// 物理内存（总量、可用；字节）。
pub fn memory_info() -> Option<(u64, u64)> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        unsafe {
            let mut status: MEMORYSTATUSEX = std::mem::zeroed();
            status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
            if GlobalMemoryStatusEx(&mut status) != 0 {
                return Some((status.ullTotalPhys, status.ullAvailPhys));
            }
        }
        None
    }

    #[cfg(target_os = "linux")]
    {
        let text = std::fs::read_to_string("/proc/meminfo").ok()?;
        let (total, free, buffers, cached) = parse_meminfo_bytes(&text)?;
        // 对齐宝塔/psutil 口径：已用 = 总 - MemFree - Buffers - Cached - SReclaimable，
        // 即“可用”按宽松口径计算（缓存视为可回收）
        let mut avail = free + buffers + cached;
        if avail > total {
            // 容器等场景数值失真时退化为纯空闲（psutil 同处理）
            avail = free;
        }
        Some((total, avail))
    }

    #[cfg(target_os = "macos")]
    {
        let text = run_capture("sysctl", &["-n", "hw.memsize"])?;
        let bytes: u64 = text.trim().parse().ok()?;
        Some((bytes, 0))
    }
}

/// MAC 地址格式（`xx-xx-xx-xx-xx-xx`）。
#[cfg_attr(not(windows), allow(dead_code))]
fn format_mac(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join("-")
}

/// 排除的虚拟/过滤类网卡关键字（对齐 C# `ShowMachineInfo._Excludes`）。
#[cfg_attr(not(windows), allow(dead_code))]
const VIRTUAL_ADAPTER_EXCLUDES: [&str; 14] = [
    "Loopback",
    "VMware",
    "VBox",
    "Virtual",
    "Teredo",
    "Tunnel",
    "VPN",
    "VNIC",
    "IEEE",
    "Filter",
    "Npcap",
    "QoS",
    "Miniport",
    "Kernel Debug",
];

/// 是否为虚拟/过滤类网卡（按描述匹配，忽略大小写）。
#[cfg_attr(not(windows), allow(dead_code))]
fn is_virtual_adapter(description: &str) -> bool {
    let lower = description.to_ascii_lowercase();
    VIRTUAL_ADAPTER_EXCLUDES
        .iter()
        .any(|e| lower.contains(&e.to_ascii_lowercase()))
}

/// 网络接口信息。
pub struct NetInterface {
    /// 接口名称（中文系统为“以太网/WLAN”等；Linux 为 eth0 等）
    pub name: String,
    /// 描述（网卡型号；Linux 通常为空）
    pub description: String,
    /// 是否已连接
    pub up: bool,
    /// 链路速率（Mbps；0 表示未知）
    pub speed_mbps: u64,
    /// MAC 地址（`xx-xx-xx-xx-xx-xx`）
    pub mac: String,
    /// IPv4 地址列表
    pub ips: Vec<String>,
    /// 网关列表
    pub gateways: Vec<String>,
    /// DNS 服务器列表
    pub dns: Vec<String>,
    /// 累计接收字节数（自系统启动；0 表示未知）
    pub bytes_received: u64,
    /// 累计发送字节数（自系统启动；0 表示未知）
    pub bytes_sent: u64,
}

/// 枚举网络接口（排除回环/虚拟网卡；Windows 含网关/DNS/速率，Linux 含累计收发）。
pub fn network_interfaces() -> Vec<NetInterface> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, NO_ERROR};
        use windows_sys::Win32::NetworkManagement::IpHelper::{
            FreeMibTable, GAA_FLAG_INCLUDE_GATEWAYS, GetAdaptersAddresses, GetIfTable2,
            IP_ADAPTER_ADDRESSES_LH, MIB_IF_TABLE2,
        };
        use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_UNSPEC, SOCKADDR, SOCKADDR_IN};

        /// 从 `SOCKET_ADDRESS` 提取 IPv4（非 IPv4 返回 None）。
        fn sockaddr_ipv4(addr: *const SOCKADDR) -> Option<String> {
            unsafe {
                if addr.is_null() || (*addr).sa_family != AF_INET {
                    return None;
                }
                let sin = &*(addr as *const SOCKADDR_IN);
                let b = sin.sin_addr.S_un.S_addr.to_ne_bytes();
                Some(format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3]))
            }
        }

        /// 读取 PWSTR（UTF-16）到 String。
        fn pwstr(ptr: *const u16) -> String {
            unsafe {
                if ptr.is_null() {
                    return String::new();
                }
                let mut len = 0usize;
                while *ptr.add(len) != 0 && len < 512 {
                    len += 1;
                }
                String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len))
            }
        }

        // 每网卡累计收发（MIB 接口行按 LUID 匹配；对齐 C# `GetIPv4Statistics()`）
        let mut luid_stats: Vec<(u64, u64, u64)> = Vec::new(); // (LUID, 接收字节, 发送字节)
        unsafe {
            let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
            if GetIfTable2(&mut table) == 0 && !table.is_null() {
                let t = &*table;
                for i in 0..t.NumEntries as usize {
                    let row = &*t.Table.as_ptr().add(i);
                    // 过滤软件回环（IfType=24）
                    if row.Type == 24 {
                        continue;
                    }
                    // 计数器不可用时为 u64::MAX
                    let rx = if row.InOctets == u64::MAX { 0 } else { row.InOctets };
                    let tx = if row.OutOctets == u64::MAX { 0 } else { row.OutOctets };
                    luid_stats.push((row.InterfaceLuid.Value, rx, tx));
                }
                FreeMibTable(table as *const _);
            }
        }

        let mut out = Vec::new();
        unsafe {
            let mut size: u32 = 16 * 1024;
            let mut buf: Vec<u8> = vec![0; size as usize];
            let mut ret = GetAdaptersAddresses(
                AF_UNSPEC as u32,
                GAA_FLAG_INCLUDE_GATEWAYS,
                std::ptr::null(),
                buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH,
                &mut size,
            );
            if ret == ERROR_BUFFER_OVERFLOW {
                buf = vec![0; size as usize];
                ret = GetAdaptersAddresses(
                    AF_UNSPEC as u32,
                    GAA_FLAG_INCLUDE_GATEWAYS,
                    std::ptr::null(),
                    buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH,
                    &mut size,
                );
            }
            if ret != NO_ERROR {
                return out;
            }

            let mut adapter = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
            while !adapter.is_null() {
                let a = &*adapter;
                adapter = a.Next;

                // 过滤：软件回环（IfType=24）/ 隧道（131）与虚拟网卡（对齐 C# 排除表）
                let description = pwstr(a.Description);
                if a.IfType == 24 || a.IfType == 131 || is_virtual_adapter(&description) {
                    continue;
                }

                let mut ips = Vec::new();
                let mut ua = a.FirstUnicastAddress;
                while !ua.is_null() {
                    if let Some(ip) = sockaddr_ipv4((*ua).Address.lpSockaddr) {
                        if !ips.contains(&ip) {
                            ips.push(ip);
                        }
                    }
                    ua = (*ua).Next;
                }

                let mut gateways = Vec::new();
                let mut ga = a.FirstGatewayAddress;
                while !ga.is_null() {
                    if let Some(ip) = sockaddr_ipv4((*ga).Address.lpSockaddr) {
                        gateways.push(ip);
                    }
                    ga = (*ga).Next;
                }

                let mut dns = Vec::new();
                let mut da = a.FirstDnsServerAddress;
                while !da.is_null() {
                    if let Some(ip) = sockaddr_ipv4((*da).Address.lpSockaddr) {
                        dns.push(ip);
                    }
                    da = (*da).Next;
                }

                let mac_len = (a.PhysicalAddressLength as usize).min(a.PhysicalAddress.len());
                let luid = a.Luid.Value;
                let (bytes_received, bytes_sent) = if luid != 0 {
                    luid_stats
                        .iter()
                        .find(|(l, _, _)| *l == luid)
                        .map(|(_, rx, tx)| (*rx, *tx))
                        .unwrap_or((0, 0))
                } else {
                    (0, 0)
                };
                out.push(NetInterface {
                    name: pwstr(a.FriendlyName),
                    description,
                    up: a.OperStatus == 1, // IfOperStatusUp
                    speed_mbps: a.TransmitLinkSpeed / 1_000_000,
                    mac: format_mac(&a.PhysicalAddress[..mac_len]),
                    ips,
                    gateways,
                    dns,
                    bytes_received,
                    bytes_sent,
                });
            }
        }
        out
    }

    #[cfg(target_os = "linux")]
    {
        // 每网卡累计收发：/proc/net/dev 单文件源（任一接口缺失不影响其他接口）
        let dev_entries = std::fs::read_to_string("/proc/net/dev")
            .map(|t| super::net::parse_net_dev_entries(&t))
            .unwrap_or_default();
        let mut out = Vec::new();
        let Ok(dir) = std::fs::read_dir("/sys/class/net") else {
            return out;
        };
        for entry in dir.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name == "lo" {
                continue;
            }
            let base = entry.path();
            let read = |file: &str| {
                std::fs::read_to_string(base.join(file))
                    .map(|s| s.trim().to_string())
                    .ok()
            };
            let mac = read("address").unwrap_or_default();
            if mac.is_empty() || mac == "00:00:00:00:00:00" {
                continue; // 虚拟接口常见全零 MAC
            }
            let (bytes_received, bytes_sent) = dev_entries
                .iter()
                .find(|(n, _, _)| n == &name)
                .map(|(_, rx, tx)| (*rx, *tx))
                .unwrap_or((0, 0));
            out.push(NetInterface {
                name,
                description: String::new(),
                up: read("operstate").as_deref() == Some("up"),
                speed_mbps: read("speed")
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(0),
                mac: mac.to_uppercase().replace(':', "-"),
                ips: Vec::new(),
                gateways: Vec::new(),
                dns: Vec::new(),
                bytes_received,
                bytes_sent,
            });
        }
        out
    }

    #[cfg(target_os = "macos")]
    {
        Vec::new() // macOS 待实机补充（需 ifconfig/netstat 解析）
    }
}

/// 磁盘信息。
pub struct DiskItem {
    /// 盘符（`C:\`）或挂载点
    pub name: String,
    /// 类型（固定/可移动/网络/光驱等）
    pub kind: String,
    /// 文件系统（NTFS/ext4 等）
    pub format: String,
    /// 卷标
    pub label: String,
    /// 总字节（未就绪为 0）
    pub total: u64,
    /// 可用字节
    pub free: u64,
    /// 是否就绪（光驱无盘等）
    pub ready: bool,
}

/// 枚举磁盘（对齐 C# `ShowMachineInfo`：全量枚举并标注类型）。
pub fn disks() -> Vec<DiskItem> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Storage::FileSystem::{
            GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
        };

        fn utf16z(buf: &[u16]) -> String {
            let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            String::from_utf16_lossy(&buf[..end])
        }

        let mut out = Vec::new();
        let mask = unsafe { GetLogicalDrives() };
        for i in 0..26u32 {
            if mask & (1 << i) == 0 {
                continue;
            }
            let letter = (b'A' + i as u8) as char;
            let root = format!("{letter}:\\");
            let root_w: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();

            let kind = match unsafe { GetDriveTypeW(root_w.as_ptr()) } {
                2 => "可移动",
                3 => "固定",
                4 => "网络",
                5 => "光驱",
                6 => "内存盘",
                _ => "其它",
            };

            let mut label_buf = [0u16; 128];
            let mut fs_buf = [0u16; 32];
            let ok = unsafe {
                GetVolumeInformationW(
                    root_w.as_ptr(),
                    label_buf.as_mut_ptr(),
                    label_buf.len() as u32,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    fs_buf.as_mut_ptr(),
                    fs_buf.len() as u32,
                )
            } != 0;

            let mut free = 0u64;
            let mut total = 0u64;
            let mut total_free = 0u64;
            let got = unsafe {
                GetDiskFreeSpaceExW(root_w.as_ptr(), &mut free, &mut total, &mut total_free)
            } != 0;

            out.push(DiskItem {
                name: root,
                kind: kind.to_string(),
                format: if ok { utf16z(&fs_buf) } else { String::new() },
                label: if ok { utf16z(&label_buf) } else { String::new() },
                total: if got { total } else { 0 },
                free: if got { free } else { 0 },
                ready: ok,
            });
        }
        out
    }

    #[cfg(target_os = "linux")]
    {
        let mut out = Vec::new();
        let Ok(mounts) = std::fs::read_to_string("/proc/mounts") else {
            return out;
        };
        for line in mounts.lines() {
            let mut cols = line.split_whitespace();
            let (Some(dev), Some(mp), Some(fs)) = (cols.next(), cols.next(), cols.next()) else {
                continue;
            };
            if !dev.starts_with("/dev/") {
                continue;
            }
            // 八进制转义还原（\040 等）；过滤引导分区与系统虚拟文件系统（对齐 DHDeploy `IsTemporaryVolume`）
            let mount = super::disk::unescape_mount(mp);
            if super::disk::is_temporary_mount(&mount, Some(fs)) {
                continue;
            }
            let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
            let Ok(path) = std::ffi::CString::new(mount.as_str()) else {
                continue;
            };
            let ok = unsafe { libc::statvfs(path.as_ptr(), &mut stat) } == 0;
            let (total, free) = if ok {
                let block = stat.f_frsize as u64;
                (stat.f_blocks as u64 * block, stat.f_bavail as u64 * block)
            } else {
                (0, 0)
            };
            out.push(DiskItem {
                name: mount.clone(),
                kind: "固定".to_string(),
                format: fs.to_string(),
                label: String::new(),
                total,
                free,
                ready: ok,
            });
        }
        out
    }

    #[cfg(target_os = "macos")]
    {
        Vec::new() // macOS 待实机补充
    }
}

/// 全部就绪磁盘的用量列表：`(已用 MB, 总量 MB, 名称)`。
/// 过滤未就绪（光驱无盘等）与零容量项；Linux 尽量把根分区 `/` 排在最前。
pub fn disk_usages() -> Vec<(u64, u64, String)> {
    // Windows 下盘符已按 A-Z 升序，无需排序；`mut` 仅非 Windows 平台使用
    #[cfg_attr(windows, allow(unused_mut))]
    let mut list: Vec<DiskItem> = disks()
        .into_iter()
        .filter(|d| d.ready && d.total > 0)
        .collect();

    #[cfg(not(windows))]
    list.sort_by_key(|d| if d.name == "/" { 0 } else { 1 });

    list.into_iter()
        .map(|d| {
            (
                d.total.saturating_sub(d.free) / 1024 / 1024,
                d.total / 1024 / 1024,
                d.name,
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_meminfo_matches_baota_semantics() {
        let sample = "\
MemTotal:       16299544 kB
MemFree:          512000 kB
MemAvailable:    8192000 kB
Buffers:          102400 kB
Cached:          4096000 kB
SReclaimable:     204800 kB
Shmem:            100000 kB
";
        let (total, free, buffers, cached) = parse_meminfo_bytes(sample).unwrap();
        assert_eq!(total, 16299544 * 1024);
        assert_eq!(free, 512000 * 1024);
        assert_eq!(buffers, 102400 * 1024);
        // cached = Cached + SReclaimable（free 命令/psutil 口径）
        assert_eq!(cached, (4096000 + 204800) * 1024);
        // 宝塔 memRealUsed = 总 - MemFree - Buffers - Cached - SReclaimable
        let used = total - free - buffers - cached;
        assert_eq!(used, (16299544 - 512000 - 102400 - 4096000 - 204800) * 1024);
    }

    #[test]
    fn parse_meminfo_without_optional_fields() {
        let sample = "MemTotal:       1000 kB\nMemFree:         100 kB\n";
        let (total, free, buffers, cached) = parse_meminfo_bytes(sample).unwrap();
        assert_eq!((total, free, buffers, cached), (1000 * 1024, 100 * 1024, 0, 0));
        assert!(
            parse_meminfo_bytes("MemFree: 100 kB\n").is_none(),
            "缺 MemTotal 应失败"
        );
    }

    #[test]
    fn format_mac_shape() {
        assert_eq!(
            format_mac(&[0x8c, 0x32, 0x23, 0x17, 0x8d, 0x54]),
            "8c-32-23-17-8d-54"
        );
        assert_eq!(format_mac(&[]), "");
    }

    #[test]
    fn split_epoch_ms_handles_fractions_and_negatives() {
        assert_eq!(split_epoch_ms(946_730_096_789), (946_730_096, 789_000_000));
        assert_eq!(split_epoch_ms(0), (0, 0));
        assert_eq!(split_epoch_ms(-1), (-1, 999_000_000));
    }

    #[cfg(windows)]
    #[test]
    fn systemtime_conversion_uses_utc() {
        // 946 730 096 789 ms = 2000-01-01 12:34:56.789 UTC（周六）
        let st = epoch_ms_to_systemtime(946_730_096_789).unwrap();
        assert_eq!(
            (
                st.wYear,
                st.wMonth,
                st.wDayOfWeek,
                st.wDay,
                st.wHour,
                st.wMinute,
                st.wSecond,
                st.wMilliseconds
            ),
            (2000, 1, 6, 1, 12, 34, 56, 789)
        );
    }

    #[test]
    fn disk_usages_is_sane() {
        for (used, total, name) in disk_usages() {
            assert!(total > 0, "磁盘总量应为正");
            assert!(used <= total, "磁盘已用不应超过总量");
            assert!(!name.is_empty(), "磁盘名称不应为空");
        }
    }

    #[test]
    fn machine_facts_are_readable() {
        assert!(!os_description().is_empty());
        assert!(!hostname().is_empty());
        // 机器标识与 CPU 型号允许取不到（非常规平台），但不应 panic
        let _ = machine_guid();
        let _ = cpu_model();
        if let Some((total, _avail)) = memory_info() {
            assert!(total > 0);
        }
    }
}
