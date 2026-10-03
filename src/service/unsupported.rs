//! 不支持的平台占位实现。

use super::{ServiceManager, ServiceState};

fn unsupported() -> Result<(), String> {
    Err("当前平台暂不支持服务化安装（仅支持 Windows / Linux / macOS）".to_string())
}

/// 服务管理器类型名（用于状态显示）。
pub fn init_name() -> &'static str {
    "（未支持）"
}

/// 查询状态。
pub fn query(_mgr: &ServiceManager) -> ServiceState {
    ServiceState::NotInstalled
}

/// 安装。
/// 查询服务实际注册的程序路径（平台不支持服务化，恒为 `None`）。
pub fn query_installed_exe(_mgr: &ServiceManager) -> Option<std::path::PathBuf> {
    None
}

pub fn install(_mgr: &ServiceManager, _start: bool) -> Result<(), String> {
    unsupported()
}

/// 清理旧服务名（平台不支持服务化，空操作）。
pub fn cleanup_legacy(_mgr: &ServiceManager) {}

/// 重新安装。
pub fn reinstall(_mgr: &ServiceManager) -> Result<(), String> {
    unsupported()
}

/// 卸载。
pub fn uninstall(_mgr: &ServiceManager, _stop: bool) -> Result<(), String> {
    unsupported()
}

/// 启动。
pub fn start(_mgr: &ServiceManager) -> Result<(), String> {
    unsupported()
}

/// 停止。
pub fn stop(_mgr: &ServiceManager) -> Result<(), String> {
    unsupported()
}

/// 重启。
pub fn restart(_mgr: &ServiceManager) -> Result<(), String> {
    unsupported()
}
