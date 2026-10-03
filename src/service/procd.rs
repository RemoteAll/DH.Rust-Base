//! OpenWrt procd 服务管理（`/etc/init.d/{name}` 脚本 + `/etc/rc.common`）。

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::inits;
use super::linux::{process_running, require_root, run};
use super::{ServiceManager, ServiceState};

/// init 脚本路径。
fn script_path(name: &str) -> PathBuf {
    PathBuf::from(format!("/etc/init.d/{}", name))
}

/// 用于进程匹配的二进制名（如 `pek-ragent`）。
fn process_stem(mgr: &ServiceManager) -> String {
    mgr.exe
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| mgr.name.clone())
}

/// 查询状态。
pub fn query(mgr: &ServiceManager) -> ServiceState {
    if !script_path(&mgr.name).exists() {
        return ServiceState::NotInstalled;
    }

    if process_running(&process_stem(mgr)) {
        ServiceState::Running
    } else {
        ServiceState::Stopped
    }
}

/// 调用 init 脚本动作（start/stop/restart/enable/disable…）。
fn ctl(mgr: &ServiceManager, action: &str) -> (i32, String) {
    let path = script_path(&mgr.name).to_string_lossy().into_owned();
    let (code, _, err) = run(&path, &[action]);
    (code, err)
}

/// 等待状态。
fn wait_state(mgr: &ServiceManager, expected: ServiceState, timeout_ms: u64) -> bool {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    while Instant::now() < deadline {
        if query(mgr) == expected {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    query(mgr) == expected
}

/// 安装（`start` 为 true 时安装并启动）。
pub fn install(mgr: &ServiceManager, start: bool) -> Result<(), String> {
    require_root()?;

    let path = script_path(&mgr.name);
    if path.exists() {
        return Err(format!(
            "服务已存在：{}（可先 -uninstall，或使用 -reinstall）",
            mgr.name
        ));
    }

    std::fs::write(&path, inits::procd_script_text(mgr))
        .map_err(|e| format!("写入 init 脚本失败 {}：{}", path.display(), e))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("设置执行权限失败：{}", e))?;

    let (code, err) = ctl(mgr, "enable");
    if code != 0 {
        return Err(format!("rc.common enable 失败：{}", err.trim()));
    }

    super::log_format("procd 服务已安装：{}", &[&mgr.name]);

    if start {
        start_service(mgr)?;
    }

    Ok(())
}

/// 卸载（`stop` 为 true 时先停止）。
pub fn uninstall(mgr: &ServiceManager, stop: bool) -> Result<(), String> {
    require_root()?;

    let path = script_path(&mgr.name);
    if !path.exists() {
        return Ok(());
    }

    // 归属校验：脚本不含本程序文件名时视为其它程序（防误删）
    if let Ok(text) = std::fs::read_to_string(&path) {
        let ours = mgr
            .exe
            .file_name()
            .and_then(|s| s.to_str())
            .map(|n| text.contains(n))
            .unwrap_or(false);
        if !ours {
            return Err(super::ownership_conflict_message(
                &mgr.name,
                &format!("非本程序（{}）", path.display()),
                &format!("rm {} && {} disable", path.display(), path.display()),
            ));
        }
    }

    if stop {
        let _ = ctl(mgr, "stop");
        let _ = wait_state(mgr, ServiceState::Stopped, 5_000);
    }
    let _ = ctl(mgr, "disable");

    std::fs::remove_file(&path)
        .map_err(|e| format!("删除 init 脚本失败 {}：{}", path.display(), e))?;

    super::log_format("procd 服务已卸载：{}", &[&mgr.name]);
    Ok(())
}

/// 启动服务。
pub fn start(mgr: &ServiceManager) -> Result<(), String> {
    start_service(mgr)
}

fn start_service(mgr: &ServiceManager) -> Result<(), String> {
    if query(mgr) == ServiceState::NotInstalled {
        return Err(format!("服务未安装：{}", mgr.name));
    }
    if query(mgr) == ServiceState::Running {
        return Ok(());
    }

    let (code, err) = ctl(mgr, "start");
    if code != 0 {
        return Err(format!("init 脚本 start 失败：{}", err.trim()));
    }

    if wait_state(mgr, ServiceState::Running, 30_000) {
        Ok(())
    } else {
        Err("启动服务超时".to_string())
    }
}

/// 停止服务。
pub fn stop(mgr: &ServiceManager) -> Result<(), String> {
    stop_service(mgr)
}

fn stop_service(mgr: &ServiceManager) -> Result<(), String> {
    if query(mgr) != ServiceState::Running {
        return Ok(());
    }

    let (code, err) = ctl(mgr, "stop");
    if code != 0 {
        return Err(format!("init 脚本 stop 失败：{}", err.trim()));
    }

    if wait_state(mgr, ServiceState::Stopped, 30_000) {
        Ok(())
    } else {
        Err("停止服务超时".to_string())
    }
}

/// 重启服务（stop → start，兼容各版本 rc.common）。
pub fn restart(mgr: &ServiceManager) -> Result<(), String> {
    stop_service(mgr)?;
    std::thread::sleep(Duration::from_millis(500));
    start_service(mgr)
}
