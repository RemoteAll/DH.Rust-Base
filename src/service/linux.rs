//! Linux 平台分发：探测 init 实现（systemd / OpenWrt procd / SysVinit·OpenRC）并调用对应适配器。

use std::path::Path;

use super::inits::{self, InitKind};
use super::{procd, systemd, sysv, ServiceManager, ServiceState};

/// 探测当前系统的 init 实现。
///
/// 判定顺序（可用环境变量 `PEK_RAGENT_INIT=systemd|procd|sysv` 强制覆盖）：
/// 1. systemd：`/run/systemd/system` 存在（sd_booted 语义）；
/// 2. procd：`/etc/rc.common` 或 `/sbin/procd` 存在（OpenWrt）；
/// 3. SysVinit/OpenRC：`/etc/init.d` 存在。
pub fn detect_init() -> InitKind {
    let over = std::env::var("PEK_RAGENT_INIT").ok();
    inits::decide(
        over.as_deref(),
        Path::new("/run/systemd/system").is_dir(),
        Path::new("/etc/rc.common").exists(),
        Path::new("/sbin/procd").exists(),
        Path::new("/etc/init.d").is_dir(),
    )
}

/// 服务管理器类型名（用于状态显示）。
pub fn init_name() -> &'static str {
    detect_init().text()
}

/// 查询状态。
pub fn query(mgr: &ServiceManager) -> ServiceState {
    match detect_init() {
        InitKind::Systemd => systemd::query(mgr),
        InitKind::Procd => procd::query(mgr),
        InitKind::Sysv => sysv::query(mgr),
        InitKind::Unknown => ServiceState::Unknown,
    }
}

/// 安装（`start` 为 true 时安装并启动）。
/// 查询服务实际注册的程序路径（systemd 读单元文件；其他 init 暂缺，返回 `None`）。
pub fn query_installed_exe(mgr: &ServiceManager) -> Option<std::path::PathBuf> {
    match detect_init() {
        InitKind::Systemd => systemd::query_installed_exe(mgr),
        _ => None,
    }
}

/// 清理旧服务名（改名迁移，best-effort）：旧注册**指向本程序**时停止并删除；
/// 指向其它程序时保留不动（同名异构程序的 init 脚本）。
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

    // systemd：/etc/systemd/system 与 /lib/systemd/system 下的旧单元
    for dir in ["/etc/systemd/system", "/lib/systemd/system"] {
        let path = Path::new(dir).join(format!("{legacy}.service"));
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        match inits::parse_exec_start(&text) {
            Some(exe) if super::is_same_program(&exe, &mgr.exe) => {
                let _ = run("systemctl", &["stop", legacy]);
                let _ = run("systemctl", &["disable", legacy]);
                let _ = std::fs::remove_file(&path);
                let _ = run("systemctl", &["daemon-reload"]);
                super::log_format(
                    "已自动清理旧单元文件 {}（原指向本程序，已停止并删除）",
                    &[&path.display().to_string()],
                );
            }
            Some(exe) => super::log_format(
                "检测到旧单元文件 {}（指向 {}，可能为其它程序），保留不动",
                &[&path.display().to_string(), &exe.display().to_string()],
            ),
            None => {}
        }
    }

    // SysVinit / OpenRC / procd：/etc/init.d/{legacy}（脚本内含本程序路径时清理）
    let script = Path::new("/etc/init.d").join(legacy);
    if let Ok(text) = std::fs::read_to_string(&script) {
        if text.contains(&mgr.exe.display().to_string()) {
            let _ = run(&script.display().to_string(), &["stop"]);
            let _ = std::fs::remove_file(&script);
            let _ = run("update-rc.d", &[legacy, "remove"]);
            let _ = run("chkconfig", &["--del", legacy]);
            let _ = run("rc-update", &["del", legacy]);
            super::log_format(
                "已自动清理旧 init 脚本 {}（原指向本程序，已停止并移除自启）",
                &[&script.display().to_string()],
            );
        }
    }
}

pub fn install(mgr: &ServiceManager, start: bool) -> Result<(), String> {
    let kind = detect_init();
    super::log_format("检测到 init 系统：{}", &[kind.text()]);

    match kind {
        InitKind::Systemd => systemd::install(mgr, start),
        InitKind::Procd => procd::install(mgr, start),
        InitKind::Sysv => sysv::install(mgr, start),
        InitKind::Unknown => Err(no_init_message()),
    }
}

/// 重新安装（先卸载再安装并启动）。
pub fn reinstall(mgr: &ServiceManager) -> Result<(), String> {
    let _ = uninstall(mgr, true);
    std::thread::sleep(std::time::Duration::from_millis(500));
    install(mgr, true)
}

/// 卸载（`stop` 为 true 时先停止）。
pub fn uninstall(mgr: &ServiceManager, stop: bool) -> Result<(), String> {
    match detect_init() {
        InitKind::Systemd => systemd::uninstall(mgr, stop),
        InitKind::Procd => procd::uninstall(mgr, stop),
        InitKind::Sysv => sysv::uninstall(mgr, stop),
        InitKind::Unknown => Err(no_init_message()),
    }
}

/// 启动。
pub fn start(mgr: &ServiceManager) -> Result<(), String> {
    match detect_init() {
        InitKind::Systemd => systemd::start(mgr),
        InitKind::Procd => procd::start(mgr),
        InitKind::Sysv => sysv::start(mgr),
        InitKind::Unknown => Err(no_init_message()),
    }
}

/// 停止。
pub fn stop(mgr: &ServiceManager) -> Result<(), String> {
    match detect_init() {
        InitKind::Systemd => systemd::stop(mgr),
        InitKind::Procd => procd::stop(mgr),
        InitKind::Sysv => sysv::stop(mgr),
        InitKind::Unknown => Err(no_init_message()),
    }
}

/// 重启。
pub fn restart(mgr: &ServiceManager) -> Result<(), String> {
    match detect_init() {
        InitKind::Systemd => systemd::restart(mgr),
        InitKind::Procd => procd::restart(mgr),
        InitKind::Sysv => sysv::restart(mgr),
        InitKind::Unknown => Err(no_init_message()),
    }
}

/// 未检测到 init 时的提示。
fn no_init_message() -> String {
    "未检测到受支持的 init 系统（systemd / OpenWrt procd / SysVinit·OpenRC）；\
可先用 `-run` 前台运行，或设置环境变量 PEK_RAGENT_INIT 强制指定（systemd|procd|sysv）"
        .to_string()
}

// ————— 公共辅助（各适配器共用） —————

/// 执行命令并返回（退出码，标准输出，标准错误）。
pub(crate) fn run(program: &str, args: &[&str]) -> (i32, String, String) {
    crate::sys::process::run(program, args)
}

/// 要求 root 权限。
pub(crate) fn require_root() -> Result<(), String> {
    let euid = unsafe { libc::geteuid() };
    if euid != 0 {
        Err("需要 root 权限（请使用 sudo 运行）".to_string())
    } else {
        Ok(())
    }
}

/// 进程是否在运行（pidof，回退 pgrep）。
pub(crate) fn process_running(stem: &str) -> bool {
    if run("pidof", &[stem]).0 == 0 {
        return true;
    }
    run("pgrep", &["-x", stem]).0 == 0
}
