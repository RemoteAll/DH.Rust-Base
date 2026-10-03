//! Linux systemd 服务管理。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::linux::{require_root, run};
use super::{ServiceManager, ServiceState};

/// 单元文件路径。
fn unit_path(name: &str) -> PathBuf {
    PathBuf::from(format!("/etc/systemd/system/{}.service", name))
}

/// 查询状态。
pub fn query(mgr: &ServiceManager) -> ServiceState {
    if !unit_path(&mgr.name).exists() {
        return ServiceState::NotInstalled;
    }

    let (_, stdout, _) = run("systemctl", &["is-active", &mgr.name]);
    match stdout.trim() {
        "active" => ServiceState::Running,
        "inactive" | "failed" => ServiceState::Stopped,
        "activating" | "deactivating" => ServiceState::Unknown,
        _ => ServiceState::Unknown,
    }
}

/// 等待状态。
/// 查询服务实际注册的程序路径（读单元文件解析 `ExecStart=`；失败返回 `None`）。
pub fn query_installed_exe(mgr: &ServiceManager) -> Option<PathBuf> {
    let text = std::fs::read_to_string(unit_path(&mgr.name)).ok()?;
    super::inits::parse_exec_start(&text)
}

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

    let path = unit_path(&mgr.name);
    if path.exists() {
        return Err(format!(
            "服务已存在：{}（可先 -uninstall，或使用 -reinstall）",
            mgr.name
        ));
    }

    std::fs::write(&path, super::inits::systemd_unit_text(mgr))
        .map_err(|e| format!("写入单元文件失败 {}：{}", path.display(), e))?;

    // 单元文件预检（systemd-analyze verify）：不同 systemd 版本对指令支持度/语法要求不同，
    // 输出（若有）提前落日志——成功场景也留档，便于事后排查
    let (_, vout, verr) = run("systemd-analyze", &["verify", &path.display().to_string()]);
    let verify = format!("{vout}{verr}");
    let verify = verify.trim();
    if !verify.is_empty() {
        for line in verify.lines().take(10) {
            super::log_format("systemd 单元预检: {}", &[line]);
        }
    }

    let (code, _, err) = run("systemctl", &["daemon-reload"]);
    if code != 0 {
        return Err(format!("systemctl daemon-reload 失败：{}", err.trim()));
    }

    let (code, _, err) = run("systemctl", &["enable", &mgr.name]);
    if code != 0 {
        return Err(format!("systemctl enable 失败：{}", err.trim()));
    }

    super::log_format("systemd 服务已安装：{}", &[&mgr.name]);

    if start {
        start_service(mgr)?;
    }

    Ok(())
}

/// 卸载（`stop` 为 true 时先停止）。
pub fn uninstall(mgr: &ServiceManager, stop: bool) -> Result<(), String> {
    require_root()?;

    let path = unit_path(&mgr.name);
    if !path.exists() {
        return Ok(());
    }

    // 归属校验：仅允许卸载**指向本程序**的单元（防误删其它程序的同名注册，如 C# 版星尘）
    if let Ok(text) = std::fs::read_to_string(&path) {
        if let Some(exe) = super::inits::parse_exec_start(&text) {
            if !super::is_same_program(&exe, &mgr.exe) {
                return Err(super::ownership_conflict_message(
                    &mgr.name,
                    &exe.display().to_string(),
                    &format!("systemctl disable --now {} && rm {}", mgr.name, path.display()),
                ));
            }
        }
    }

    if stop {
        let _ = run("systemctl", &["stop", &mgr.name]);
    }
    let _ = run("systemctl", &["disable", &mgr.name]);

    std::fs::remove_file(&path).map_err(|e| format!("删除单元文件失败 {}：{}", path.display(), e))?;

    let (code, _, err) = run("systemctl", &["daemon-reload"]);
    if code != 0 {
        return Err(format!("systemctl daemon-reload 失败：{}", err.trim()));
    }

    super::log_format("systemd 服务已卸载：{}", &[&mgr.name]);
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

    let (code, _, err) = run("systemctl", &["start", &mgr.name]);
    if code != 0 {
        // 内置自动诊断：失败时直接收集 systemd 侧详细信息（用户无需手工命令排查）
        let msg = format!(
            "systemctl start 失败：{}{}",
            err.trim(),
            diagnose(&mgr.name)
        );
        super::log_error(&msg);
        return Err(msg);
    }

    if wait_state(mgr, ServiceState::Running, 30_000) {
        Ok(())
    } else {
        let msg = format!("启动服务超时：{}{}", mgr.name, diagnose(&mgr.name));
        super::log_error(&msg);
        Err(msg)
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

    let (code, _, err) = run("systemctl", &["stop", &mgr.name]);
    if code != 0 {
        return Err(format!("systemctl stop 失败：{}", err.trim()));
    }

    if wait_state(mgr, ServiceState::Stopped, 30_000) {
        Ok(())
    } else {
        Err("停止服务超时".to_string())
    }
}

/// 重启服务。
pub fn restart(mgr: &ServiceManager) -> Result<(), String> {
    if query(mgr) == ServiceState::NotInstalled {
        return Err(format!("服务未安装：{}", mgr.name));
    }

    let (code, _, err) = run("systemctl", &["restart", &mgr.name]);
    if code != 0 {
        let msg = format!(
            "systemctl restart 失败：{}{}",
            err.trim(),
            diagnose(&mgr.name)
        );
        super::log_error(&msg);
        return Err(msg);
    }

    if wait_state(mgr, ServiceState::Running, 30_000) {
        Ok(())
    } else {
        let msg = format!("重启服务超时：{}{}", mgr.name, diagnose(&mgr.name));
        super::log_error(&msg);
        Err(msg)
    }
}

// ————— 失败自动诊断（内置排查：失败时把详情直接给用户，无需手工命令） —————

/// 收集服务启动/重启失败的 systemd 侧详细信息：
///
/// 1. 单元文件回显（控制字符可视化：`\r`/BOM/NUL 等“看不见的问题”直接现形）；
/// 2. `systemd-analyze verify`（通常直接指出坏行）；
/// 3. `systemctl status` 尾部（Loaded/Active 与错误行）；
/// 4. `journalctl -u` 尾部（解析/启动错误）。
///
/// 返回带缩进的文本块（追加到错误消息）；调用方同时写入日志。
fn diagnose(name: &str) -> String {
    let unit = unit_path(name);
    let unit_str = unit.display().to_string();
    let mut out = String::new();

    match std::fs::read_to_string(&unit) {
        Ok(text) => out.push_str(&format!(
            "\n—— 单元文件 {} ——\n{}",
            unit_str,
            super::inits::visualize_invisibles(&text, 2000)
        )),
        Err(e) => out.push_str(&format!("\n（无法读取单元文件 {unit_str}：{e}）")),
    }

    let (_, so, se) = run("systemd-analyze", &["verify", &unit_str]);
    let verify = format!("{so}{se}");
    if !verify.trim().is_empty() {
        out.push_str("\n—— systemd-analyze verify ——\n");
        out.push_str(&super::inits::clip(verify.trim(), 3000));
    }

    let (_, so, _) = run("systemctl", &["status", name, "--no-pager", "-l"]);
    if !so.trim().is_empty() {
        out.push_str("\n—— systemctl status（尾部）——\n");
        out.push_str(&super::inits::clip(&super::inits::tail_lines(&so, 15), 2000));
    }

    let (_, so, _) = run("journalctl", &["-u", name, "-n", "15", "--no-pager"]);
    if !so.trim().is_empty() {
        out.push_str("\n—— journalctl -u（尾部）——\n");
        out.push_str(&super::inits::clip(so.trim(), 2000));
    }

    out.push_str("\n（以上为自动诊断信息，已同时写入日志）");
    out
}
