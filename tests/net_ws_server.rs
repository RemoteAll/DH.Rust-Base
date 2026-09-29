#![cfg(feature = "net")]
//! N003 验收：WS 服务端 + HTTP 升级（dhrust 自测闭环：dhrust 服务端 ↔ dhrust 客户端）。
//!
//! 覆盖：升级握手（含 RFC 6455 已知向量逐字节校验）、ping json → pong json 脚本、
//! 掩码帧往返、普通 HTTP 共用端口、多连接隔离、服务端主动关闭/客户端关闭、
//! 空闲超时断开、非法升级请求拒绝（400/426）。

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dhrust::net::http::{
    handler, HttpOutcome, HttpRequest, HttpResponse, HttpServer, HttpServerOptions,
};
use dhrust::net::ws::{
    WsClient, WsClientOptions, WsEvent, WsHooks, WsServerConn, WsServerHooks, WsServerOptions,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

// ————— 测试服务端 —————

/// 服务端观察状态。
#[derive(Default)]
struct St {
    /// on_message 收到的应用消息（ping/pong 不应出现）
    messages: Mutex<Vec<String>>,
    /// on_open 次数
    opens: AtomicUsize,
    /// on_close 原因列表
    closes: Mutex<Vec<String>>,
    /// 最近一次连接句柄（用于主动踢出）
    last_conn: Mutex<Option<WsServerConn>>,
}

async fn start_server(st: Arc<St>, options: HttpServerOptions) -> SocketAddr {
    let server = HttpServer::bind("127.0.0.1:0").await.unwrap();
    let addr = server.local_addr().unwrap();

    let st_open = st.clone();
    let st_msg = st.clone();
    let st_close = st.clone();
    let hooks = WsServerHooks {
        on_open: Some(Arc::new(move |c: WsServerConn| {
            st_open.opens.fetch_add(1, Ordering::SeqCst);
            *st_open.last_conn.lock().unwrap() = Some(c);
        })),
        on_message: Some(Arc::new(move |m| {
            st_msg.messages.lock().unwrap().push(m.text.clone());
            // 回显（带前缀便于断言）
            m.conn.send_text(format!("echo:{}", m.text));
        })),
        on_binary: Some(Arc::new(move |m| {
            // 二进制原样回显（send_binary 走同一写通道）
            m.conn.send_binary(m.data.clone());
        })),
        on_close: Some(Arc::new(move |_c, reason| {
            st_close.closes.lock().unwrap().push(reason);
        })),
    };

    let svc = handler(move |req: HttpRequest| {
        let hooks = hooks.clone();
        async move {
            if req.path == "/ws" && req.is_websocket_upgrade() {
                HttpOutcome::WebSocket(hooks)
            } else if req.path == "/ws" {
                HttpOutcome::Response(HttpResponse::text(400, "need upgrade"))
            } else {
                HttpOutcome::Response(HttpResponse::text(200, format!("hello {}", req.path)))
            }
        }
    });

    tokio::spawn(async move {
        let _ = server.serve_with(svc, options).await;
    });
    addr
}

// ————— 工具 —————

fn fast_opts() -> WsClientOptions {
    WsClientOptions {
        connect_timeout: Duration::from_millis(800),
        ping_interval: Duration::from_millis(150),
        pong_timeout: Duration::from_millis(400),
        reconnect_base: Duration::from_millis(50),
        reconnect_max: Duration::from_millis(200),
        ..Default::default()
    }
}

fn ws_url(addr: SocketAddr) -> String {
    format!("ws://127.0.0.1:{}/ws", addr.port())
}

async fn wait_until(mut cond: impl FnMut() -> bool, timeout_ms: u64) -> bool {
    let start = std::time::Instant::now();
    loop {
        if cond() {
            return true;
        }
        if start.elapsed() >= Duration::from_millis(timeout_ms) {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// 手工连接 + 发升级请求 + 读响应头（返回 头文本 与 头之后的剩余字节）。
async fn raw_upgrade_handshake(
    addr: SocketAddr,
    path: &str,
    key: Option<&str>,
    version: u8,
) -> (String, Vec<u8>, TcpStream) {
    let mut tcp = TcpStream::connect(addr).await.unwrap();
    let mut req = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
    );
    if let Some(key) = key {
        req.push_str(&format!("Sec-WebSocket-Key: {key}\r\n"));
    }
    req.push_str(&format!("Sec-WebSocket-Version: {version}\r\n\r\n"));
    tcp.write_all(req.as_bytes()).await.unwrap();

    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..pos + 4]).into_owned();
            let rest = buf[pos + 4..].to_vec();
            return (head, rest, tcp);
        }
        let n = tcp.read(&mut chunk).await.unwrap();
        if n == 0 {
            return (String::from_utf8_lossy(&buf).into_owned(), Vec::new(), tcp);
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// 构造客户端掩码帧（RFC 6455：客户端必须掩码；仅测试用短负载）。
fn masked_frame(opcode: u8, payload: &[u8], mask: [u8; 4]) -> Vec<u8> {
    let mut out = vec![0x80 | opcode, 0x80 | payload.len() as u8];
    out.extend_from_slice(&mask);
    for (i, b) in payload.iter().enumerate() {
        out.push(b ^ mask[i % 4]);
    }
    out
}

/// 构造客户端掩码文本帧。
fn masked_text_frame(payload: &[u8], mask: [u8; 4]) -> Vec<u8> {
    masked_frame(0x1, payload, mask)
}

/// 构造客户端掩码二进制帧。
fn masked_binary_frame(payload: &[u8], mask: [u8; 4]) -> Vec<u8> {
    masked_frame(0x2, payload, mask)
}

/// 读一个服务端帧（服务端出帧不应掩码）。
async fn read_server_frame(tcp: &mut TcpStream, rest: &mut Vec<u8>) -> (u8, Vec<u8>) {
    // 先凑满 2 字节头
    while rest.len() < 2 {
        let mut chunk = [0u8; 512];
        let n = tcp.read(&mut chunk).await.unwrap();
        assert!(n > 0, "连接被提前关闭");
        rest.extend_from_slice(&chunk[..n]);
    }
    let b0 = rest[0];
    let b1 = rest[1];
    let opcode = b0 & 0x0f;
    assert_eq!(b1 & 0x80, 0, "服务端出帧不应设置掩码位");
    let len = (b1 & 0x7f) as usize;
    assert!(len < 126, "测试帧长度应小于 126");
    while rest.len() < 2 + len {
        let mut chunk = [0u8; 512];
        let n = tcp.read(&mut chunk).await.unwrap();
        assert!(n > 0, "连接被提前关闭");
        rest.extend_from_slice(&chunk[..n]);
    }
    let payload = rest[2..2 + len].to_vec();
    rest.drain(..2 + len);
    (opcode, payload)
}

// ————— 用例 —————

/// dhrust 客户端 ↔ dhrust 服务端：升级成功 + 回显往返。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ws_upgrade_echo() {
    let st = Arc::new(St::default());
    let addr = start_server(st.clone(), HttpServerOptions::default()).await;

    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(64);
    let client = WsClient::connect(
        ws_url(addr),
        WsHooks {
            on_message: Some(Arc::new(move |m| {
                let _ = tx.try_send(m.text);
            })),
            ..Default::default()
        },
        fast_opts(),
    );
    assert!(
        wait_until(|| client.is_connected(), 2000).await,
        "未连接到测试服务端"
    );

    assert!(client.send_text("hello"), "发送失败");
    let got = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("2 秒内未收到回显")
        .unwrap();
    assert_eq!(got, "echo:hello");
    assert_eq!(st.opens.load(Ordering::SeqCst), 1);
    client.close();
}

/// ping json 由服务端脚本自动回 pong json（不进入业务分发）；连接保持存活。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ping_pong_script_replies_and_not_dispatched() {
    let st = Arc::new(St::default());
    let addr = start_server(st.clone(), HttpServerOptions::default()).await;

    let client = WsClient::connect(ws_url(addr), WsHooks::default(), fast_opts());
    assert!(
        wait_until(|| client.is_connected(), 2000).await,
        "未连接到测试服务端"
    );
    let mut events = client.subscribe();

    // 150ms 一次心跳；700ms 窗口内应至少收到 2 次 Pong（服务端自动应答）
    let mut pongs = 0usize;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(700);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(200), events.recv()).await {
            Ok(Ok(WsEvent::PongReceived)) => pongs += 1,
            Ok(Ok(_)) => {}
            Ok(Err(_)) | Err(_) => {}
        }
    }
    assert!(pongs >= 2, "Pong 应答不足（pongs={pongs}）");
    assert!(client.is_connected(), "连接应保持存活");

    // ping/pong 不应进入业务分发
    let msgs = st.messages.lock().unwrap().clone();
    assert!(
        !msgs
            .iter()
            .any(|m| m.contains("\"Type\":\"ping\"") || m.contains("\"Type\":\"pong\"")),
        "ping/pong 泄漏到了业务分发: {msgs:?}"
    );
    client.close();
}

/// 手工逐字节：RFC 6455 已知向量握手（Accept 校验）+ 掩码帧往返（服务端解掩码/出帧不掩码）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn raw_handshake_and_frame_echo() {
    let st = Arc::new(St::default());
    let addr = start_server(st.clone(), HttpServerOptions::default()).await;

    let (head, mut rest, mut tcp) =
        raw_upgrade_handshake(addr, "/ws", Some("dGhlIHNhbXBsZSBub25jZQ=="), 13).await;
    assert!(head.starts_with("HTTP/1.1 101"), "应返回 101: {head}");
    assert!(
        head.to_lowercase()
            .contains("sec-websocket-accept: s3pplmbitxaq9kygzzhzrbk+xoo="),
        "Accept 应为 RFC 6455 示例值: {head}"
    );
    assert!(head.to_lowercase().contains("upgrade: websocket"));

    // 发送掩码文本帧 "hi" → 期待服务端回显帧 "echo:hi"（不掩码）
    let frame = masked_text_frame(b"hi", [0x01, 0x02, 0x03, 0x04]);
    tcp.write_all(&frame).await.unwrap();
    let (opcode, payload) = read_server_frame(&mut tcp, &mut rest).await;
    assert_eq!(opcode, 0x1, "应为文本帧");
    assert_eq!(String::from_utf8_lossy(&payload), "echo:hi");

    assert!(
        wait_until(
            || st.messages.lock().unwrap().iter().any(|m| m == "hi"),
            1000
        )
        .await,
        "服务端未收到业务消息"
    );
}

/// 非法升级请求：版本不符 → 426；缺 Key → 400。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bad_handshake_rejected() {
    let st = Arc::new(St::default());
    let addr = start_server(st.clone(), HttpServerOptions::default()).await;

    let (head, _rest, _tcp) =
        raw_upgrade_handshake(addr, "/ws", Some("dGhlIHNhbXBsZSBub25jZQ=="), 8).await;
    assert!(head.starts_with("HTTP/1.1 426"), "版本不符应回 426: {head}");
    assert!(
        head.to_lowercase().contains("sec-websocket-version: 13"),
        "426 应携带版本提示: {head}"
    );

    let (head, _rest, _tcp) = raw_upgrade_handshake(addr, "/ws", None, 13).await;
    assert!(head.starts_with("HTTP/1.1 400"), "缺 Key 应回 400: {head}");
}

/// 同一端口普通 HTTP 与 WS 共存（非 WS 请求走普通响应路径）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http_plain_request_on_same_port() {
    let st = Arc::new(St::default());
    let addr = start_server(st.clone(), HttpServerOptions::default()).await;

    let mut tcp = TcpStream::connect(addr).await.unwrap();
    tcp.write_all(b"GET /other HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut buf: Vec<u8> = Vec::new();
    tcp.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf).into_owned();
    assert!(text.starts_with("HTTP/1.1 200"), "应回 200: {text}");
    assert!(
        text.contains("hello /other"),
        "响应体应为 hello /other: {text}"
    );
}

/// 多连接隔离：两个客户端各自收自己的回显。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_clients_isolated() {
    let st = Arc::new(St::default());
    let addr = start_server(st.clone(), HttpServerOptions::default()).await;

    let (tx1, mut rx1) = tokio::sync::mpsc::channel::<String>(64);
    let c1 = WsClient::connect(
        ws_url(addr),
        WsHooks {
            on_message: Some(Arc::new(move |m| {
                let _ = tx1.try_send(m.text);
            })),
            ..Default::default()
        },
        fast_opts(),
    );
    let (tx2, mut rx2) = tokio::sync::mpsc::channel::<String>(64);
    let c2 = WsClient::connect(
        ws_url(addr),
        WsHooks {
            on_message: Some(Arc::new(move |m| {
                let _ = tx2.try_send(m.text);
            })),
            ..Default::default()
        },
        fast_opts(),
    );
    assert!(
        wait_until(|| c1.is_connected() && c2.is_connected(), 2000).await,
        "两个客户端应都连上"
    );

    assert!(c1.send_text("a1"));
    assert!(c2.send_text("b1"));
    let g1 = tokio::time::timeout(Duration::from_secs(2), rx1.recv())
        .await
        .unwrap()
        .unwrap();
    let g2 = tokio::time::timeout(Duration::from_secs(2), rx2.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(g1, "echo:a1");
    assert_eq!(g2, "echo:b1");
    c1.close();
    c2.close();
}

/// 服务端主动踢出（理由=服务端关闭连接）与客户端主动关闭（理由=客户端关闭连接）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn server_kick_and_client_close() {
    let st = Arc::new(St::default());
    let addr = start_server(st.clone(), HttpServerOptions::default()).await;

    let client = WsClient::connect(ws_url(addr), WsHooks::default(), fast_opts());
    assert!(
        wait_until(|| client.is_connected(), 2000).await,
        "未连接到测试服务端"
    );

    // 服务端主动关闭
    let conn = st.last_conn.lock().unwrap().clone().expect("应已建立连接");
    conn.close();
    assert!(
        wait_until(
            || st
                .closes
                .lock()
                .unwrap()
                .iter()
                .any(|r| r == "服务端关闭连接"),
            2000
        )
        .await,
        "服务端主动关闭未记录: {:?}",
        st.closes.lock().unwrap()
    );

    // 客户端关闭（等重连稳定后再关）
    assert!(
        wait_until(|| client.is_connected(), 2000).await,
        "客户端应自动重连"
    );
    client.close();
    assert!(
        wait_until(
            || st
                .closes
                .lock()
                .unwrap()
                .iter()
                .any(|r| r == "客户端关闭连接"),
            2000
        )
        .await,
        "客户端主动关闭未记录: {:?}",
        st.closes.lock().unwrap()
    );
}

/// 空闲超时：客户端不发任何数据（心跳拉长），服务端按 idle_timeout 断开。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_timeout_disconnects() {
    let st = Arc::new(St::default());
    let options = HttpServerOptions {
        ws: WsServerOptions {
            idle_timeout: Some(Duration::from_millis(300)),
            ..Default::default()
        },
        ..Default::default()
    };
    let addr = start_server(st.clone(), options).await;

    let client = WsClient::connect(
        ws_url(addr),
        WsHooks::default(),
        WsClientOptions {
            ping_interval: Duration::from_secs(30), // 不触发心跳，保持空闲
            pong_timeout: Duration::from_secs(60),
            ..fast_opts()
        },
    );
    assert!(
        wait_until(|| client.is_connected(), 2000).await,
        "未连接到测试服务端"
    );

    assert!(
        wait_until(
            || st
                .closes
                .lock()
                .unwrap()
                .iter()
                .any(|r| r.starts_with("空闲超时")),
            2000
        )
        .await,
        "空闲超时未断开: {:?}",
        st.closes.lock().unwrap()
    );
    client.close();
}

/// 等待服务端记录最近连接句柄（on_open 先于测试逻辑完成）。
async fn wait_for_conn(st: &St, timeout_ms: u64) -> WsServerConn {
    let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if let Some(conn) = st.last_conn.lock().unwrap().clone() {
            return conn;
        }
        assert!(std::time::Instant::now() < deadline, "等待连接句柄超时");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// 二进制帧：服务端 on_binary 收到并按原样回显（send_binary 走同一写通道）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn binary_frame_echo() {
    let st = Arc::new(St::default());
    let addr = start_server(st.clone(), HttpServerOptions::default()).await;

    let (head, mut rest, mut tcp) =
        raw_upgrade_handshake(addr, "/ws", Some("dGhlIHNhbXBsZSBub25jZQ=="), 13).await;
    assert!(head.starts_with("HTTP/1.1 101"), "应返回 101: {head}");

    // 发掩码二进制帧 → 服务端回显（不掩码）
    let frame = masked_binary_frame(b"bin-hello", [0x11, 0x22, 0x33, 0x44]);
    tcp.write_all(&frame).await.unwrap();
    let (opcode, payload) = read_server_frame(&mut tcp, &mut rest).await;
    assert_eq!(opcode, 0x2, "应为二进制帧");
    assert_eq!(payload, b"bin-hello");
}

/// 服务端主动 Ping（server_ping）：按间隔发帧层 Ping；配合 idle_timeout 由 Pong 保活。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn server_ping_keeps_alive() {
    let st = Arc::new(St::default());
    let options = HttpServerOptions {
        ws: WsServerOptions {
            server_ping: Some(Duration::from_millis(100)),
            idle_timeout: Some(Duration::from_millis(250)),
            ..Default::default()
        },
        ..Default::default()
    };
    let addr = start_server(st.clone(), options).await;

    let (head, mut rest, mut tcp) =
        raw_upgrade_handshake(addr, "/ws", Some("dGhlIHNhbXBsZSBub25jZQ=="), 13).await;
    assert!(head.starts_with("HTTP/1.1 101"), "应返回 101: {head}");

    // 连续响应 6 轮 Ping（约 600ms，超过 idle_timeout）：每轮回 Pong 刷新服务端活动时间
    for _ in 0..6 {
        let (opcode, payload) = read_server_frame(&mut tcp, &mut rest).await;
        assert_eq!(opcode, 0x9, "应持续收到服务端 Ping 帧");
        let pong = masked_frame(0xA, &payload, [0x01, 0x02, 0x03, 0x04]);
        tcp.write_all(&pong).await.unwrap();
    }
    assert!(
        st.closes.lock().unwrap().is_empty(),
        "有 Pong 响应不应被空闲超时断开: {:?}",
        st.closes.lock().unwrap()
    );
}

/// 慢消费者溢出保护（close_on_send_overflow）：队列满后客户端恢复读取，
/// 会话应写出 Close 并断开（服务端记录溢出原因）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slow_consumer_disconnected_on_overflow() {
    let st = Arc::new(St::default());
    let options = HttpServerOptions {
        ws: WsServerOptions {
            send_queue: 2,
            close_on_send_overflow: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let addr = start_server(st.clone(), options).await;

    // raw 客户端连接后先不读——制造 TCP 背压
    let (head, rest, mut tcp) =
        raw_upgrade_handshake(addr, "/ws", Some("dGhlIHNhbXBsZSBub25jZQ=="), 13).await;
    assert!(head.starts_with("HTTP/1.1 101"), "应返回 101: {head}");
    let conn = wait_for_conn(&st, 2000).await;

    // 灌入大消息（每条 128KB），直到发送返回 false（队列溢出）
    let big = "x".repeat(128 * 1024);
    let mut overflowed = false;
    for _ in 0..16 {
        if !conn.send_text(big.clone()) {
            overflowed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(overflowed, "持续发送应触发队列溢出（返回 false）");

    // 客户端恢复读取（让会话循环写完当前帧并处理溢出通知）：最终应读到 EOF（连接被断开）
    let mut drained = rest;
    let mut chunk = vec![0u8; 64 * 1024];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!left.is_zero(), "等待服务端断开超时");
        match tokio::time::timeout(left, tcp.read(&mut chunk)).await {
            Ok(Ok(0)) => break, // EOF：服务端已关闭
            Ok(Ok(n)) => drained.extend_from_slice(&chunk[..n]),
            Ok(Err(_)) => break, // RST：等同断开
            Err(_) => panic!("等待服务端断开超时"),
        }
    }
    assert!(!drained.is_empty(), "断开前应已收到部分积压数据");
    assert!(
        wait_until(
            || st
                .closes
                .lock()
                .unwrap()
                .iter()
                .any(|r| r.contains("发送队列溢出")),
            2000
        )
        .await,
        "溢出断开未记录: {:?}",
        st.closes.lock().unwrap()
    );
}
