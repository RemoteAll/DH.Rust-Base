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

/// 证书有效期的最晚年份（x509 时间字段可表示的上限，9999-12-31）。
///
/// 年限**不设人为上限**（用户口径：年限自定义），只受这个格式上限约束；
/// 超出时返回明确错误，而不是悄悄夹到一个更短的值。
pub const MAX_CERT_YEAR: i32 = 9999;

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

/// 证书用途（决定扩展密钥用法 EKU）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CertPurpose {
    /// 服务端证书（`serverAuth`）：MQTT / WSS / HTTPS 服务端使用。
    Server,
    /// 客户端证书（`clientAuth`）：mTLS 双向认证时由客户端出示。
    Client,
}

/// 一张待签发证书的参数（CN / 有效期 / SAN / 用途）。
///
/// 与 [`ensure_self_signed_cert`] 的区别：这里是**由 CA 签发**的叶子参数，
/// 年限自定义、SAN 显式给定（不自动枚举本机网卡），供 MQTT TLS / mTLS 使用。
#[derive(Debug, Clone)]
pub struct CertSpec {
    /// 主题通用名（CN）。
    pub common_name: String,
    /// 有效期（天，必须 ≥ 1；**无人为上限**，仅受 x509 可表示的 9999 年约束）。
    pub days: i64,
    /// DNS SAN（域名）。
    pub dns_names: Vec<String>,
    /// IP SAN。
    pub ip_addresses: Vec<String>,
    /// 用途（服务端 / 客户端）。
    pub purpose: CertPurpose,
}

impl CertSpec {
    /// 新建一个证书参数；SAN 稍后由 [`CertSpec::with_sans`] 追加。
    pub fn new(common_name: impl Into<String>, days: i64, purpose: CertPurpose) -> Self {
        Self {
            common_name: common_name.into(),
            days,
            dns_names: Vec::new(),
            ip_addresses: Vec::new(),
            purpose,
        }
    }

    /// 追加 SAN（域名与 IP 分开给；自动去空白、去重）。
    pub fn with_sans(mut self, dns_names: Vec<String>, ip_addresses: Vec<String>) -> Self {
        self.dns_names = dns_names;
        self.ip_addresses = ip_addresses;
        self
    }

    /// 服务端证书参数（EKU = serverAuth）。
    pub fn server(common_name: impl Into<String>, days: i64) -> Self {
        Self::new(common_name, days, CertPurpose::Server)
    }

    /// 客户端证书参数（EKU = clientAuth）。
    pub fn client(common_name: impl Into<String>, days: i64) -> Self {
        Self::new(common_name, days, CertPurpose::Client)
    }
}

/// 一对 PEM 文本（证书 + 私钥）。
#[derive(Debug, Clone)]
pub struct CertPair {
    /// 证书 PEM（**不含** CA，只含叶子）。
    pub cert_pem: String,
    /// 私钥 PEM（PKCS#8）。
    pub key_pem: String,
}

/// 一套 CA：持有证书与私钥，可签发多张叶子证书（服务端 / 客户端）。
///
/// 只在内存里持有 rcgen 类型 —— 调用方永远不接触 `rcgen`，将来换实现不影响业务代码。
/// 典型用法：
/// 1. [`CertAuthority::generate`] 生成新 CA → 签发服务端与客户端证书；
/// 2. [`CertAuthority::load`] 从磁盘 PEM **恢复同一个 CA** → 只重签服务端证书（续期），
///    客户端信任的根 CA 不变，设备**不需要重新导入证书**。
pub struct CertAuthority {
    cert: rcgen::Certificate,
    key: rcgen::KeyPair,
}

impl CertAuthority {
    /// 生成一个新的 CA（`CA:TRUE`，KeyUsage = keyCertSign/cRLSign/digitalSignature）。
    ///
    /// # 参数
    /// - `common_name`：CA 名称（如 `HlktechIoT MQTT CA`）。
    /// - `days`：有效期（天，≥1；不设人为上限）。
    pub fn generate(common_name: &str, days: i64) -> io::Result<Self> {
        let key = rcgen::KeyPair::generate().map_err(|e| io::Error::other(format!("生成 CA 密钥失败: {e}")))?;
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("CA 参数失败: {e}")))?;
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, common_name.trim());
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params.key_usages = vec![
            rcgen::KeyUsagePurpose::KeyCertSign,
            rcgen::KeyUsagePurpose::CrlSign,
            rcgen::KeyUsagePurpose::DigitalSignature,
        ];
        set_validity(&mut params, days)?;
        let cert = params
            .self_signed(&key)
            .map_err(|e| io::Error::other(format!("签发 CA 证书失败: {e}")))?;
        Ok(Self { cert, key })
    }

    /// CA 证书 PEM（分发给客户端作为信任根）。
    pub fn cert_pem(&self) -> String {
        self.cert.pem()
    }

    /// 从磁盘上的 PEM 恢复同一个 CA（续期用）。
    ///
    /// # 参数
    /// - `cert_pem`：CA 证书 PEM（当初 [`CertAuthority::cert_pem`] 落盘的内容）。
    /// - `key_pem`：CA 私钥 PEM（当初 [`CertAuthority::key_pem`] 落盘的内容）。
    ///
    /// # 语义
    /// 只恢复「签发所需的信息」（主题、密钥、SKI、KeyUsage），不保证 DER 字节与原证书一致；
    /// 用它签出的新证书，与旧证书**同属一个信任根**（客户端无需重新导入）。
    pub fn load(cert_pem: &str, key_pem: &str) -> io::Result<Self> {
        let params = rcgen::CertificateParams::from_ca_cert_pem(cert_pem)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("解析 CA 证书失败: {e}")))?;
        let key = rcgen::KeyPair::from_pem(key_pem)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("解析 CA 私钥失败: {e}")))?;
        let cert = params
            .self_signed(&key)
            .map_err(|e| io::Error::other(format!("恢复 CA 失败: {e}")))?;
        Ok(Self { cert, key })
    }

    /// CA 私钥 PEM（**只落服务端**，不得下发）。
    pub fn key_pem(&self) -> String {
        self.key.serialize_pem()
    }

    /// 由本 CA 签发一张叶子证书。
    ///
    /// SAN 为空时自动用 CN 兜底（CN 像 IP 就写 IP SAN，否则写 DNS SAN）；
    /// 仍然为空则报错 —— 没有 SAN 的证书在现代客户端一律被拒。
    pub fn issue(&self, spec: &CertSpec) -> io::Result<CertPair> {
        let mut dns: Vec<String> = Vec::new();
        let mut ips: Vec<String> = Vec::new();
        for d in &spec.dns_names {
            let d = d.trim();
            if !d.is_empty() && !dns.iter().any(|x| x == d) {
                dns.push(d.to_string());
            }
        }
        for ip in &spec.ip_addresses {
            let ip = ip.trim();
            if !ip.is_empty() && !ips.iter().any(|x| x == ip) {
                ips.push(ip.to_string());
            }
        }
        if dns.is_empty() && ips.is_empty() {
            let cn = spec.common_name.trim();
            if cn.parse::<std::net::IpAddr>().is_ok() {
                ips.push(cn.to_string());
            } else if !cn.is_empty() {
                dns.push(cn.to_string());
            }
        }
        if dns.is_empty() && ips.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "证书至少需要一个 SAN（域名或 IP）",
            ));
        }

        // rcgen 的 `CertificateParams::new` 按字符串自动识别 IP / DNS。
        let mut sans = dns.clone();
        sans.extend(ips.iter().cloned());
        let key = rcgen::KeyPair::generate().map_err(|e| io::Error::other(format!("生成证书密钥失败: {e}")))?;
        let mut params = rcgen::CertificateParams::new(sans)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("证书参数失败: {e}")))?;
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, spec.common_name.trim());
        params.is_ca = rcgen::IsCa::NoCa;
        params.use_authority_key_identifier_extension = true;
        params.key_usages = vec![rcgen::KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = match spec.purpose {
            CertPurpose::Server => vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth],
            CertPurpose::Client => vec![rcgen::ExtendedKeyUsagePurpose::ClientAuth],
        };
        set_validity(&mut params, spec.days)?;
        let cert = params
            .signed_by(&key, &self.cert, &self.key)
            .map_err(|e| io::Error::other(format!("签发证书失败: {e}")))?;
        Ok(CertPair { cert_pem: cert.pem(), key_pem: key.serialize_pem() })
    }
}

/// 把「今天 + days 天」写进证书有效期（`not_before` 从今天 0 点起算）。
///
/// 年限不设人为上限，只校验「≥ 1 天」与「不超出 x509 可表示的 9999 年」。
fn set_validity(params: &mut rcgen::CertificateParams, days: i64) -> io::Result<()> {
    if days < 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("证书有效期至少 1 天（收到 {days}）"),
        ));
    }
    let now = chrono::Utc::now();
    let delta = chrono::Duration::try_days(days).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, format!("证书有效期过大：{days} 天"))
    })?;
    let expire = now.checked_add_signed(delta).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, format!("证书有效期过大：{days} 天"))
    })?;
    if expire.year() > MAX_CERT_YEAR {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "证书到期年份 {} 超出 x509 可表示范围（最晚 {MAX_CERT_YEAR}-12-31），请减小年限",
                expire.year()
            ),
        ));
    }
    params.not_before = rcgen::date_time_ymd(now.year(), now.month() as u8, now.day() as u8);
    params.not_after =
        rcgen::date_time_ymd(expire.year(), expire.month() as u8, expire.day() as u8);
    Ok(())
}

/// 把一对 PEM 写到 `{dir}/{name}.pem` 与 `{dir}/{name}.key`（Unix 下私钥 `0600`）。
///
/// # 返回值
/// `(证书路径, 私钥路径)`。
pub fn write_pem_pair(dir: &Path, name: &str, pair: &CertPair) -> io::Result<(std::path::PathBuf, std::path::PathBuf)> {
    std::fs::create_dir_all(dir)?;
    let cert_path = dir.join(format!("{name}.pem"));
    let key_path = dir.join(format!("{name}.key"));
    std::fs::write(&cert_path, &pair.cert_pem)?;
    std::fs::write(&key_path, &pair.key_pem)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600));
    }
    Ok((cert_path, key_path))
}

/// 安装进程级默认 `CryptoProvider`（ring），幂等。
///
/// # 为什么需要
/// 同一进程里 `dhrust`（面板 TLS）固定用 **ring**，而 `rmqtt-net` 在非 Windows 目标上
/// 启用 **aws-lc-rs**。两套 provider 同时编进同一个二进制时，rustls 无法自动决定默认值，
/// 任何走 `ServerConfig::builder()` / `ClientConfig::builder()` 的代码都会在运行期 panic
/// （`Could not automatically determine the process-level CryptoProvider`）。
/// 服务启动最早处调用本函数即可固定为 ring（显式传 provider 的代码不受影响）。
///
/// # 返回值
/// `true` = 本次安装成功；`false` = 已经装过（忽略即可，不视为错误）。
pub fn install_default_crypto_provider() -> bool {
    static ONCE: std::sync::Once = std::sync::Once::new();
    let mut installed = false;
    ONCE.call_once(|| {
        installed = tokio_rustls::rustls::crypto::ring::default_provider()
            .install_default()
            .is_ok();
    });
    installed
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

    /// CA 签发：服务端/客户端证书性质正确（CA:FALSE、EKU 用途、SAN、年限）。
    #[test]
    fn ca_issued_certs_have_expected_shape() {
        let ca = CertAuthority::generate("hlktech-test-ca", 400).unwrap();
        let ca_info = crate::cert::parse_certificate_pem(&ca.cert_pem()).expect("CA 应可解析");
        assert!(ca_info.is_ca, "CA 必须是 CA:TRUE，否则 WebPkiClientVerifier 拒绝");

        let server = ca
            .issue(
                &CertSpec::server("mqtt.example.com", 30)
                    .with_sans(vec!["mqtt.example.com".into()], vec!["10.1.2.3".into()]),
            )
            .unwrap();
        let s = crate::cert::parse_certificate_pem(&server.cert_pem).expect("服务端证书应可解析");
        assert!(!s.is_ca, "服务端证书必须是叶子证书");
        assert!(s.covers_host("mqtt.example.com"));
        assert!(s.covers_ip("10.1.2.3"));
        let days = (s.not_after - chrono::Utc::now().timestamp()) / 86_400;
        assert!((29..=30).contains(&days), "年限应自定义为 30 天，实际 {days}");

        // 客户端证书：SAN 为空时用 CN 兜底（CN 像主机名 → DNS SAN）
        let client = ca.issue(&CertSpec::client("device-0001", 30)).unwrap();
        let c = crate::cert::parse_certificate_pem(&client.cert_pem).expect("客户端证书应可解析");
        assert!(!c.is_ca);
        assert!(c.covers_host("device-0001"));
        assert!(client.key_pem.contains("PRIVATE KEY"));

        // 完全无 SAN 且 CN 为空 → 明确报错（没有 SAN 的证书现代客户端一律拒）
        let err = ca
            .issue(&CertSpec::client("", 30))
            .expect_err("空 CN + 空 SAN 必须报错");
        assert!(err.to_string().contains("SAN"), "{err}");
    }

    /// 落盘：文件名约定 + 私钥权限（Unix 0600）。
    #[test]
    fn write_pem_pair_uses_fixed_layout_and_protects_key() {
        let dir = std::env::temp_dir().join(format!("tls-pair-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let pair = CertPair {
            cert_pem: "CERT\n".to_string(),
            key_pem: "KEY\n".to_string(),
        };
        let (cert, key) = write_pem_pair(&dir, "server", &pair).unwrap();
        assert!(cert.ends_with("server.pem") && key.ends_with("server.key"));
        assert_eq!(std::fs::read_to_string(&cert).unwrap(), "CERT\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&key).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "私钥必须仅属主可读");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **续期**：从磁盘恢复同一个 CA 后重签服务端证书，原 CA 信任关系不变。
    ///
    /// 证明方式：客户端只信任「第一次生成时落盘的 CA 证书」，服务端换成「重新加载 CA 后签发的新证书」，
    /// 仍然能完成完整 TLS 校验（说明续期不会让设备重新导入证书）。
    #[tokio::test]
    async fn ca_reload_can_renew_server_cert_under_same_root() {
        use crate::net::http::{handler, HttpOutcome, HttpRequest, HttpResponse, HttpServer};
        use crate::net::http_client::{get, HttpClientOptions};

        let ca = CertAuthority::generate("renew-ca", 400).unwrap();
        let ca_pem = ca.cert_pem();
        let ca_key = ca.key_pem();
        let _first = ca
            .issue(&CertSpec::server("renew.local", 30).with_sans(vec![], vec!["127.0.0.1".into()]))
            .unwrap();

        // 模拟「面板重启后重新加载 CA」：只剩 PEM 文件，没有内存对象
        let reloaded = CertAuthority::load(&ca_pem, &ca_key).expect("CA 应能从 PEM 恢复");
        let renewed = reloaded
            .issue(&CertSpec::server("renew.local", 90).with_sans(vec![], vec!["127.0.0.1".into()]))
            .unwrap();
        let info = crate::cert::parse_certificate_pem(&renewed.cert_pem).unwrap();
        let days = (info.not_after - chrono::Utc::now().timestamp()) / 86_400;
        assert!((89..=90).contains(&days), "续期年限应可自定义，实际 {days}");

        let server = HttpServer::bind_tls(
            "127.0.0.1:0",
            renewed.cert_pem.as_bytes(),
            renewed.key_pem.as_bytes(),
        )
        .await
        .unwrap();
        let port = server.local_addr().unwrap().port();
        tokio::spawn(server.serve(handler(|_req: HttpRequest| async move {
            HttpOutcome::Response(HttpResponse::text(200, "renewed-ok"))
        })));

        // 客户端仍只信任**原** CA 证书
        let resp = get(
            &format!("https://127.0.0.1:{port}/ping"),
            &[],
            &HttpClientOptions { ca_pem: Some(ca_pem.into_bytes()), ..Default::default() },
        )
        .await
        .expect("续期后的证书应被原 CA 信任");
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body_text(), "renewed-ok");
    }

    /// **mTLS 端到端**：CA 签发的服务端证书 + 客户端证书能完成双向握手并传数据。
    ///
    /// 这条用例是「面板签发出来的证书真能用」的唯一可靠证明：
    /// 只解析 PEM 字段不代表链能被 rustls 接受（EKU / KeyUsage / AKI 任一写错都会失败）。
    #[tokio::test]
    async fn ca_signed_certs_complete_mutual_tls_handshake() {
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        use tokio_rustls::rustls::pki_types::ServerName;
        use tokio_rustls::rustls::server::WebPkiClientVerifier;
        use tokio_rustls::rustls::{ClientConfig, RootCertStore, ServerConfig};
        use tokio_rustls::{TlsAcceptor, TlsConnector};

        let ca = CertAuthority::generate("hlktech-mtls-ca", 31).unwrap();
        let server = ca
            .issue(
                &CertSpec::server("localhost", 30)
                    .with_sans(vec!["localhost".into()], vec!["127.0.0.1".into()]),
            )
            .unwrap();
        let client = ca.issue(&CertSpec::client("device-1", 30)).unwrap();

        let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
        let ca_roots = || {
            let mut roots = RootCertStore::empty();
            for c in rustls_pemfile::certs(&mut ca.cert_pem().as_bytes()) {
                roots.add(c.unwrap()).unwrap();
            }
            roots
        };
        // 服务端：要求客户端证书，且只信任本 CA
        let verifier =
            WebPkiClientVerifier::builder_with_provider(ca_roots().into(), provider.clone())
                .build()
                .unwrap();
        let server_cfg = ServerConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_client_cert_verifier(verifier)
            .with_single_cert(
                rustls_pemfile::certs(&mut server.cert_pem.as_bytes())
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap(),
                rustls_pemfile::private_key(&mut server.key_pem.as_bytes())
                    .unwrap()
                    .unwrap(),
            )
            .unwrap();
        // 客户端：信任 CA，并出示客户端证书
        let client_cfg = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(ca_roots())
            .with_client_auth_cert(
                rustls_pemfile::certs(&mut client.cert_pem.as_bytes())
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap(),
                rustls_pemfile::private_key(&mut client.key_pem.as_bytes())
                    .unwrap()
                    .unwrap(),
            )
            .unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(server_cfg));
        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut tls = acceptor.accept(stream).await.unwrap();
            let mut buf = [0u8; 4];
            tls.read_exact(&mut buf).await.unwrap();
            tls.write_all(b"pong").await.unwrap();
            tls.flush().await.unwrap();
            String::from_utf8_lossy(&buf).to_string()
        });

        let connector = TlsConnector::from(Arc::new(client_cfg));
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let name = ServerName::try_from("127.0.0.1").unwrap();
        let mut tls = connector.connect(name, stream).await.expect("双向握手应成功");
        tls.write_all(b"ping").await.unwrap();
        tls.flush().await.unwrap();
        let mut buf = [0u8; 4];
        tls.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"pong");
        assert_eq!(server_task.await.unwrap(), "ping");
    }
}
