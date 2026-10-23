//! 本机防火墙端口放行/撤销（Windows 防火墙 / Linux firewalld·ufw·iptables）。
//!
//! 只处理**本机**防火墙；云服务器安全组需在云控制台单独放行。
//!
//! 典型用途：面板里打开「允许远程访问」时自动放行监听端口。
//! 由 Pek.RDbProxy 的本地实现提炼下沉（2026-10-23）。
//!
//! ```no_run
//! // 打开远程访问：放行端口；关闭：撤销规则
//! let _ = dhrust::sys::firewall::ensure("MyPanel", true, 5599);
//! ```
//!
//! 注意：Windows 下需管理员权限（服务以 LocalSystem 运行即可）；Linux 下需 root
//! 或具备 sudo 免密。失败会返回原因，*不会* panic。

use std::process::Command;

/// 放行端口（幂等：先删同名规则再添加）。
///
/// * `rule`：规则名（Windows 防火墙规则名；Linux 忽略）
pub fn allow_port(rule: &str, port: u16) -> Result<String, String> {
    #[cfg(windows)]
    {
        let _ = Command::new("netsh")
            .args([
                "advfirewall",
                "firewall",
                "delete",
                "rule",
                &format!("name={rule}"),
            ])
            .output();
        let out = Command::new("netsh")
            .args([
                "advfirewall",
                "firewall",
                "add",
                "rule",
                &format!("name={rule}"),
                "dir=in",
                "action=allow",
                "protocol=TCP",
                &format!("localport={port}"),
            ])
            .output()
            .map_err(|e| format!("调用 netsh 失败：{e}"))?;
        if out.status.success() {
            Ok(format!("已在 Windows 防火墙放行 TCP {port}（规则 {rule}）"))
        } else {
            let text = String::from_utf8_lossy(&out.stdout);
            Err(format!(
                "netsh 执行失败（需以管理员身份运行）：{}",
                text.trim()
            ))
        }
    }
    #[cfg(not(windows))]
    {
        let _ = rule;
        if has("firewall-cmd") {
            run(
                "firewall-cmd",
                &["--add-port", &format!("{port}/tcp"), "--permanent"],
            )?;
            run("firewall-cmd", &["--reload"])?;
            return Ok(format!("已在 firewalld 放行 TCP {port}（永久）"));
        }
        if has("ufw") {
            run("ufw", &["allow", &format!("{port}/tcp")])?;
            return Ok(format!("已在 ufw 放行 TCP {port}"));
        }
        if has("iptables") {
            run(
                "iptables",
                &[
                    "-I",
                    "INPUT",
                    "-p",
                    "tcp",
                    "--dport",
                    &port.to_string(),
                    "-j",
                    "ACCEPT",
                ],
            )?;
            return Ok(format!(
                "已用 iptables 放行 TCP {port}（重启后失效，建议装 firewalld/ufw 持久化）"
            ));
        }
        Err("未检测到 firewalld / ufw / iptables，请手动放行端口".into())
    }
}

/// 撤销放行（幂等）。
pub fn close_port(rule: &str, port: u16) -> Result<String, String> {
    #[cfg(windows)]
    {
        // Windows 按规则名删除，不需要端口
        let _ = port;
        let out = Command::new("netsh")
            .args([
                "advfirewall",
                "firewall",
                "delete",
                "rule",
                &format!("name={rule}"),
            ])
            .output()
            .map_err(|e| format!("调用 netsh 失败：{e}"))?;
        let _ = out;
        Ok(format!("已撤销 Windows 防火墙规则 {rule}"))
    }
    #[cfg(not(windows))]
    {
        let _ = rule;
        if has("firewall-cmd") {
            let _ = run(
                "firewall-cmd",
                &["--remove-port", &format!("{port}/tcp"), "--permanent"],
            );
            let _ = run("firewall-cmd", &["--reload"]);
            return Ok(format!("已从 firewalld 撤销 TCP {port}"));
        }
        if has("ufw") {
            let _ = run("ufw", &["delete", "allow", &format!("{port}/tcp")]);
            return Ok(format!("已从 ufw 撤销 TCP {port}"));
        }
        if has("iptables") {
            let _ = run(
                "iptables",
                &[
                    "-D",
                    "INPUT",
                    "-p",
                    "tcp",
                    "--dport",
                    &port.to_string(),
                    "-j",
                    "ACCEPT",
                ],
            );
            return Ok(format!("已从 iptables 撤销 TCP {port}"));
        }
        Ok("未检测到本机防火墙，无需撤销".into())
    }
}

/// 按 `allow` 放行或撤销（幂等，供启动/设置变更时调用）。
pub fn ensure(rule: &str, allow: bool, port: u16) -> Result<String, String> {
    if allow {
        allow_port(rule, port)
    } else {
        close_port(rule, port)
    }
}

#[cfg(not(windows))]
fn has(cmd: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {cmd}")])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

#[cfg(not(windows))]
fn run(cmd: &str, args: &[&str]) -> Result<(), String> {
    let out = Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| format!("执行 {cmd} 失败：{e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{cmd} {} 失败：{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_does_not_panic() {
        // 真实放行需要管理员权限，这里只验证不 panic（不作为断言）
        let _ = allow_port("dhrust-test", 0);
        let _ = close_port("dhrust-test", 0);
    }
}
