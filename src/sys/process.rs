//! 进程管理（跨平台）：拉起/存活判定/信号与强制停止/资源查询/OOM 与优先级。
//!
//! 由 Pek.RAgent 下沉（2026-10-03 第二批）：代理类程序（应用守护、部署代理）对
//! 子进程与“接管进程”的通用控制面，供多项目复用。
//!
//! 设计要点：
//! - 启动的子进程默认重定向到空设备，避免服务模式无控制台时输出异常；
//! - 停止先温和（Unix SIGTERM / Windows taskkill），超时后强制（SIGKILL / taskkill /F）；
//! - 内存读取：Linux `/proc/{pid}/statm`、Windows `GetProcessMemoryInfo`、macOS `ps`；
//! - **Windows `is_alive` 必须查 `GetExitCodeProcess != STILL_ACTIVE`**：已终止但句柄
//!   未关闭的“僵尸”进程 `OpenProcess` 依然成功，只看句柄会误判存活
//!   （曾导致停止操作等待超时并错误返回失败）。
//!
//! 注意：自有子进程请用 `Child::try_wait` 回收；[`stop_process`] 用于“接管”的进程
//! （没有子进程句柄）。

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// 子进程句柄：自己拉起的持有 `Child`；接管来的只记 pid（如代理重启后继续守护）。
pub enum Handle {
    /// 自己拉起的进程
    Owned(Child),
    /// 接管（或外部）进程
    Adopted(u32),
}

impl Handle {
    /// 是否已退出（不阻塞）。
    pub fn has_exited(&mut self) -> bool {
        match self {
            Handle::Owned(c) => matches!(c.try_wait(), Ok(Some(_))),
            Handle::Adopted(p) => !is_alive(*p),
        }
    }
}

/// 进程启动请求。
pub struct SpawnRequest<'a> {
    /// 可执行程序（含 PATH 命令，如 dotnet/java）
    pub program: &'a str,
    /// 参数
    pub args: &'a [String],
    /// 工作目录
    pub cwd: &'a Path,
    /// 环境变量
    pub envs: &'a [(String, String)],
    /// 调试输出文件（追加）。为 None 时输出到空设备
    pub log_file: Option<&'a Path>,
    /// 独立会话/分离进程。用于一次性拉起后父进程立即退出的场景（zip 发布）
    pub detached: bool,
}

/// 拉起进程。
pub fn spawn(req: &SpawnRequest) -> std::io::Result<Child> {
    let mut cmd = Command::new(req.program);
    cmd.args(req.args);
    cmd.current_dir(req.cwd);
    for (k, v) in req.envs {
        cmd.env(k, v);
    }

    match req.log_file {
        Some(file) => {
            if let Some(parent) = file.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let out = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(file)?;
            let err = out.try_clone()?;
            cmd.stdin(Stdio::null())
                .stdout(Stdio::from(out))
                .stderr(Stdio::from(err));
        }
        None => {
            cmd.stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;

        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const DETACHED_PROCESS: u32 = 0x0000_0008;

        let mut flags = CREATE_NEW_PROCESS_GROUP;
        if req.detached {
            flags |= DETACHED_PROCESS;
        }
        cmd.creation_flags(flags);
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        let detached = req.detached;
        unsafe {
            cmd.pre_exec(move || {
                // 独立会话：避免随宿主进程组收到终端信号，也便于一次性拉起的应用继续运行
                libc::setsid();
                Ok(())
            });
            let _ = detached;
        }
    }

    cmd.spawn()
}

/// 进程是否存活。
#[cfg(windows)]
pub fn is_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // STILL_ACTIVE：进程仍在运行。
    // 注意：已终止但句柄未关闭的“僵尸”进程 OpenProcess 依然成功，必须检查退出码，
    // 否则会误判为存活（曾导致停止操作等待超时并错误返回失败）。
    const STILL_ACTIVE: u32 = 259;

    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return false;
        }

        let mut code: u32 = 0;
        let ok = GetExitCodeProcess(h, &mut code);
        CloseHandle(h);

        ok != 0 && code == STILL_ACTIVE
    }
}

/// 进程是否存活。
#[cfg(unix)]
pub fn is_alive(pid: u32) -> bool {
    unsafe {
        if libc::kill(pid as libc::pid_t, 0) == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

/// 发送温和停止信号（Unix SIGTERM / Windows taskkill）。
pub fn signal_graceful(pid: u32) {
    if pid == 0 {
        return;
    }

    #[cfg(unix)]
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }

    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// 强制结束进程（Unix SIGKILL / Windows taskkill /F）。
pub fn signal_force(pid: u32) {
    if pid == 0 {
        return;
    }

    #[cfg(unix)]
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }

    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// 停止进程：先温和后强制。返回是否已退出。
/// 用于“接管”的进程（没有子进程句柄）；自有子进程请用 `Child::try_wait` 回收。
pub fn stop_process(pid: u32, timeout_ms: u64) -> bool {
    if pid == 0 || !is_alive(pid) {
        return true;
    }

    signal_graceful(pid);

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    while Instant::now() < deadline {
        if !is_alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    signal_force(pid);

    let deadline = Instant::now() + Duration::from_millis(2_000);
    while Instant::now() < deadline {
        if !is_alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    !is_alive(pid)
}

/// 进程名（含扩展名，如 `app.exe`/`dotnet`）。尽力而为，取不到返回 None。
#[cfg(windows)]
pub fn process_name(pid: u32) -> Option<String> {
    let pid_s = pid.to_string();
    let text = run_capture(
        "tasklist",
        &["/FI", &format!("PID eq {}", pid_s), "/FO", "CSV", "/NH"],
    )?;

    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with('"') {
            continue;
        }
        let name = line.split(',').next()?.trim().trim_matches('"');
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }

    None
}

/// 进程名。
#[cfg(target_os = "linux")]
pub fn process_name(pid: u32) -> Option<String> {
    let text = std::fs::read_to_string(format!("/proc/{}/comm", pid)).ok()?;
    let name = text.trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// 进程名。
#[cfg(target_os = "macos")]
pub fn process_name(pid: u32) -> Option<String> {
    let text = run_capture("ps", &["-o", "comm=", "-p", &pid.to_string()])?;
    let name = text.trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// 进程私有内存（MB）。取不到返回 None。
#[cfg(windows)]
pub fn memory_mb(pid: u32) -> Option<u64> {
    use std::mem::size_of;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return None;
        }

        let mut counters: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        counters.cb = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        let ok = GetProcessMemoryInfo(h, &mut counters, counters.cb);
        CloseHandle(h);

        if ok == 0 {
            None
        } else {
            // PagefileUsage 即进程已提交私有内存（与 C# PrivateMemorySize64 语义一致）
            Some(counters.PagefileUsage as u64 / 1024 / 1024)
        }
    }
}

/// 进程常驻内存（MB）。
#[cfg(target_os = "linux")]
pub fn memory_mb(pid: u32) -> Option<u64> {
    let text = std::fs::read_to_string(format!("/proc/{}/statm", pid)).ok()?;
    let resident_pages: u64 = text.split_whitespace().nth(1)?.parse().ok()?;

    let mut page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page_size <= 0 {
        page_size = 4096;
    }

    Some(resident_pages * page_size as u64 / 1024 / 1024)
}

/// 进程常驻内存（MB）。
#[cfg(target_os = "macos")]
pub fn memory_mb(pid: u32) -> Option<u64> {
    let text = run_capture("ps", &["-o", "rss=", "-p", &pid.to_string()])?;
    let kb: u64 = text.trim().parse().ok()?;
    Some(kb / 1024)
}

/// 设置 OOM 分值（仅 Linux；尽力而为）。
#[cfg(target_os = "linux")]
pub fn set_oom_score_adjust(pid: u32, value: i32) {
    let _ = std::fs::write(format!("/proc/{}/oom_score_adj", pid), value.to_string());
}

/// 设置 OOM 分值（非 Linux 平台为空操作）。
#[cfg(not(target_os = "linux"))]
pub fn set_oom_score_adjust(_pid: u32, _value: i32) {}

/// 提高当前进程优先级（服务模式下确保代理能有效管控各应用进程）。
pub fn raise_priority() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{
            GetCurrentProcess, SetPriorityClass, ABOVE_NORMAL_PRIORITY_CLASS,
        };
        unsafe {
            SetPriorityClass(GetCurrentProcess(), ABOVE_NORMAL_PRIORITY_CLASS);
        }
    }

    #[cfg(unix)]
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, -5);
    }
}

/// 释放当前进程工作集（Windows `EmptyWorkingSet`；其它平台空操作）。
pub fn empty_working_set() -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::ProcessStatus::EmptyWorkingSet;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_SET_QUOTA,
        };

        unsafe {
            // EmptyWorkingSet 需要 SET_QUOTA 权限（PROCESS_QUERY_LIMITED_INFORMATION 不足）
            let h = OpenProcess(
                PROCESS_SET_QUOTA | PROCESS_QUERY_INFORMATION,
                0,
                std::process::id(),
            );
            if h.is_null() {
                return false;
            }
            let ok = EmptyWorkingSet(h);
            CloseHandle(h);
            ok != 0
        }
    }

    #[cfg(target_os = "linux")]
    {
        // glibc 的 malloc_trim 将空闲堆归还系统（等价于 C# GC + 释放虚拟内存的尽力而为）；
        // musl 无此扩展（交叉编译到 musl 目标时因缺符号失败过），按“空操作成功”处理
        // （面板显示释放 0MB，而非误报失败）。
        #[cfg(target_env = "gnu")]
        {
            unsafe { libc::malloc_trim(0) != 0 }
        }

        #[cfg(not(target_env = "gnu"))]
        {
            true
        }
    }

    #[cfg(not(any(windows, target_os = "linux")))]
    {
        false
    }
}

/// 执行外部命令并捕获标准输出（失败返回 None）。
///
/// Windows/macOS 的部分进程查询（tasklist/ps）与 macOS CPU 型号读取使用；
/// Linux 下无调用方（走 /proc 直读）。
#[cfg_attr(target_os = "linux", allow(dead_code))]
pub(crate) fn run_capture(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&output.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_process_is_alive() {
        let pid = std::process::id();
        assert!(is_alive(pid));
    }

    #[test]
    fn memory_of_current_process() {
        let pid = std::process::id();
        if let Some(mb) = memory_mb(pid) {
            assert!(mb > 0);
        }
    }

    /// 已退出但句柄未回收的“僵尸”进程不得误判为存活（Windows）。
    #[cfg(windows)]
    #[test]
    fn zombie_process_reports_not_alive() {
        let mut child = std::process::Command::new("cmd")
            .args(["/c", "exit", "0"])
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(500));

        let pid = child.id();
        assert!(!is_alive(pid), "僵尸进程被误判为存活");
        let _ = child.wait();
    }
}
