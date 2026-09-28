//! 审计探针：对目标服务端做**协议正确性**核验（与性能无关，供 N005 审计使用）。
//!
//! 检查项：
//! 1) HTTP GET /ping → 200 且响应体与既定 JSON 逐字节一致
//! 2) HTTP POST /echo 1KB → 200 且回显字节逐一相等（含 Content-Length 正确）
//! 3) WS 原始握手（RFC 6455 已知向量）→ 101 且 Sec-WebSocket-Accept 一致
//! 4) WS 文本回显（dhrust 客户端，256B 掩码帧）→ 内容逐字节相等
//!
//! 用法: echo_probe <http_addr> <ws_addr>（同一地址服务两者时传两次同值）
//! 退出码：0 = 全部通过；1 = 存在失败项

use std::sync::{Arc, Mutex};
use std::time::Duration;

use dhrust::net::ws::{WsClient, WsClientOptions, WsHooks};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let http_addr = args.next().unwrap_or_else(|| "127.0.0.1:18084".into());
    let ws_addr = args.next().unwrap_or_else(|| "127.0.0.1:18094".into());

    let mut fails: u32 = 0;
    let mut report = |name: &str, ok: bool, detail: String| {
        if ok {
            println!("PASS  {name}");
        } else {
            fails += 1;
            println!("FAIL  {name} — {detail}");
        }
    };

    // ———— 1) GET /ping ————
    match http_request(&http_addr, b"GET /ping HTTP/1.1\r\nHost: bench\r\nConnection: close\r\n\r\n")
        .await
    {
        Ok((status, body)) => {
            let expect = b"{\"code\":0,\"msg\":\"ok\"}";
            report(
                "HTTP GET /ping 状态与响应体",
                status == 200 && body == expect,
                format!(
                    "status={status} body={:?}（期望 200 / {:?}）",
                    String::from_utf8_lossy(&body),
                    String::from_utf8_lossy(expect)
                ),
            );
        }
        Err(e) => report("HTTP GET /ping 状态与响应体", false, e),
    }

    // ———— 2) POST /echo 1KB（确定性图案，捕获任何字节损坏）————
    let payload: Vec<u8> = (0..1024).map(|i| (i as u8) ^ 0x5A).collect();
    let head = format!(
        "POST /echo HTTP/1.1\r\nHost: bench\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    );
    let mut req = head.into_bytes();
    req.extend_from_slice(&payload);
    match http_request(&http_addr, &req).await {
        Ok((status, body)) => report(
            "HTTP POST /echo 回显字节一致",
            status == 200 && body == payload,
            format!("status={status} 回显 {}B / 期望 {}B", body.len(), payload.len()),
        ),
        Err(e) => report("HTTP POST /echo 回显字节一致", false, e),
    }

    // ———— 3) WS 原始握手（RFC 6455 已知向量）————
    match raw_ws_handshake(&ws_addr).await {
        Ok((status_ok, accept_ok, detail)) => report(
            "WS 升级 101 + Accept 校验",
            status_ok && accept_ok,
            detail,
        ),
        Err(e) => report("WS 升级 101 + Accept 校验", false, e),
    }

    // ———— 4) WS 文本回显 256B（dhrust 客户端逐字节比对）————
    {
        let got: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let g = got.clone();
        let hooks = WsHooks {
            on_message: Some(Arc::new(move |m| {
                *g.lock().unwrap() = Some(m.text);
            })),
            ..Default::default()
        };
        let opts = WsClientOptions {
            connect_timeout: Duration::from_millis(1500),
            ping_interval: Duration::from_secs(60),
            ..Default::default()
        };
        let url = format!("ws://{ws_addr}/");
        let client = WsClient::connect(url, hooks, opts);
        let mut connected = false;
        for _ in 0..150 {
            if client.is_connected() {
                connected = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        if !connected {
            report("WS 文本回显 256B", false, "1.5s 内未连接".into());
        } else {
            let text: String = (0..256).map(|i| (b'a' + (i % 26) as u8) as char).collect();
            client.send_text(text.clone());
            let mut outcome: Option<Result<(), String>> = None;
            for _ in 0..200 {
                if let Some(v) = got.lock().unwrap().take() {
                    outcome = Some(if v == text {
                        Ok(())
                    } else {
                        Err(format!("回显不一致（收到 {}B）", v.len()))
                    });
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            match outcome {
                Some(Ok(())) => report("WS 文本回显 256B", true, String::new()),
                Some(Err(d)) => report("WS 文本回显 256B", false, d),
                None => report("WS 文本回显 256B", false, "2s 内未收到回显".into()),
            }
        }
        client.close();
    }

    println!(
        "\n== echo_probe 结果: {} ==",
        if fails == 0 {
            "全部通过".to_string()
        } else {
            format!("{fails} 项失败")
        }
    );
    std::process::exit(if fails == 0 { 0 } else { 1 });
}

/// 发一个 HTTP 请求（Connection: close），返回（状态码, body）。
async fn http_request(addr: &str, req: &[u8]) -> Result<(u16, Vec<u8>), String> {
    let mut sock = TcpStream::connect(addr)
        .await
        .map_err(|e| format!("connect: {e}"))?;
    sock.set_nodelay(true).ok();
    sock.write_all(req).await.map_err(|e| format!("write: {e}"))?;
    let mut buf = Vec::new();
    sock.read_to_end(&mut buf)
        .await
        .map_err(|e| format!("read: {e}"))?;
    let pos = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| "响应无空行".to_string())?;
    let head = String::from_utf8_lossy(&buf[..pos]).to_ascii_lowercase();
    let status: u16 = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Ok((status, buf[pos + 4..].to_vec()))
}

/// 原始 WS 握手：RFC 6455 已知向量（key dGhlIHNhbXBsZSBub25jZQ== → accept s3pPLMBiTxaQ9kYGzzhZRbK+xOo=）。
async fn raw_ws_handshake(addr: &str) -> Result<(bool, bool, String), String> {
    let mut sock = TcpStream::connect(addr)
        .await
        .map_err(|e| format!("connect: {e}"))?;
    let req = "GET / HTTP/1.1\r\nHost: bench\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n";
    sock.write_all(req.as_bytes())
        .await
        .map_err(|e| format!("write: {e}"))?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..pos]).to_string();
            let lower = head.to_ascii_lowercase();
            let status_ok = head.starts_with("HTTP/1.1 101");
            let accept_ok =
                lower.contains("sec-websocket-accept: s3pplmbitxaq9kygzzhzrbk+xoo=");
            return Ok((status_ok, accept_ok, head.replace(['\r', '\n'], " | ")));
        }
        if tokio::time::Instant::now() > deadline {
            return Err("2s 内未收到握手响应".into());
        }
        let n = sock
            .read(&mut chunk)
            .await
            .map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            return Err("对端提前关闭".into());
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}
