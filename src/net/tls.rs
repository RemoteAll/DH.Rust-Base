//! TLS 服务端支持（feature `net-tls`）：自签证书生成、PEM 加载与服务端配置构建。
//!
//! 场景：Agent 对齐 C# Kestrel 的 `https://*:8283` 能力——控制面 HTTPS 监听。
//! 证书策略：缺失自动生成自签**叶子**证书（`CA:FALSE`，10 年）；
//! SAN 自动包含 `localhost` / `127.0.0.1` / `::1` + **本机全部网卡 IPv4** + 调用方附加名，
//! 因此浏览器直接用 `https://内网IP:端口` 访问也能匹配（仅需导入一次证书信任）。
//!
//! 安全取舍：
//! - 叶子证书 `CA:FALSE`：即使私钥泄露，也**无法签发其他站点证书**（比自建 CA 安全得多）；
//! - 私钥文件在 Unix 上权限 `0600`；
//! - 不做自动续签：到期前由面板提示「重新签发」（避免运行中重建监听带来的故障面）。
//!
//! 调用方（DHDeploy Client/Server）均忽略证书校验（对齐 C#
//! `ServerCertificateCustomValidationCallback`），自签即可全链路互通。

use std::io;
use std::path::Path;
use std::sync::Arc;

use chrono::Datelike;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::ServerConfig;

/// 自签证书有效期（天）；`3653` ≈ 10 年（含闰日）。
const SELF_SIGNED_DAYS: i64 = 3653;

/// 确保证书/私钥文件存在（缺失则生成自签）；返回是否本次新生成。
///
/// SAN 基础项：`localhost`、`127.0.0.1`、`::1` + **本机全部网卡 IPv4**；
/// `extra_sans` 追加主机名/IP（去重）。
pub fn ensure_self_signed_cert(
    cert_path: &Path,
    key_path: &Path,
    extra_sans: &[String],
) -> io::Result<bool> {
    if cert_path.is_file() && key_path.is_file() {
        return Ok(false);
    }
    if let Some(dir) = cert_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if let Some(dir) = key_path.parent() {
        std::fs::create_dir_all(dir)?;
    }

    let mut sans = vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
    ];
    // 本机全部网卡 IPv4：浏览器按内网 IP 直访问时必须写入 SAN（现代浏览器不看 CN）
    for ip in local_ipv4() {
        if !sans.contains(&ip) {
            sans.push(ip);
        }
    }
    for s in extra_sans {
        let s = s.trim();
        if !s.is_empty() && !sans.iter().any(|x| x == s) {
            sans.push(s.to_string());
        }
    }

    let key_pair =
        rcgen::KeyPair::generate().map_err(|e| io::Error::other(format!("生成密钥失败: {e}")))?;
    let mut params = rcgen::CertificateParams::new(sans)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("证书参数失败: {e}")))?;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "dhdeploy-agent");
    // 有效期从**签发时刻**起算（此前硬编码 2036 年，到期后将无法再生成有效证书）
    let expire = chrono::Utc::now() + chrono::Duration::days(SELF_SIGNED_DAYS);
    params.not_after =
        rcgen::date_time_ymd(expire.year(), expire.month() as u8, expire.day() as u8);

    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| io::Error::other(format!("签发自签名证书失败: {e}")))?;

    std::fs::write(cert_path, cert.pem())?;
    std::fs::write(key_path, key_pair.serialize_pem())?;
    // 私钥仅属主可读（Unix；Windows 依赖目录 ACL）
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(key_path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(true)
}

/// 本机全部 IPv4（含回环；去重）；枚举失败时返回空（不影响生成）。
fn local_ipv4() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for itf in crate::sys::machine::network_interfaces() {
        for ip in itf.ips {
            if !out.iter().any(|x| x == &ip) {
                out.push(ip);
            }
        }
    }
    out
}

/// 现有证书是否覆盖指定主机（域名**或** IP）。
///
/// 用途：换了网卡/网段后判断旧证书是否需重签（不覆盖时浏览器会报“证书名称不匹配”）。
pub fn cert_covers(cert_path: &Path, host: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(cert_path) else {
        return false;
    };
    crate::cert::parse_certificate_pem(&text)
        .map(|info| info.covers_host(host))
        .unwrap_or(false)
}

/// 删除证书与私钥（“重新签发”用；下次启动/下次加载时自动重建）。
pub fn reset_cert(cert_path: &Path, key_path: &Path) -> io::Result<()> {
    for p in [cert_path, key_path] {
        match std::fs::remove_file(p) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// 生成 **Windows 一键导入证书** 批处理脚本（`.cmd`，CRLF，证书以 base64 内嵌）。
///
/// 为什么需要它：自签证书的信任必须建立在**发起访问的那台电脑**上（操作系统安全边界，
/// 服务端无法代劳）。把「找文件 → 双击 → 选存储 → 多步向导」压缩成"双击 + UAC 确认一次"。
///
/// 行为：先导入「当前用户 → 受信任的根证书颁发机构」（**无需管理员**），
/// 再尝试「本机 → 受信任的根证书颁发机构」（需管理员，失败不影响第一步）。
/// 末尾提醒 Firefox 使用独立证书库需单独导入。
pub fn install_script_windows(cert_pem: &[u8]) -> String {
    let b64 = crate::sign::base64_encode(cert_pem);
    let body = format!(
        r#"@echo off
setlocal
chcp 65001 >nul
title 导入面板自签证书
set "B64={b64}"
set "CRT=%TEMP%\panel-cert-%RANDOM%.crt"
echo 正在释放证书...
powershell -NoProfile -Command "[IO.File]::WriteAllBytes('%CRT%', [Convert]::FromBase64String('%B64%'))"
if not exist "%CRT%" (
  echo [错误] 释放证书失败，请确认系统支持 PowerShell。
  pause
  exit /b 1
)
echo.
echo [1/2] 导入到「当前用户 -^> 受信任的根证书颁发机构」（无需管理员）
certutil -user -addstore -f Root "%CRT%" >nul 2>&1 && echo   完成 || echo   失败（可手动双击证书导入）
echo.
echo [2/2] 导入到「本机 -^> 受信任的根证书颁发机构」（需管理员；失败不影响第 1 步）
certutil -addstore -f Root "%CRT%" >nul 2>&1 && echo   完成 || echo   未获得管理员权限（可右键以管理员身份运行本文件重试）
del "%CRT%" >nul 2>&1
echo.
echo 全部完成。请**完全退出并重新启动浏览器**（Chrome / Edge）后再打开面板。
echo 注意：Firefox 使用独立证书库，需在 设置 -^> 隐私与安全 -^> 证书 -^> 查看证书 -^> 导入 手动导入面板证书。
echo.
pause
"#
    );
    // .cmd 必须 CRLF（混入 LF 会让批处理解析出错）
    body.replace('\n', "\r\n")
}

/// 生成 **Linux/macOS 一键导入证书** 脚本（`.sh`，证书以 base64 内嵌）。
///
/// macOS 走 `security add-trusted-cert`（系统钥匙串），Linux 走
/// `/usr/local/share/ca-certificates` + `update-ca-certificates`；两者都需要 sudo。
pub fn install_script_unix(cert_pem: &[u8]) -> String {
    let b64 = crate::sign::base64_encode(cert_pem);
    format!(
        r#"#!/bin/sh
# 一键导入面板自签证书（单文件，证书已内嵌）
set -e
B64='{b64}'
TMP="$(mktemp /tmp/panel-cert.XXXXXX.crt)"
if base64 -d </dev/null >/dev/null 2>&1; then
  DEC="base64 -d"
else
  DEC="base64 -D"
fi
printf '%s' "$B64" | $DEC > "$TMP"
echo "证书已释放到 $TMP"
if [ "$(uname -s)" = "Darwin" ]; then
  echo "需要管理员密码（导入到 macOS 系统钥匙串）..."
  sudo security add-trusted-cert -d -r trustRoot -k /Library/Keychains/System.keychain "$TMP"
  echo "完成：已导入 macOS 系统钥匙串"
else
  echo "需要管理员密码（导入到系统 CA 目录）..."
  sudo cp "$TMP" /usr/local/share/ca-certificates/panel-selfsigned.crt
  sudo update-ca-certificates
  echo "完成：已导入系统 CA（update-ca-certificates）"
fi
rm -f "$TMP"
echo
echo "请重新启动浏览器后再打开面板。"
echo "注意：Firefox 使用独立证书库，需在 首选项 -> 隐私与安全 -> 证书 -> 查看证书 -> 导入 手动导入。"
"#
    )
}

/// 加载 PEM 证书链与私钥（原始字节，供 [`crate::net::http::HttpServer::bind_tls`] 使用）。
pub fn load_pem(cert_path: &Path, key_path: &Path) -> io::Result<(Vec<u8>, Vec<u8>)> {
    let cert = std::fs::read(cert_path)?;
    let key = std::fs::read(key_path)?;
    Ok((cert, key))
}

/// 构建 rustls 服务端配置（供 HTTP 服务端使用；ring 后端与全仓一致）。
pub(crate) fn server_config(cert_pem: &[u8], key_pem: &[u8]) -> io::Result<ServerConfig> {
    let mut cert_reader = cert_pem;
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cert_reader)
        .collect::<Result<_, _>>()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("证书解析失败: {e}")))?;
    if certs.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "证书为空"));
    }
    let mut key_reader = key_pem;
    let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut key_reader)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("私钥解析失败: {e}")))?
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "私钥为空"))?;

    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    let config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("TLS 配置失败: {e}")))?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("证书装载失败: {e}")))?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Windows 一键导入脚本：内嵌 base64 证书、双存储导入、CRLF、含 Firefox 提示。
    #[test]
    fn install_script_windows_is_valid_cmd() {
        let s = install_script_windows(
            b"-----BEGIN CERTIFICATE-----\nQUJD\n-----END CERTIFICATE-----\n",
        );
        assert!(s.contains("set \"B64="), "证书必须 base64 内嵌");
        assert!(
            s.contains("certutil -user -addstore -f Root"),
            "先导入当前用户（免管理员）"
        );
        assert!(s.contains("certutil -addstore -f Root"), "再尝试本机级存储");
        assert!(s.contains("Firefox"), "必须提醒 Firefox 用自己的证书库");
        assert!(s.contains("\r\n"));
        assert!(
            !s.replace("\r\n", "").contains('\n'),
            "存在裸 LF，.cmd 必须是 CRLF"
        );
    }

    /// Linux/macOS 脚本：shebang、内嵌证书、双平台分支、macOS base64 -D 回退。
    #[test]
    fn install_script_unix_covers_darwin_and_linux() {
        let s =
            install_script_unix(b"-----BEGIN CERTIFICATE-----\nQUJD\n-----END CERTIFICATE-----\n");
        assert!(s.starts_with("#!/bin/sh"));
        assert!(s.contains("B64='"), "证书必须 base64 内嵌");
        assert!(
            s.contains("security add-trusted-cert"),
            "macOS 导入系统钥匙串"
        );
        assert!(s.contains("update-ca-certificates"), "Linux 导入系统 CA");
        assert!(s.contains("base64 -D"), "macOS 的 base64 是大写 -D");
        assert!(s.contains("Firefox"));
    }

    #[test]
    fn ensure_and_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!("tls-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cert = dir.join("agent.pem");
        let key = dir.join("agent.key");
        let _ = std::fs::remove_file(&cert);
        let _ = std::fs::remove_file(&key);

        // 首次生成
        assert!(ensure_self_signed_cert(&cert, &key, &["agent.local".to_string()]).unwrap());
        assert!(cert.is_file() && key.is_file());
        // 二次调用不重复生成
        assert!(!ensure_self_signed_cert(&cert, &key, &[]).unwrap());

        // 加载 + 构建服务端配置
        let (cert_pem, key_pem) = load_pem(&cert, &key).unwrap();
        assert!(server_config(&cert_pem, &key_pem).is_ok());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 自签**叶子**证书：SAN 含本机网卡 IP、非 CA、有效期≈10 年；覆盖判定与重新签发可用。
    ///
    /// 这些性质直接决定“浏览器用 `https://内网IP:端口` 打开是否报证书错误”。
    #[test]
    fn self_signed_cert_covers_local_ip_and_is_not_ca() {
        let dir = std::env::temp_dir().join(format!("tls-sans-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cert = dir.join("c.pem");
        let key = dir.join("c.key");
        let _ = std::fs::remove_file(&cert);
        let _ = std::fs::remove_file(&key);

        assert!(ensure_self_signed_cert(&cert, &key, &[]).unwrap());

        let text = std::fs::read_to_string(&cert).unwrap();
        let info = crate::cert::parse_certificate_pem(&text).expect("证书应可解析");
        assert!(
            !info.is_ca,
            "自签必须是叶子证书（CA:FALSE），私钥泄露也无法签其他站点"
        );
        assert!(info.covers_host("127.0.0.1"));
        assert!(info.covers_host("localhost"));
        for ip in local_ipv4() {
            assert!(info.covers_ip(&ip), "SAN 应包含本机 IP {ip}");
            assert!(cert_covers(&cert, &ip), "cert_covers 应判定覆盖 {ip}");
        }
        assert!(
            !cert_covers(&cert, "10.99.99.99"),
            "未写入 SAN 的 IP 不应判定覆盖"
        );

        let days = (info.not_after - chrono::Utc::now().timestamp()) / 86_400;
        assert!(
            (3600..=3700).contains(&days),
            "有效期应约 10 年，实际 {days} 天"
        );

        // 重新签发：删除后再次生成（面板「重新签发」走同一路径）
        reset_cert(&cert, &key).unwrap();
        assert!(!cert.exists() && !key.exists());
        assert!(ensure_self_signed_cert(&cert, &key, &[]).unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn bind_tls_end_to_end() {
        use crate::net::http::{handler, HttpOutcome, HttpRequest, HttpResponse, HttpServer};
        use crate::net::http_client::{get, HttpClientOptions};

        let dir = std::env::temp_dir().join(format!("tls-e2e-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cert_path = dir.join("c.pem");
        let key_path = dir.join("c.key");
        let _ = std::fs::remove_file(&cert_path);
        let _ = std::fs::remove_file(&key_path);
        ensure_self_signed_cert(&cert_path, &key_path, &[]).unwrap();
        let (cert, key) = load_pem(&cert_path, &key_path).unwrap();

        // HTTPS 服务端（自签）→ 客户端忽略证书校验访问（对齐 DHDeploy 全链路）
        let server = HttpServer::bind_tls("127.0.0.1:0", &cert, &key)
            .await
            .unwrap();
        let port = server.local_addr().unwrap().port();
        tokio::spawn(server.serve(handler(|req: HttpRequest| async move {
            HttpOutcome::Response(HttpResponse::text(200, format!("tls-ok:{}", req.path)))
        })));

        let resp = get(
            &format!("https://127.0.0.1:{port}/ping"),
            &[],
            &HttpClientOptions {
                insecure_tls: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body_text(), "tls-ok:/ping");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn self_signed_https_with_custom_ca_pem() {
        use crate::net::http::{handler, HttpOutcome, HttpRequest, HttpResponse, HttpServer};
        use crate::net::http_client::{get, HttpClientOptions};

        let dir = std::env::temp_dir().join(format!("tls-ca-e2e-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cert_path = dir.join("c.pem");
        let key_path = dir.join("c.key");
        let _ = std::fs::remove_file(&cert_path);
        let _ = std::fs::remove_file(&key_path);
        ensure_self_signed_cert(&cert_path, &key_path, &[]).unwrap();
        let (cert, key) = load_pem(&cert_path, &key_path).unwrap();
        let ca_pem = std::fs::read(&cert_path).unwrap();

        let server = HttpServer::bind_tls("127.0.0.1:0", &cert, &key)
            .await
            .unwrap();
        let port = server.local_addr().unwrap().port();
        tokio::spawn(server.serve(handler(|_req: HttpRequest| async move {
            HttpOutcome::Response(HttpResponse::text(200, "ca-ok"))
        })));

        // ① 提供自定义根证书（ca_pem）→ 正常校验通过（无需 insecure_tls）
        let resp = get(
            &format!("https://127.0.0.1:{port}/ping"),
            &[],
            &HttpClientOptions {
                ca_pem: Some(ca_pem.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body_text(), "ca-ok");

        // ② 不提供（默认系统/内置根）→ 自签证书应校验失败
        let err = get(
            &format!("https://127.0.0.1:{port}/ping"),
            &[],
            &HttpClientOptions::default(),
        )
        .await;
        assert!(err.is_err(), "自签证书不在默认根中应失败：{err:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
