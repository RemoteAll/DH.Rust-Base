//! TLS 服务端支持（feature `net-tls`）：自签证书生成、PEM 加载与服务端配置构建。
//!
//! 场景：Agent 对齐 C# Kestrel 的 `https://*:8283` 能力——控制面 HTTPS 监听。
//! 证书策略：缺失自动生成自签（10 年、SAN 含 localhost/回环 + 调用方附加名）；
//! 调用方（DHDeploy Client/Server）均忽略证书校验（对齐 C#
//! `ServerCertificateCustomValidationCallback`），自签即可全链路互通。

use std::io;
use std::path::Path;
use std::sync::Arc;

use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::ServerConfig;

/// 确保证书/私钥文件存在（缺失则生成自签）；返回是否本次新生成。
///
/// SAN 基础项：`localhost`、`127.0.0.1`、`::1`；`extra_sans` 追加主机名/IP。
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
    for s in extra_sans {
        if !s.trim().is_empty() && !sans.contains(s) {
            sans.push(s.clone());
        }
    }

    let key_pair = rcgen::KeyPair::generate()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("生成密钥失败: {e}")))?;
    let mut params = rcgen::CertificateParams::new(sans)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("证书参数失败: {e}")))?;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "dhdeploy-agent");
    params.not_after = rcgen::date_time_ymd(2036, 1, 1);

    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("签发自签证书失败: {e}")))?;

    std::fs::write(cert_path, cert.pem())?;
    std::fs::write(key_path, key_pair.serialize_pem())?;
    Ok(true)
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
}
