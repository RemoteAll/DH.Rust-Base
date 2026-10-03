//! macOS launchd 服务管理（尽力支持，未经真机验证）。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::{ServiceManager, ServiceState};

/// plist 路径。
fn plist_path(name: &str) -> PathBuf {
    PathBuf::from(format!("/Library/LaunchDaemons/{}.plist", name))
}

/// 执行命令并返回（退出码，标准输出，标准错误）。
fn run(args: &[&str]) -> (i32, String, String) {
    crate::sys::process::run("launchctl", args)
}

/// 要求 root 权限。
fn require_root() -> Result<(), String> {
    let euid = unsafe { libc::geteuid() };
    if euid != 0 {
        Err("需要 root 权限（请使用 sudo 运行）".to_string())
    } else {
        Ok(())
    }
}

/// XML 转义。
fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// plist 内容。
/// 查询服务实际注册的程序路径（解析 plist 的 `ProgramArguments` 首项；失败返回 `None`）。
pub fn query_installed_exe(mgr: &ServiceManager) -> Option<std::path::PathBuf> {
    let text = std::fs::read_to_string(plist_path(&mgr.name)).ok()?;
    parse_program_argument(&text).map(std::path::PathBuf::from)
}

/// 解析 plist 文本中 `ProgramArguments` 数组的首个 `<string>` 值。
fn parse_program_argument(text: &str) -> Option<String> {
    let key = text.find("<key>ProgramArguments</key>")?;
    let after = &text[key..];
    let start = after.find("<string>")? + "<string>".len();
    let end = after[start..].find("</string>")? + start;
    Some(after[start..end].to_string())
}

/// 清理旧服务名（改名迁移，best-effort）：旧 plist 指向本程序时卸载并删除；
/// 指向其它程序时保留不动。
pub fn cleanup_legacy(mgr: &ServiceManager) {
    for legacy in mgr.legacy_names() {
        cleanup_one(mgr, legacy);
    }
}

/// 清理单个旧服务名。
fn cleanup_one(mgr: &ServiceManager, legacy: &str) {
    if mgr.name == legacy {
        return;
    }
    let path = plist_path(legacy);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let Some(exe) = parse_program_argument(&text) else {
        return;
    };
    let exe = std::path::PathBuf::from(exe);
    if !super::is_same_program(&exe, &mgr.exe) {
        super::log_format(
            "检测到旧 launchd 任务 {}（指向 {}，可能为其它程序），保留不动",
            &[&path.display().to_string(), &exe.display().to_string()],
        );
        return;
    }
    let _ = run(&["bootout", &format!("system/{legacy}")]);
    let _ = std::fs::remove_file(&path);
    super::log_format(
        "已自动清理旧 launchd 任务 {}（原指向本程序，已卸载并删除）",
        &[&path.display().to_string()],
    );
}

fn plist_text(mgr: &ServiceManager) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n\
<dict>\n\
  <key>Label</key><string>{name}</string>\n\
  <key>ProgramArguments</key>\n\
  <array><string>{exe}</string><string>-s</string></array>\n\
  <key>WorkingDirectory</key><string>{base}</string>\n\
  <key>RunAtLoad</key><true/>\n\
  <key>KeepAlive</key><true/>\n\
  <key>StandardOutPath</key><string>{base}/Log/launchd.out.log</string>\n\
  <key>StandardErrorPath</key><string>{base}/Log/launchd.err.log</string>\n\
</dict>\n\
</plist>\n",
        name = xml_escape(&mgr.name),
        exe = xml_escape(&mgr.exe.display().to_string()),
        base = xml_escape(&mgr.base.display().to_string())
    )
}

/// 服务管理器类型名（用于状态显示）。
pub fn init_name() -> &'static str {
    "launchd"
}

/// 查询状态。
pub fn query(mgr: &ServiceManager) -> ServiceState {
    if !plist_path(&mgr.name).exists() {
        return ServiceState::NotInstalled;
    }

    let (code, stdout, _) = run(&["print", &format!("system/{}", mgr.name)]);
    if code == 0 {
        if stdout.contains("state = running") {
            ServiceState::Running
        } else {
            ServiceState::Stopped
        }
    } else {
        ServiceState::Stopped
    }
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

    let path = plist_path(&mgr.name);
    if path.exists() {
        return Err(format!(
            "服务已存在：{}（可先 -uninstall，或使用 -reinstall）",
            mgr.name
        ));
    }

    std::fs::write(&path, plist_text(mgr))
        .map_err(|e| format!("写入 plist 失败 {}：{}", path.display(), e))?;

    let path_text = path.display().to_string();
    let (code, _, err) = run(&["bootstrap", "system", &path_text]);
    if code != 0 {
        // 旧版 launchctl 回退
        let (code2, _, err2) = run(&["load", "-w", &path_text]);
        if code2 != 0 {
            return Err(format!(
                "launchctl bootstrap 失败：{}；load 失败：{}",
                err.trim(),
                err2.trim()
            ));
        }
    }

    super::log_format("launchd 服务已安装：{}", &[&mgr.name]);

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
    require_root()?;

    let path = plist_path(&mgr.name);
    if !path.exists() {
        return Ok(());
    }

    // 归属校验：仅允许卸载指向本程序的 plist（防误删）
    if let Ok(text) = std::fs::read_to_string(&path) {
        if let Some(exe) = parse_program_argument(&text).map(std::path::PathBuf::from) {
            if !super::is_same_program(&exe, &mgr.exe) {
                return Err(super::ownership_conflict_message(
                    &mgr.name,
                    &exe.display().to_string(),
                    &format!("launchctl bootout system/{} && rm {}", mgr.name, path.display()),
                ));
            }
        }
    }

    if stop {
        let _ = run(&["kill", "SIGTERM", &format!("system/{}", mgr.name)]);
    }

    let path_text = path.display().to_string();
    let (code, _, _) = run(&["bootout", "system", &path_text]);
    if code != 0 {
        let _ = run(&["unload", "-w", &path_text]);
    }

    std::fs::remove_file(&path).map_err(|e| format!("删除 plist 失败 {}：{}", path.display(), e))?;

    super::log_format("launchd 服务已卸载：{}", &[&mgr.name]);
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

    let (code, _, err) = run(&["kickstart", "-k", &format!("system/{}", mgr.name)]);
    if code != 0 {
        let (code2, _, err2) = run(&["start", &mgr.name]);
        if code2 != 0 {
            return Err(format!(
                "launchctl kickstart 失败：{}；start 失败：{}",
                err.trim(),
                err2.trim()
            ));
        }
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

    let (code, _, err) = run(&["kill", "SIGTERM", &format!("system/{}", mgr.name)]);
    if code != 0 {
        let (code2, _, err2) = run(&["stop", &mgr.name]);
        if code2 != 0 {
            return Err(format!(
                "launchctl kill 失败：{}；stop 失败：{}",
                err.trim(),
                err2.trim()
            ));
        }
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

    let _ = stop_service(mgr);
    start_service(mgr)
}
