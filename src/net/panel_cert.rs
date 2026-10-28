//! 面板 HTTPS 自签证书的**通用装配层**（Pek.RAgent / HlktechIoT / 其它带面板的 Rust 项目共用）。
//!
//! 为什么有这个模块：证书生成、PEM 解析、导入脚本原本分别散在 `net::tls` 与 `cert`，
//! 每个项目再写一层「路径约定 + 状态 JSON + 重签 + 下载」的薄壳 —— 于是同一段逻辑出现了
//! 第二份、第三份（Pek.RAgent 的 `tlscfg.rs` 与 HlktechIoT 的 `tlscfg.rs`）。
//! **这里是唯一实现**，各项目只写一行调用，不要再抄。
//!
//! 口径（与 Pek.RAgent 原实现完全一致，不要自行改）：
//! - 证书落 `{base}/Config/keys/panel.pem` + `panel.key`；
//! - **自签叶子证书 `CA:FALSE`**：私钥泄露也只能冒充本机 IP，不能冒充其他站点
//!   （远优于「每台机器各建 CA 再让运维全部信任」）；
//! - SAN 自动含 `localhost` / 回环 + **本机全部网卡 IPv4**，有效期 ≈10 年；
//! - **不做自动续签**（面板显示剩余天数 + 一键重签），避免运行中重建 TLS 监听的故障面；
//! - 私钥不进配置项、不落日志、Unix 权限 `0600`（由 [`super::tls`] 保证）。

use std::path::{Path, PathBuf};

/// 证书文件名。
pub const CERT_FILE: &str = "panel.pem";
/// 私钥文件名。
pub const KEY_FILE: &str = "panel.key";

/// 一次「确保证书可用」的结果。
#[derive(Debug, Clone)]
pub struct PanelCert {
    /// 证书链 PEM（供 `HttpServer::bind_tls`）。
    pub cert_pem: Vec<u8>,
    /// 私钥 PEM（供 `HttpServer::bind_tls`）。
    pub key_pem: Vec<u8>,
    /// 证书落盘路径（面板展示用）。
    pub cert_path: PathBuf,
    /// 是否**本次新生成**（调用方据此提示运维「导入一次信任」）。
    pub generated: bool,
}

/// 证书与私钥路径（`{base}/Config/keys/panel.{pem,key}`）。
///
/// # 参数
/// - `base`：部署根目录（面板配置文件所在目录的父级）。
///
/// # 返回值
/// `(证书路径, 私钥路径)`。
pub fn cert_paths(base: &Path) -> (PathBuf, PathBuf) {
    let dir = base.join("Config").join("keys");
    (dir.join(CERT_FILE), dir.join(KEY_FILE))
}

/// 确保证书存在（缺失自动生成）并加载 PEM；供启动时 `HttpServer::bind_tls` 使用。
///
/// # 参数
/// - `base`：部署根目录。
///
/// # 返回值
/// [`PanelCert`]；生成/加载失败返回可读错误（调用方应告警并**明确降级**为 HTTP，不要静默）。
pub fn ensure_pem(base: &Path) -> Result<PanelCert, String> {
    let (cert_path, key_path) = cert_paths(base);
    let generated = super::tls::ensure_self_signed_cert(&cert_path, &key_path, &[])
        .map_err(|e| format!("生成面板证书失败（{}）：{e}", cert_path.display()))?;
    let (cert_pem, key_pem) = super::tls::load_pem(&cert_path, &key_path)
        .map_err(|e| format!("加载面板证书失败（{}）：{e}", cert_path.display()))?;
    Ok(PanelCert {
        cert_pem,
        key_pem,
        cert_path,
        generated,
    })
}

/// 重新签发（删除旧证书后重建）；**需重启服务才生效**（监听层已持有旧证书）。
///
/// # 参数
/// - `base`：部署根目录。
///
/// # 返回值
/// 新的 [`PanelCert`]。
pub fn renew(base: &Path) -> Result<PanelCert, String> {
    let (cert_path, key_path) = cert_paths(base);
    super::tls::reset_cert(&cert_path, &key_path).map_err(|e| format!("删除旧证书失败：{e}"))?;
    ensure_pem(base)
}

/// 证书 PEM 原文（面板「下载证书」用；文件不存在时**顺带生成**，省掉"先启用再下载"的来回）。
///
/// # 参数
/// - `base`：部署根目录。
///
/// # 返回值
/// PEM 字节。
pub fn cert_pem_bytes(base: &Path) -> Result<Vec<u8>, String> {
    Ok(ensure_pem(base)?.cert_pem)
}

/// 一键导入信任的脚本（**单文件、内嵌证书**）。
///
/// # 参数
/// - `base`：部署根目录。
/// - `os`：`win`/`windows` → `.cmd`；其它 → `.sh`（macOS 走 security，Linux 走 update-ca-certificates）。
///
/// # 返回值
/// `(建议文件名, 脚本内容)`。
pub fn install_script(base: &Path, os: &str) -> Result<(String, String), String> {
    let pem = cert_pem_bytes(base)?;
    let os = os.trim().to_ascii_lowercase();
    if os == "win" || os == "windows" {
        Ok((
            "install-panel-cert.cmd".to_string(),
            super::tls::install_script_windows(&pem),
        ))
    } else {
        Ok((
            "install-panel-cert.sh".to_string(),
            super::tls::install_script_unix(&pem),
        ))
    }
}

/// 证书状态（面板展示用 JSON）。
///
/// 字段：`enabled`（配置开）/ `active`（本次运行**真的**在跑 TLS）/ `exists` / `path` /
/// `notAfter` / `daysLeft` / `sans` / `sha256` / `panelUrl` / `expiringSoon`。
/// 证书缺失或损坏时 `exists=false` 且其余为空，**不报错**（面板在明文 HTTP 下也必须可用）。
///
/// # 参数
/// - `base`：部署根目录。
/// - `enabled`：配置里是否启用 TLS。
/// - `active`：本次进程是否真的用 TLS 监听（证书加载失败时调用方传 `false`，便于运维一眼分辨
///   「配置开了但没生效」与「压根没开」）。
/// - `addr`：面板监听地址（如 `0.0.0.0:11880`），用于生成建议访问端口。
///
/// # 返回值
/// 状态 JSON。
pub fn status(base: &Path, enabled: bool, active: bool, addr: &str) -> serde_json::Value {
    let (cert_path, _) = cert_paths(base);
    let port = addr.rsplit(':').next().unwrap_or("0").trim();
    let mut out = serde_json::json!({
        "enabled": enabled,
        "active": active,
        "exists": false,
        "path": cert_path.display().to_string(),
        "notAfter": "",
        "daysLeft": 0,
        "sans": Vec::<String>::new(),
        "sha256": "",
        // 建议访问地址（只有真的在跑 TLS 才给 https）
        "panelUrl": format!("{}://127.0.0.1:{port}", if active { "https" } else { "http" }),
        "expiringSoon": false,
    });

    let Ok(text) = std::fs::read_to_string(&cert_path) else {
        return out;
    };
    let Some(info) = crate::cert::parse_certificate_pem(&text) else {
        return out;
    };

    let mut sans = info.dns_names.clone();
    sans.extend(info.ip_addresses.iter().cloned());
    let days = info.days_left(chrono::Utc::now().timestamp());

    out["exists"] = serde_json::json!(true);
    out["notAfter"] = serde_json::json!(crate::cert::format_unix_utc(info.not_after));
    out["daysLeft"] = serde_json::json!(days);
    out["sans"] = serde_json::json!(sans);
    out["sha256"] = serde_json::json!(info.sha256_hex.to_uppercase());
    // 到期前 30 天提示重签（与「无自动续签」的口径配套）
    out["expiringSoon"] = serde_json::json!(days <= 30);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_base(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "panel-cert-{}-{}-{tag}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn paths_are_under_config_keys() {
        let (cert, key) = cert_paths(Path::new("/tmp/x"));
        assert!(cert.ends_with("Config/keys/panel.pem"));
        assert!(key.ends_with("Config/keys/panel.key"));
    }

    /// 缺失时状态**不报错**；生成后 SAN 含回环、剩余天数≈10 年、指纹非空；重签仍可用。
    #[test]
    fn status_and_renew() {
        let base = temp_base("status");
        let v = status(&base, false, false, "0.0.0.0:11880");
        assert_eq!(v["exists"], false);
        assert_eq!(v["panelUrl"], "http://127.0.0.1:11880");

        let cert = ensure_pem(&base).expect("首次应生成");
        assert!(cert.generated, "首次应报告 generated=true");
        assert_eq!(ensure_pem(&base).expect("二次").generated, false, "已存在不应重生成");

        let v = status(&base, true, true, "0.0.0.0:11880");
        assert_eq!(v["exists"], true);
        assert_eq!(v["panelUrl"], "https://127.0.0.1:11880");
        assert!(v["daysLeft"].as_i64().unwrap() > 3600, "自签有效期应约 10 年");
        assert!(!v["sha256"].as_str().unwrap().is_empty());
        let sans = v["sans"].as_array().unwrap();
        assert!(sans.iter().any(|s| s == "127.0.0.1"), "SAN 必须含回环地址");

        let renew_cert = renew(&base).expect("重签应成功");
        assert!(renew_cert.generated);
        assert_eq!(status(&base, true, true, "0.0.0.0:11880")["exists"], true);

        // 下载与导入脚本
        assert!(!cert_pem_bytes(&base).unwrap().is_empty());
        let (name, script) = install_script(&base, "win").unwrap();
        assert_eq!(name, "install-panel-cert.cmd");
        assert!(script.contains("certutil") || script.contains("import"), "应包含导入命令");
        let (name, _) = install_script(&base, "linux").unwrap();
        assert_eq!(name, "install-panel-cert.sh");

        let _ = std::fs::remove_dir_all(&base);
    }
}
