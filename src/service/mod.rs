//! 跨平台服务管理：安装/卸载/启动/停止/重启/状态查询。
//!
//! - **Windows**：SCM。安装用 `sc.exe create`（binPath 指向 `{exe} -s`），运行时由
//!   `windows-service` crate 与 SCM 交互（见 [`windows::run_as_service`]）；
//! - **Linux**：自动探测 init 并分发——**systemd**（主流发行版）、**procd**（OpenWrt，
//!   `/etc/init.d/{name}` + `/etc/rc.common`）、**SysVinit / OpenRC**（`/etc/init.d` +
//!   `update-rc.d` / `chkconfig` / `rc-update`）；对应实现见 systemd / procd / sysv 模块；
//! - **macOS**：launchd `/Library/LaunchDaemons/{name}.plist` + `launchctl`。

use std::path::{Path, PathBuf};

/// 信息日志（`{}` 占位格式化的行日志；服务模块内部使用）。
pub(crate) fn log_format(template: &str, args: &[&str]) {
    let mut msg = String::with_capacity(template.len() + 32);
    let mut parts = template.split("{}");
    if let Some(first) = parts.next() {
        msg.push_str(first);
    }
    for (i, part) in parts.enumerate() {
        msg.push_str(args.get(i).copied().unwrap_or("?"));
        msg.push_str(part);
    }
    crate::logs::write_line(&msg);
}

/// 错误日志（服务模块内部使用）。
pub(crate) fn log_error(message: &str) {
    crate::logs::log().error(message);
}

#[cfg(windows)]
pub mod windows;

#[cfg(target_os = "linux")]
mod systemd;
#[cfg(target_os = "linux")]
mod procd;
#[cfg(target_os = "linux")]
mod sysv;
#[cfg(target_os = "linux")]
mod linux;

/// Linux init 探测与脚本模板（纯逻辑；测试时也编译，便于在开发机上跑单测）
#[cfg(any(target_os = "linux", test))]
mod inits;

#[cfg(target_os = "macos")]
mod launchd;

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
mod unsupported;

#[cfg(windows)]
use windows as platform;
#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(target_os = "macos")]
use launchd as platform;
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
use unsupported as platform;

/// 服务状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceState {
    /// 未安装
    NotInstalled,
    /// 已安装未运行
    Stopped,
    /// 运行中
    Running,
    /// 未知（查询失败等）
    Unknown,
}

impl ServiceState {
    /// 中文文本。
    pub fn text(self) -> &'static str {
        match self {
            ServiceState::NotInstalled => "未安装",
            ServiceState::Stopped => "未启动",
            ServiceState::Running => "运行中",
            ServiceState::Unknown => "未知",
        }
    }
}

/// 平台服务管理器。
pub struct ServiceManager {
    /// 服务名（Windows 服务名 / systemd 单元名 / init 脚本名 / launchd 标签）
    pub name: String,
    /// 显示名
    pub display: String,
    /// 描述（仅 Windows 安装时写入服务描述）
    #[cfg_attr(not(windows), allow(dead_code))]
    pub description: String,
    /// 代理可执行文件
    pub exe: PathBuf,
    /// 基础目录
    pub base: PathBuf,
    /// 旧服务名（服务改名迁移用）：安装/重装前自动清理**指向本程序**的旧注册；
    /// 指向其它程序（如同名异构程序）的注册保留不动。
    pub legacy: Vec<String>,
}

impl ServiceManager {
    /// 按给定信息创建（`legacy` 为旧服务名列表；无旧名传空切片）。
    pub fn new(
        base: &Path,
        name: &str,
        display: &str,
        description: &str,
        legacy: &[&str],
    ) -> ServiceManager {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from(name));
        ServiceManager {
            name: name.to_string(),
            display: display.to_string(),
            description: description.to_string(),
            exe,
            base: base.to_path_buf(),
            legacy: legacy.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// 旧服务名列表（迁移清理用）。
    pub(crate) fn legacy_names(&self) -> &[String] {
        &self.legacy
    }

    /// 查询状态。
    pub fn query(&self) -> ServiceState {
        platform::query(self)
    }

    /// 服务管理器类型名（`systemd` / `procd` / `SysVinit` / `launchd` / `Windows 服务`），
    /// 用于状态行显示（对齐 C# `-status` 的“状态：systemd ...”风格）。
    pub fn init_name(&self) -> &'static str {
        platform::init_name()
    }

    /// 安装（`start` 为 true 时安装并启动）。
    ///
    /// 安装前先做旧服务名清理（best-effort）：[`ServiceManager::legacy`] 中的旧名
    /// 若指向本程序（改名迁移场景），自动停止并删除其注册；若指向其它程序，保留不动。
    pub fn install(&self, start: bool) -> Result<(), String> {
        platform::cleanup_legacy(self);
        platform::install(self, start)
    }

    /// 查询服务实际注册的程序路径（读取服务注册信息；未安装或读取失败返回 `None`）。
    ///
    /// 与 [`ServiceManager::exe`]（当前进程路径）不同：服务可能安装在其他目录
    /// （如从开发输出目录启动菜单时），用于展示真实安装位置。
    pub fn installed_exe(&self) -> Option<PathBuf> {
        platform::query_installed_exe(self)
    }

    /// 重新安装（先卸载再安装并启动）。
    ///
    /// 与 [`ServiceManager::install`] 相同：安装前先做旧服务名清理（best-effort）。
    pub fn reinstall(&self) -> Result<(), String> {
        platform::cleanup_legacy(self);
        platform::reinstall(self)
    }

    /// 卸载（`stop` 为 true 时先停止再卸载）。
    pub fn uninstall(&self, stop: bool) -> Result<(), String> {
        platform::uninstall(self, stop)
    }

    /// 启动。
    pub fn start(&self) -> Result<(), String> {
        platform::start(self)
    }

    /// 停止。
    pub fn stop(&self) -> Result<(), String> {
        platform::stop(self)
    }

    /// 重启。
    pub fn restart(&self) -> Result<(), String> {
        platform::restart(self)
    }
}

// ————— 旧服务名清理（默认名迁移）与程序身份比对 —————

/// 判断服务注册的程序路径与本程序是否指向同一文件（`canonicalize` 后比较文本）。
pub(crate) fn same_exe(registered: &Path, current: &Path) -> bool {
    let a = std::fs::canonicalize(registered).unwrap_or_else(|_| registered.to_path_buf());
    let b = std::fs::canonicalize(current).unwrap_or_else(|_| current.to_path_buf());
    same_exe_text(&a.to_string_lossy(), &b.to_string_lossy())
}

/// 程序路径文本比较（Windows 大小写不敏感；其它平台大小写敏感）。
pub(crate) fn same_exe_text(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

/// 旧注册是否属于本程序（迁移清理的安全校验）：
/// 同一文件，或其文件名与本程序一致（开发目录与部署目录的同名副本）。
pub(crate) fn is_same_program(registered: &Path, current: &Path) -> bool {
    if same_exe(registered, current) {
        return true;
    }
    match (
        registered.file_name().and_then(|s| s.to_str()),
        current.file_name().and_then(|s| s.to_str()),
    ) {
        (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
        _ => false,
    }
}

/// 卸载归属冲突时的拒绝信息：服务名被其它程序占用（防误删同名异构程序的注册）。
pub(crate) fn ownership_conflict_message(service: &str, owner: &str, manual: &str) -> String {
    format!(
        "服务名 {service} 已被其它程序占用（{owner}），已拒绝卸载以避免误删。\n\
如需两者并存：请将本程序的服务名配置改为其它名称后重试；\n\
确需用本程序接管该服务名：请先手工执行 {manual}，再重新安装。"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_exe_text_respects_platform_case_rules() {
        if cfg!(windows) {
            assert!(same_exe_text("C:\\Dir\\App.exe", "c:\\dir\\app.exe"));
        } else {
            assert!(!same_exe_text("/opt/App", "/opt/app"));
        }
    }

    #[test]
    fn is_same_program_matches_same_file_name() {
        // 文件名一致（开发目录 vs 部署目录的同名副本）→ 视为本程序
        assert!(is_same_program(
            Path::new("C:\\deploy\\pek-ragent.exe"),
            Path::new("G:\\target\\release\\pek-ragent.exe")
        ));
        // 文件名不同（如 C# 版 StarAgent.exe）→ 不视为本程序
        assert!(!is_same_program(
            Path::new("C:\\StarAgent\\StarAgent.exe"),
            Path::new("C:\\StarAgent\\pek-ragent.exe")
        ));
    }
}
