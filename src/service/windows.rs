//! Windows 服务：SCM 控制（sc.exe）与运行时宿主（windows-service crate）。

use std::time::Duration;

use super::{ServiceManager, ServiceState};

/// 执行命令并返回（退出码，标准输出，标准错误）。
fn run(program: &str, args: &[&str]) -> (i32, String, String) {
    crate::sys::process::run(program, args)
}

/// 服务管理器类型名（用于状态显示）。
pub fn init_name() -> &'static str {
    "Windows 服务"
}

/// 查询服务状态。
pub fn query(mgr: &ServiceManager) -> ServiceState {
    let (code, stdout, stderr) = run("sc", &["query", &mgr.name]);
    let text = format!("{}\n{}", stdout, stderr).to_uppercase();

    if code != 0 {
        // 1060 = 指定的服务未安装
        return ServiceState::NotInstalled;
    }

    if text.contains("RUNNING") {
        ServiceState::Running
    } else if text.contains("STOPPED") {
        ServiceState::Stopped
    } else {
        ServiceState::Unknown
    }
}

/// 查询服务实际注册的可执行文件路径（`sc qc` 的二进制路径列）。
///
/// 与 [`ServiceManager::exe`]（当前进程路径）不同：服务可能安装在其他目录，
/// 从开发输出目录启动菜单时用它识别真实安装位置；未安装或读取失败返回 `None`。
pub fn query_installed_exe(mgr: &ServiceManager) -> Option<std::path::PathBuf> {
    let (code, stdout, stderr) = run("sc", &["qc", &mgr.name]);
    if code != 0 {
        return None;
    }
    parse_bin_path(&format!("{stdout}\n{stderr}"))
}

/// 从 `sc qc` 输出解析注册的程序路径。
///
/// 不依赖字段名（避免系统语言差异）：定位首个 `.exe`，优先取引号包裹的完整路径
/// （安装时写入的 `"{exe}" -s` 形态），无引号时取到空白前的连续段。
fn parse_bin_path(output: &str) -> Option<std::path::PathBuf> {
    for line in output.lines() {
        let lower = line.to_ascii_lowercase();
        let Some(pos) = lower.find(".exe") else {
            continue;
        };
        let end = pos + 4;

        // 引号包裹（路径含空格时的标准形态）：`"C:\dir\app.exe" -s`
        if line[end..].trim_start().starts_with('"') {
            if let Some(open) = line[..pos].rfind('"') {
                let path = line[open + 1..end].trim();
                if !path.is_empty() {
                    return Some(std::path::PathBuf::from(path));
                }
            }
        }

        // 无引号：取 `.exe` 结尾的连续非空白段
        let start = line[..end]
            .rfind(char::is_whitespace)
            .map(|i| i + 1)
            .unwrap_or(0);
        let path = line[start..end].trim_matches('"');
        if !path.is_empty() {
            return Some(std::path::PathBuf::from(path));
        }
    }
    None
}

/// 清理旧服务名（改名迁移，best-effort）：旧注册**指向本程序**时停止并删除；
/// 指向其它程序时保留不动，二者可继续并存。
pub fn cleanup_legacy(mgr: &ServiceManager) {
    for legacy in mgr.legacy_names() {
        cleanup_one(mgr, legacy);
    }
}

/// 清理单个旧服务名。
fn cleanup_one(mgr: &ServiceManager, legacy: &str) {
    if mgr.name.eq_ignore_ascii_case(legacy) {
        return;
    }
    let (code, stdout, stderr) = run("sc", &["qc", legacy]);
    if code != 0 {
        return; // 旧服务不存在
    }
    let Some(path) = parse_bin_path(&format!("{stdout}\n{stderr}")) else {
        return;
    };
    if !super::is_same_program(&path, &mgr.exe) {
        super::log_format(
            "检测到旧服务名 {}（指向 {}，可能为其它程序），保留不动",
            &[legacy, &path.display().to_string()],
        );
        return;
    }

    // 停止旧服务（≤15 秒），随后删除注册
    let _ = run("sc", &["stop", legacy]);
    let deadline = std::time::Instant::now() + Duration::from_millis(15_000);
    while std::time::Instant::now() < deadline {
        let (c, o, e) = run("sc", &["query", legacy]);
        let text = format!("{o}{e}").to_uppercase();
        if c != 0 || text.contains("STOPPED") {
            break;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    let (code, stdout, stderr) = run("sc", &["delete", legacy]);
    let detail = format!("{stdout}{stderr}");
    let detail = detail.trim();
    if code == 0 {
        super::log_format("已自动清理旧服务名 {}（原指向本程序，已停止并删除）", &[legacy]);
    } else {
        super::log_format(
            "旧服务名 {} 清理失败：{}（如仍存在请手工执行 sc delete {}）",
            &[legacy, detail, legacy],
        );
    }
}

/// 等待服务到达期望状态。
fn wait_state(mgr: &ServiceManager, expected: ServiceState, timeout_ms: u64) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
    while std::time::Instant::now() < deadline {
        if query(mgr) == expected {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    query(mgr) == expected
}

/// 安装（`start` 为 true 时安装并启动）。
pub fn install(mgr: &ServiceManager, start: bool) -> Result<(), String> {
    if query(mgr) != ServiceState::NotInstalled {
        return Err(format!(
            "服务已存在：{}（可先 -uninstall，或使用 -reinstall）",
            mgr.name
        ));
    }

    let bin_path = format!("\"{}\" -s", mgr.exe.display());
    let (code, stdout, stderr) = run(
        "sc",
        &[
            "create",
            &mgr.name,
            "binPath=",
            &bin_path,
            "start=",
            "auto",
            "DisplayName=",
            &mgr.display,
        ],
    );
    if code != 0 {
        return Err(format!(
            "安装服务失败（请以管理员身份运行）：{}",
            format!("{}\n{}", stdout.trim(), stderr.trim()).trim()
        ));
    }

    // 描述与失败恢复策略（尽力而为）
    let _ = run("sc", &["description", &mgr.name, &mgr.description]);
    let _ = run(
        "sc",
        &[
            "failure",
            &mgr.name,
            "reset=",
            "86400",
            "actions=",
            "restart/5000/restart/10000/restart/30000",
        ],
    );

    super::log_format(
        "Windows 服务已安装：{}（程序目录 {}）",
        &[&mgr.name, &mgr.base.display().to_string()],
    );

    if start {
        start_service(mgr)?;
    }

    Ok(())
}

/// 重新安装。
pub fn reinstall(mgr: &ServiceManager) -> Result<(), String> {
    let _ = uninstall(mgr, true);
    std::thread::sleep(Duration::from_millis(500));
    install(mgr, true)
}

/// 卸载（`stop` 为 true 时先停止）。
pub fn uninstall(mgr: &ServiceManager, stop: bool) -> Result<(), String> {
    if query(mgr) == ServiceState::NotInstalled {
        return Ok(());
    }

    // 归属校验：仅允许卸载**指向本程序**的服务（防误删其它程序的同名服务，如 C# 版星尘）
    if let Some(path) = query_installed_exe(mgr) {
        if !super::is_same_program(&path, &mgr.exe) {
            return Err(super::ownership_conflict_message(
                &mgr.name,
                &path.display().to_string(),
                &format!("sc delete {}", mgr.name),
            ));
        }
    }

    if stop && query(mgr) == ServiceState::Running {
        let _ = stop_service(mgr);
    }

    let (code, stdout, stderr) = run("sc", &["delete", &mgr.name]);
    if code != 0 {
        return Err(format!(
            "卸载服务失败（请以管理员身份运行）：{}",
            format!("{}\n{}", stdout.trim(), stderr.trim()).trim()
        ));
    }

    super::log_format("Windows 服务已卸载：{}", &[&mgr.name]);
    Ok(())
}

/// 启动服务。
pub fn start(mgr: &ServiceManager) -> Result<(), String> {
    start_service(mgr)
}

/// 停止服务。
pub fn stop(mgr: &ServiceManager) -> Result<(), String> {
    stop_service(mgr)
}

/// 重启服务。
pub fn restart(mgr: &ServiceManager) -> Result<(), String> {
    if query(mgr) == ServiceState::Running {
        stop_service(mgr)?;
    }
    start_service(mgr)
}

fn start_service(mgr: &ServiceManager) -> Result<(), String> {
    if query(mgr) == ServiceState::NotInstalled {
        return Err(format!("服务未安装：{}", mgr.name));
    }
    if query(mgr) == ServiceState::Running {
        return Ok(());
    }

    let (code, stdout, stderr) = run("sc", &["start", &mgr.name]);
    if code != 0 {
        return Err(format!(
            "启动服务失败：{}",
            format!("{}\n{}", stdout.trim(), stderr.trim()).trim()
        ));
    }

    if wait_state(mgr, ServiceState::Running, 30_000) {
        Ok(())
    } else {
        Err("启动服务超时（可查看事件日志）".to_string())
    }
}

fn stop_service(mgr: &ServiceManager) -> Result<(), String> {
    if query(mgr) != ServiceState::Running {
        return Ok(());
    }

    let (code, stdout, stderr) = run("sc", &["stop", &mgr.name]);
    if code != 0 {
        return Err(format!(
            "停止服务失败：{}",
            format!("{}\n{}", stdout.trim(), stderr.trim()).trim()
        ));
    }

    if wait_state(mgr, ServiceState::Stopped, 30_000) {
        Ok(())
    } else {
        Err("停止服务超时".to_string())
    }
}

// ————— 服务运行时（windows-service crate）—————

#[cfg(windows)]
mod host {
    use super::*;

    use std::ffi::OsString;
    use std::sync::OnceLock;

    use windows_service::service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState as WinServiceState,
        ServiceStatus, ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
    use windows_service::{define_windows_service, service_dispatcher};

    static RUN: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();
    static ON_STOP: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();
    static NAME: OnceLock<String> = OnceLock::new();

    define_windows_service!(ffi_service_main, service_main);

    fn service_main(_args: Vec<OsString>) {
        if let Err(e) = run_in_service() {
            crate::service::log_error(&format!("Windows 服务运行失败：{:?}", e));
        }
    }

    fn run_in_service() -> windows_service::Result<()> {
        let name = NAME.get().cloned().unwrap_or_default();

        let event_handler = move |control: ServiceControl| -> ServiceControlHandlerResult {
            match control {
                ServiceControl::Stop | ServiceControl::Shutdown => {
                    if let Some(on_stop) = ON_STOP.get() {
                        on_stop();
                    }
                    ServiceControlHandlerResult::NoError
                }
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                _ => ServiceControlHandlerResult::NotImplemented,
            }
        };

        let status_handle = service_control_handler::register(name.as_str(), event_handler)?;

        status_handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: WinServiceState::Running,
            controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        })?;

        if let Some(run) = RUN.get() {
            run();
        }

        status_handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: WinServiceState::Stopped,
            controls_accepted: ServiceControlAccept::empty(),
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        })?;

        Ok(())
    }

    /// 以 Windows 服务方式运行；若未由 SCM 启动（如手工执行），回退为前台模式。
    ///
    /// - `on_stop`：收到 SCM 停止/关机控制时回调（用于置位宿主的退出标志）；
    /// - `run`：服务主体。
    pub fn run_as_service<F>(service_name: &str, on_stop: impl Fn() + Send + Sync + 'static, run: F)
    where
        F: Fn() + Send + Sync + 'static,
    {
        let _ = RUN.set(Box::new(run));
        let _ = ON_STOP.set(Box::new(on_stop));
        let _ = NAME.set(service_name.to_string());

        match service_dispatcher::start(service_name, ffi_service_main) {
            Ok(()) => {}
            Err(e) => {
                crate::service::log_format(
                    "未由服务控制管理器启动（{:?}），回退为前台运行；如需安装服务请使用 -install",
                    &[&format!("{:?}", e)],
                );
                if let Some(run) = RUN.get() {
                    run();
                }
            }
        }
    }
}

#[cfg(windows)]
pub use host::run_as_service;

#[cfg(test)]
mod tests {
    use super::parse_bin_path;
    use std::path::PathBuf;

    #[test]
    fn parse_bin_path_quoted_with_spaces() {
        let out = "[SC] QueryServiceConfig SUCCESS\n\nSERVICE_NAME: StarAgent\n        BINARY_PATH_NAME   : \"C:\\Program Files\\Star Agent\\pek-ragent.exe\" -s\n";
        assert_eq!(
            parse_bin_path(out),
            Some(PathBuf::from("C:\\Program Files\\Star Agent\\pek-ragent.exe"))
        );
    }

    #[test]
    fn parse_bin_path_plain() {
        let out = "        BINARY_PATH_NAME   : C:\\StarAgent\\pek-ragent.exe -s\n";
        assert_eq!(
            parse_bin_path(out),
            Some(PathBuf::from("C:\\StarAgent\\pek-ragent.exe"))
        );
    }

    #[test]
    fn parse_bin_path_not_installed() {
        let out = "[SC] OpenService FAILED 1060:\n\nThe specified service does not exist as an installed service.\n";
        assert_eq!(parse_bin_path(out), None);
    }
}
