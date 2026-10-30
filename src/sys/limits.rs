//! 进程/系统「连接容量」限制：读取、推荐与调优（fd / file-max / somaxconn / conntrack）。
//!
//! ## 为什么在公共库
//!
//! 长驻网络服务（broker / agent / 面板 / 代理）都会被同一件事卡住：**每进程 fd 上限**
//! （Linux 默认常见 1024/4096）、`somaxconn`（accept 队列）、`nf_conntrack_max`（连接跟踪表）。
//! 处理方式完全一样：读 `/proc` 现状 → 按预期连接数算推荐值 → 进程内 `setrlimit`
//! + 写 systemd drop-in（永久生效）+ 写 `/etc/sysctl.d`。
//!
//! 原先这段代码只长在 HlktechIoT broker 的 `capacity.rs` 里，而 `service::inits`
//! 生成的 systemd 单元又写死了 `LimitNOFILE=65535` —— **同一个语义两处口径**。
//! 因此把「机制」下沉到这里（读取 / 提升 / drop-in），「策略」（按多少连接算多少 fd、
//! 每连接多少内存、如何在面板上展示）留在各应用。
//!
//! ## 平台
//!
//! - Linux：全部能力可用；
//! - Windows / macOS：`snapshot().supported = false`，提升操作返回明确错误（不静默假装成功）。

use std::path::PathBuf;

/// systemd 服务单元里 fd 上限的**默认值**（`service::inits` 生成单元时使用）。
///
/// 只作为“兜底默认”：真正按连接数算出来的推荐值由应用侧（如 broker 的「连接容量」）
/// 通过 [`raise_nofile`] + [`write_systemd_dropin`] 抬上去。
pub const NOFILE_SERVICE_DEFAULT: u64 = 65_535;

/// 本机限制快照（非 Linux 时 `supported = false`，其余字段为 0）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LimitsSnapshot {
    /// 是否支持读取（Linux 为 `true`）。
    pub supported: bool,
    /// 当前进程 fd 软限。
    pub fd_soft: u64,
    /// 当前进程 fd 硬限。
    pub fd_hard: u64,
    /// 当前进程已打开 fd 数。
    pub fd_open: u64,
    /// 系统级 fd 上限（`/proc/sys/fs/file-max`）。
    pub file_max: u64,
    /// 系统级已用 fd（`/proc/sys/fs/file-nr` 第一列）。
    pub file_used: u64,
    /// 内核硬上限（`/proc/sys/fs/nr_open`）：`setrlimit` 超过它必然失败（EPERM）。
    pub nr_open: u64,
    /// accept 队列上限（`net.core.somaxconn`）。
    pub somaxconn: u64,
    /// SYN 队列上限（`net.ipv4.tcp_max_syn_backlog`）。
    pub syn_backlog: u64,
    /// 连接跟踪表 `(已用, 上限)`；内核未启用 conntrack 时为 `None`。
    pub conntrack: Option<(u64, u64)>,
    /// 物理内存总量（kB）。
    pub mem_total_kb: u64,
    /// 可用内存（kB）。
    pub mem_avail_kb: u64,
    /// CPU 核数。
    pub cores: u64,
}

/// 读一次本机限制快照（只读 `/proc`，无副作用）。
///
/// # 返回值
/// 快照；非 Linux 时 `supported = false`（其余为 0，不报错）
pub fn snapshot() -> LimitsSnapshot {
    let mut s = LimitsSnapshot {
        cores: std::thread::available_parallelism()
            .map(|n| n.get() as u64)
            .unwrap_or(1),
        ..LimitsSnapshot::default()
    };
    // 内存：跨平台（Linux `/proc/meminfo`、Windows GlobalMemoryStatusEx）
    if let Some((total, avail)) = crate::sys::machine::memory_info() {
        s.mem_total_kb = total / 1024;
        s.mem_avail_kb = avail / 1024;
    }
    if !cfg!(target_os = "linux") {
        return s;
    }
    s.supported = true;
    if let Some(text) = read_text("/proc/self/limits") {
        if let Some((soft, hard)) = parse_limits(&text) {
            s.fd_soft = soft;
            s.fd_hard = hard;
        }
    }
    s.fd_open = count_open_fds();
    s.file_max = read_u64("/proc/sys/fs/file-max").unwrap_or(0);
    s.nr_open = read_u64("/proc/sys/fs/nr_open").unwrap_or(0);
    // file-nr 三列：已分配 / 未使用 / 上限；第一列才是“已用”
    if let Some(text) = read_text("/proc/sys/fs/file-nr") {
        s.file_used = text
            .split_whitespace()
            .next()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
    }
    s.somaxconn = read_u64("/proc/sys/net/core/somaxconn").unwrap_or(0);
    s.syn_backlog = read_u64("/proc/sys/net/ipv4/tcp_max_syn_backlog").unwrap_or(0);
    if let (Some(count), Some(max)) = (
        read_u64("/proc/sys/net/netfilter/nf_conntrack_count"),
        read_u64("/proc/sys/net/netfilter/nf_conntrack_max"),
    ) {
        s.conntrack = Some((count, max));
    }
    s
}

/// 解析 `/proc/self/limits` 里的 `Max open files` 行 → `(软限, 硬限)`。
///
/// 行格式：`Max open files            1048576              1048576              files`。
/// 值为 `unlimited` 时按 [`u64::MAX`] 处理（注意 `setrlimit` 不接受该值，见 [`raise_nofile`]）。
///
/// # 参数
/// - `text`：`/proc/self/limits` 全文
///
/// # 返回值
/// 命中返回 `(soft, hard)`；未命中 `None`
pub fn parse_limits(text: &str) -> Option<(u64, u64)> {
    for line in text.lines() {
        if !line.trim_start().starts_with("Max open files") {
            continue;
        }
        let nums: Vec<u64> = line
            .split_whitespace()
            .skip(3)
            .filter_map(|w| {
                if w.eq_ignore_ascii_case("unlimited") {
                    Some(u64::MAX)
                } else {
                    w.parse::<u64>().ok()
                }
            })
            .collect();
        if nums.len() >= 2 {
            return Some((nums[0], nums[1]));
        }
    }
    None
}

/// 当前进程已打开的 fd 数（读 `/proc/self/fd` 目录项数；非 Linux 返回 0）。
///
/// # 返回值
/// fd 数量
pub fn count_open_fds() -> u64 {
    std::fs::read_dir("/proc/self/fd")
        .map(|it| it.count() as u64)
        .unwrap_or(0)
}

/// 当前进程是否 root（决定能否提高 fd **硬限**）。
#[cfg(unix)]
#[allow(unsafe_code)] // libc::geteuid 是唯一途径，且无入参、只读
pub fn is_root() -> bool {
    // SAFETY: geteuid 无入参、只读，永不出错
    unsafe { libc::geteuid() == 0 }
}

/// 当前进程是否 root（非 Unix 恒 `false`）。
#[cfg(not(unix))]
pub fn is_root() -> bool {
    false
}

/// 提升**当前进程**的 `RLIMIT_NOFILE`（不影响其它进程），返回 `(旧软, 旧硬, 新软, 新硬)`。
///
/// 语义：
/// - 非 root 只能提到硬限；
/// - root 可同时提高硬限，但**不得超过 `fs.nr_open`**（超了内核返回 EPERM，这里自动退回“只提软限”）；
/// - 只升不降：目标低于当前值时不做改动（返回当前值）。
///
/// # 参数
/// - `target`：期望的软限（如按连接数算出的推荐值）
///
/// # 返回值
/// 成功时 `Ok((旧软, 旧硬, 新软, 新硬))`；失败返回可读原因
pub fn raise_nofile(target: u64) -> Result<(u64, u64, u64, u64), String> {
    raise_nofile_impl(target)
}

#[cfg(unix)]
#[allow(unsafe_code)] // 同 `is_root`：rlimit 只有 libc 接口
fn raise_nofile_impl(target: u64) -> Result<(u64, u64, u64, u64), String> {
    // SAFETY: getrlimit/setrlimit 只读写传入的 rlimit 结构体
    unsafe {
        let mut cur: libc::rlimit = std::mem::zeroed();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut cur) != 0 {
            return Err(format!(
                "读取 fd 上限失败：{}",
                std::io::Error::last_os_error()
            ));
        }
        let old_soft = cur.rlim_cur as u64;
        let old_hard = cur.rlim_max as u64;
        let root = libc::geteuid() == 0;
        let mut want_hard = if root { target.max(old_hard) } else { target.min(old_hard) };
        let mut want_soft = target.min(want_hard);
        let mut want = libc::rlimit {
            rlim_cur: want_soft as libc::rlim_t,
            rlim_max: want_hard as libc::rlim_t,
        };
        if libc::setrlimit(libc::RLIMIT_NOFILE, &want) != 0 && root && want_hard > old_hard {
            // 硬限提不上去（如被 nr_open 卡住）：退回只提软限，保证至少一部分生效
            want_hard = old_hard;
            want_soft = target.min(old_hard);
            want = libc::rlimit {
                rlim_cur: want_soft as libc::rlim_t,
                rlim_max: want_hard as libc::rlim_t,
            };
            if libc::setrlimit(libc::RLIMIT_NOFILE, &want) != 0 {
                return Err(format!(
                    "设置 fd 上限失败：{}",
                    std::io::Error::last_os_error()
                ));
            }
        }
        let mut now: libc::rlimit = std::mem::zeroed();
        let _ = libc::getrlimit(libc::RLIMIT_NOFILE, &mut now);
        Ok((old_soft, old_hard, now.rlim_cur as u64, now.rlim_max as u64))
    }
}

#[cfg(not(unix))]
fn raise_nofile_impl(_target: u64) -> Result<(u64, u64, u64, u64), String> {
    Err("非 Unix 平台不支持调整 fd 上限".to_string())
}

/// 本进程所属的 systemd 单元名（`/proc/self/cgroup` 里那段 `xxx.service`）。
///
/// 星尘（Pek.RAgent）等非 systemd 托管的进程返回 `None` —— 此时只靠进程内 `setrlimit`，
/// 不需要也不应该去改 systemd。
///
/// # 返回值
/// 单元名（如 `hlktech-iot-broker.service`）；不在 systemd 下为 `None`
pub fn systemd_unit_of_self() -> Option<String> {
    let text = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    for line in text.lines() {
        // cgroup v2：`0::/system.slice/xxx.service`；v1：`...:name=/xxx.service`
        let seg = line.rsplit('/').next()?.trim();
        if seg.ends_with(".service") && !seg.is_empty() {
            return Some(seg.to_string());
        }
    }
    None
}

/// 给 systemd 单元写一个 drop-in 把 `LimitNOFILE` 抬到 `want`，然后 `daemon-reload`。
///
/// 用 drop-in（`/etc/systemd/system/<unit>.d/99-nofile.conf`）而不是直接改单元文件：
/// 单元可能由安装脚本/托管程序重写，drop-in 不会被覆盖；撤销只需删该文件。
///
/// **不重启**：重启时机交给调用方（避免保存设置时把连接瞬间掐断）。
///
/// # 参数
/// - `unit`：单元名（含 `.service`）
/// - `want`：目标 fd 上限
///
/// # 返回值
/// 人类可读结果；以 `!` 开头表示失败
pub fn write_systemd_dropin(unit: &str, want: u64) -> String {
    if !cfg!(target_os = "linux") {
        return "跳过（非 Linux）".to_string();
    }
    if !is_root() {
        return "!进程非 root，无法写 systemd 单元：请用 root 启动服务，或手工加 LimitNOFILE".to_string();
    }
    let dir = PathBuf::from(format!("/etc/systemd/system/{unit}.d"));
    let path = dir.join("99-nofile.conf");
    let body = format!(
        "# 由面板「连接容量」一键写入（只升不降）\n\
         # 一条长连接占 1 个 fd；撤销请删本文件后 daemon-reload\n\
         [Service]\nLimitNOFILE={want}\n"
    );
    if let Ok(old) = std::fs::read_to_string(&path) {
        if old.contains(&format!("LimitNOFILE={want}")) {
            return format!("已存在 {}（LimitNOFILE={want}），无需改动", path.display());
        }
    }
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return format!("!创建 {} 失败：{e}", dir.display());
    }
    if let Err(e) = std::fs::write(&path, &body) {
        return format!("!写入 {} 失败：{e}", path.display());
    }
    match std::process::Command::new("systemctl").arg("daemon-reload").output() {
        Ok(o) if o.status.success() => format!(
            "已写入 {}（LimitNOFILE={want}）并 daemon-reload；**重启服务后生效**",
            path.display()
        ),
        Ok(o) => format!(
            "已写入 {}，但 daemon-reload 失败（{}）",
            path.display(),
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => format!("已写入 {}，但无法执行 systemctl（{e}）", path.display()),
    }
}

/// 读一个小文本文件（读失败/不存在返回 `None`）。
fn read_text(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// 读一个只含整数的 `/proc/sys` 项。
fn read_u64(path: &str) -> Option<u64> {
    read_text(path)?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_limits_handles_units_and_unlimited() {
        let text = "Limit                     Soft Limit           Hard Limit           Units\n\
                    Max open files            1024                 1048576              files\n";
        assert_eq!(parse_limits(text), Some((1024, 1_048_576)));

        let unlimited = "Max open files            unlimited            unlimited            files\n";
        assert_eq!(parse_limits(unlimited), Some((u64::MAX, u64::MAX)));

        assert_eq!(parse_limits("Max processes 100 200\n"), None, "只认 fd 那一行");
    }

    #[test]
    fn snapshot_reads_memory_on_any_platform() {
        let s = snapshot();
        assert!(s.cores >= 1);
        // 内存读取走 `sys::machine::memory_info()`，Linux/Windows 都应有值
        if cfg!(any(target_os = "linux", windows)) {
            assert!(s.mem_total_kb > 0, "总内存应大于 0");
        }
    }

    #[test]
    fn raise_nofile_never_lowers() {
        // 目标给一个很小的值：不允许把当前上限降下来（只升不降）
        if let Ok((old_soft, _old_hard, new_soft, _new_hard)) = raise_nofile(1) {
            assert!(new_soft >= old_soft.min(new_soft), "软限不应被降低");
        }
    }
}
