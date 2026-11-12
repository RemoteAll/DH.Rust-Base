#![cfg(feature = "net")]
//! 静态文件端到端：磁盘文件**流式发送**（不整读进内存）、未知扩展名默认 404、
//! 条件请求 304 零正文、1 MB 文件完整回传。
//!
//! 单列一个测试二进制（而不是放进 `net_http.rs`）：本用例会起服务端并传输 1 MB，
//! 与 lib 单测里对 CPU 采样基线的时序敏感用例并行时会相互干扰。

use std::net::SocketAddr;
use std::time::Duration;

use dhrust::net::http::{HttpOutcome, HttpResponse, HttpServer, HttpServerOptions};
use dhrust::net::router::{route, Router};
use dhrust::net::static_files::StaticFiles;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

async fn start(router: Router) -> SocketAddr {
    let server = HttpServer::bind("127.0.0.1:0").await.unwrap();
    let addr = server.local_addr().unwrap();
    let svc = router.into_handler();
    tokio::spawn(async move {
        let _ = server.serve_with(svc, HttpServerOptions::default()).await;
    });
    addr
}

/// 裸 TCP 请求（原样返回响应体字节）。
async fn http_call_raw(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> (u16, String, Vec<u8>) {
    let mut tcp = TcpStream::connect(addr).await.unwrap();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("Content-Length: 0\r\n\r\n");
    tcp.write_all(req.as_bytes()).await.unwrap();

    let mut buf: Vec<u8> = Vec::new();
    tcp.read_to_end(&mut buf).await.unwrap();
    let split = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
        .unwrap_or(buf.len());
    let head = String::from_utf8_lossy(&buf[..split]).into_owned();
    let body_raw = buf[split..].to_vec();
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    (status, head, body_raw)
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "dhrust-static-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn static_files_stream_and_block_unknown_extensions() {
    let dir = temp_dir("e2e");
    std::fs::write(dir.join("app.js"), "console.log(1)").unwrap();
    std::fs::write(dir.join("data.bak"), "secret").unwrap();

    let statics = StaticFiles::new(&dir);
    let mut r = Router::new();
    r.fallback(route(move |ctx| {
        let statics = statics.clone();
        let req = ctx.req.clone();
        async move {
            statics
                .try_serve_request(&req)
                .map(HttpOutcome::Response)
                .unwrap_or_else(|| HttpOutcome::Response(HttpResponse::text(404, "Not Found")))
        }
    }));
    let addr = start(r).await;

    // 正常资源：200 + Content-Length + 正文
    let (status, head, body) = http_call_raw(addr, "GET", "/app.js", &[]).await;
    assert_eq!(status, 200);
    assert!(
        head.to_ascii_lowercase().contains("content-length: 14"),
        "应带准确 Content-Length：{head}"
    );
    assert_eq!(body, b"console.log(1)");

    // 未知扩展名：默认不服务
    let (status, _, _) = http_call_raw(addr, "GET", "/data.bak", &[]).await;
    assert_eq!(status, 404);

    // 条件请求：304 零正文
    let etag = head
        .lines()
        .find(|line| line.to_ascii_lowercase().starts_with("etag:"))
        .and_then(|line| line.splitn(2, ':').nth(1))
        .map(|value| value.trim().to_string())
        .expect("静态响应应带 ETag");
    let (status, _, body) =
        http_call_raw(addr, "GET", "/app.js", &[("If-None-Match", etag.as_str())]).await;
    assert_eq!(status, 304);
    assert!(body.is_empty());

    // 1 MB 文件：流式分块完整回传（走 FileStreamBody）
    let big = vec![b'x'; 1024 * 1024];
    std::fs::write(dir.join("big.txt"), &big).unwrap();
    let (status, head, body) = http_call_raw(addr, "GET", "/big.txt", &[]).await;
    assert_eq!(status, 200);
    assert!(
        head.to_ascii_lowercase()
            .contains(&format!("content-length: {}", big.len())),
        "大文件应带准确 Content-Length：{head}"
    );
    assert_eq!(body.len(), big.len());

    // 服务端已就绪，避免 remove_dir_all 与发送竞争（简单等待，非断言）
    tokio::time::sleep(Duration::from_millis(10)).await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// 预压缩（零运行时 CPU）：磁盘同名 `.gz` 与嵌入 gzip 变体都按 `Accept-Encoding` 协商。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn static_files_precompressed_gzip() {
    let dir = temp_dir("gz");
    std::fs::write(dir.join("app.js"), "plain-js-body").unwrap();
    std::fs::write(dir.join("app.js.gz"), "GZIP-BYTES").unwrap();
    std::fs::write(dir.join("only.css"), "plain-css").unwrap();

    let statics = StaticFiles::new(&dir);
    let mut r = Router::new();
    r.fallback(route(move |ctx| {
        let statics = statics.clone();
        let req = ctx.req.clone();
        async move {
            statics
                .try_serve_request(&req)
                .map(HttpOutcome::Response)
                .unwrap_or_else(|| HttpOutcome::Response(HttpResponse::text(404, "Not Found")))
        }
    }));
    let addr = start(r).await;

    // ① 接受 gzip：下发 .gz 字节 + 编码头 + Vary，Content-Type 仍按原始扩展名
    let (status, head, body) =
        http_call_raw(addr, "GET", "/app.js", &[("Accept-Encoding", "gzip, deflate")]).await;
    assert_eq!(status, 200);
    let lower_head = head.to_ascii_lowercase();
    assert!(
        lower_head.contains("content-encoding: gzip"),
        "应带 Content-Encoding: gzip：{head}"
    );
    assert!(
        lower_head.contains("vary: accept-encoding"),
        "应带 Vary: Accept-Encoding：{head}"
    );
    assert!(
        lower_head.contains("content-type: text/javascript"),
        "Content-Type 应按原始扩展名推断：{head}"
    );
    assert_eq!(body, b"GZIP-BYTES");

    // ② 不接受 gzip：原文件，无编码头
    let (status, head, body) = http_call_raw(addr, "GET", "/app.js", &[]).await;
    assert_eq!(status, 200);
    assert!(!head.to_ascii_lowercase().contains("content-encoding"));
    assert_eq!(body, b"plain-js-body");

    // ③ 没有同名 .gz：回退原文件（不报错、不加头）
    let (status, head, body) =
        http_call_raw(addr, "GET", "/only.css", &[("Accept-Encoding", "gzip")]).await;
    assert_eq!(status, 200);
    assert!(!head.to_ascii_lowercase().contains("content-encoding"));
    assert_eq!(body, b"plain-css");

    tokio::time::sleep(Duration::from_millis(10)).await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// 单 exe 形态：构建期嵌入的 gzip 变体同样能按 `Accept-Encoding` 下发。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn embedded_gzip_variant_is_served() {
    static PLAIN: &[u8] = b"plain-body";
    static GZ: &[u8] = b"GZIP-BODY";

    let statics = StaticFiles::new(std::env::temp_dir().join("dhrust-nonexistent-root"))
        .embed_many_gzip(&[("app.js", PLAIN, GZ)]);
    let mut r = Router::new();
    r.fallback(route(move |ctx| {
        let statics = statics.clone();
        let req = ctx.req.clone();
        async move {
            statics
                .try_serve_request(&req)
                .map(HttpOutcome::Response)
                .unwrap_or_else(|| HttpOutcome::Response(HttpResponse::text(404, "Not Found")))
        }
    }));
    let addr = start(r).await;

    let (status, head, body) =
        http_call_raw(addr, "GET", "/app.js", &[("Accept-Encoding", "gzip")]).await;
    assert_eq!(status, 200);
    assert!(head.to_ascii_lowercase().contains("content-encoding: gzip"));
    assert_eq!(body, b"GZIP-BODY");

    let (status, head, body) = http_call_raw(addr, "GET", "/app.js", &[]).await;
    assert_eq!(status, 200);
    assert!(!head.to_ascii_lowercase().contains("content-encoding"));
    assert_eq!(body, b"plain-body");
}