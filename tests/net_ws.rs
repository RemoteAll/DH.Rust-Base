#![cfg(feature = "net")]
//! net::ws 客户端集成测试（用例先行：对齐 C# 排障清单）。
//!
//! 自建测试服务端（原始 TCP + 手写 101 升级响应 + fastwebsockets 服务端角色），覆盖：
//! 连接/回显、心跳保活、Pong 超时重连、服务端断线重连、**慢任务不阻塞心跳**（核心回归）、
//! 延迟响应（后台补发）、并发发送串行化。

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dhrust::net::ws::{WsClient, WsClientOptions, WsEvent, WsHooks, WsMessage};
use fastwebsockets::{Frame, OpCode, Role, WebSocket};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

// ————— 测试服务端 —————

struct ServerState {
    log: Mutex<Vec<String>>,
    connections: AtomicUsize,
    /// 收到心跳 Ping 时不回 Pong（触发客户端 Pong 超时）
    silence_pong: AtomicBool,
    /// 连接建立后 N 毫秒主动关闭（触发客户端重连）
    close_after_ms: Mutex<Option<u64>>,
}

struct TestServer {
    addr: std::net::SocketAddr,
    state: Arc<ServerState>,
}

impl TestServer {
    fn text_count(&self, needle: &str) -> usize {
        self.state
            .log
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m.contains(needle))
            .count()
    }

    fn connections(&self) -> usize {
        self.state.connections.load(Ordering::SeqCst)
    }
}

async fn start_server() -> TestServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = Arc::new(ServerState {
        log: Mutex::new(Vec::new()),
        connections: AtomicUsize::new(0),
        silence_pong: AtomicBool::new(false),
        close_after_ms: Mutex::new(None),
    });
    let st = state.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            st.connections.fetch_add(1, Ordering::SeqCst);
            let st = st.clone();
            tokio::spawn(async move { serve_conn(stream, st).await });
        }
    });
    TestServer { addr, state }
}

/// 单连接：握手（手写 101）→ 帧循环（回显 / 心跳应答 / 可控静默与定时关闭）。
async fn serve_conn(mut stream: tokio::net::TcpStream, state: Arc<ServerState>) {
    // 读升级请求（直到 \r\n\r\n）
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    let head_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        let Ok(n) = stream.read(&mut chunk).await else {
            return;
        };
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let key = head
        .lines()
        .find_map(|l| {
            let (name, value) = l.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("sec-websocket-key")
                .then(|| value.trim().to_string())
        })
        .unwrap_or_default();
    let accept = dhrust::net::ws::ws_accept(&key);
    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    if stream.write_all(response.as_bytes()).await.is_err() {
        return;
    }
    let _ = stream.flush().await;

    // 帧循环（服务端：读→写 顺序执行，无并发）
    let mut ws = WebSocket::after_handshake(stream, Role::Server);
    ws.set_auto_close(false);
    ws.set_auto_pong(false);
    let start = std::time::Instant::now();

    loop {
        // 定时关闭（每轮读帧后检查；客户端心跳保证帧持续到达）
        let close_after = *state.close_after_ms.lock().unwrap();
        if let Some(ms) = close_after {
            if start.elapsed().as_millis() as u64 >= ms {
                let _ = ws.write_frame(Frame::close(1000, b"bye")).await;
                break;
            }
        }
        let frame = match ws.read_frame().await {
            Ok(f) => f,
            Err(_) => break,
        };
        match frame.opcode {
            OpCode::Text => {
                let text = String::from_utf8_lossy(&frame.payload).into_owned();
                state.log.lock().unwrap().push(text.clone());
                if text.contains("\"Type\":\"ping\"") {
                    if !state.silence_pong.load(Ordering::SeqCst) {
                        let pong = "{\"Type\":\"pong\",\"Timestamp\":1,\"ServerTime\":\"stub\"}";
                        if ws
                            .write_frame(Frame::text(pong.as_bytes().into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                } else if text == "echo:burst" {
                    for i in 0..5 {
                        let m = format!("burst-{i}");
                        if ws
                            .write_frame(Frame::text(m.into_bytes().into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                } else if text.starts_with("echo:") || text.starts_with("delayed") {
                    // 回显（覆盖并发发送/延迟响应用例：服务端记录后原样回发）
                    if ws
                        .write_frame(Frame::text(text.into_bytes().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
            OpCode::Ping => {
                let _ = ws.write_frame(Frame::pong(frame.payload)).await;
            }
            OpCode::Close => {
                let _ = ws.write_frame(Frame::close(1000, b"")).await;
                break;
            }
            _ => {}
        }
    }
}

// ————— 测试工具 —————

/// 快速时序（测试用）：心跳 150ms / Pong 超时 300ms / 重连 50ms。
fn fast_opts() -> WsClientOptions {
    WsClientOptions {
        connect_timeout: Duration::from_millis(800),
        ping_interval: Duration::from_millis(150),
        pong_timeout: Duration::from_millis(300),
        reconnect_base: Duration::from_millis(50),
        reconnect_max: Duration::from_millis(200),
        ..Default::default()
    }
}

async fn wait_until(mut f: impl FnMut() -> bool, ms: u64) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(ms);
    while tokio::time::Instant::now() < deadline {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

fn ws_url(server: &TestServer) -> String {
    format!("ws://{}/ws", server.addr)
}

// ————— 用例 —————

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connect_echo_and_handler() {
    let server = start_server().await;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let hooks = WsHooks {
        on_message: Some(Arc::new(move |m: WsMessage| {
            let _ = tx.send(m.text);
        })),
        ..Default::default()
    };
    let client = WsClient::connect(ws_url(&server), hooks, fast_opts());
    assert!(
        wait_until(|| client.is_connected(), 2000).await,
        "未在超时内连接"
    );

    assert!(client.send_text("echo:hello"));
    let got = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("等待回显超时")
        .expect("消息通道关闭");
    assert_eq!(got, "echo:hello");
    assert_eq!(server.text_count("echo:hello"), 1);
    assert_eq!(server.connections(), 1);
    client.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn events_report_connect() {
    let server = start_server().await;
    let client = WsClient::connect(ws_url(&server), WsHooks::default(), fast_opts());
    let mut events = client.subscribe();
    let ev = tokio::time::timeout(Duration::from_secs(2), events.recv())
        .await
        .expect("等待事件超时")
        .expect("事件通道关闭");
    assert!(matches!(ev, WsEvent::Connected));
    client.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ping_pong_keeps_alive() {
    let server = start_server().await;
    let client = WsClient::connect(ws_url(&server), WsHooks::default(), fast_opts());
    assert!(wait_until(|| client.is_connected(), 2000).await);

    tokio::time::sleep(Duration::from_millis(650)).await;
    assert!(client.is_connected(), "心跳保活失败：连接已断开");
    assert!(
        server.text_count("\"Type\":\"ping\"") >= 2,
        "心跳 Ping 数量不足（{}）",
        server.text_count("\"Type\":\"ping\"")
    );
    assert_eq!(server.connections(), 1, "正常心跳不应触发重连");
    client.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pong_timeout_triggers_reconnect() {
    let server = start_server().await;
    server.state.silence_pong.store(true, Ordering::SeqCst);
    let client = WsClient::connect(ws_url(&server), WsHooks::default(), fast_opts());
    assert!(wait_until(|| client.is_connected(), 2000).await);

    // 静默 Pong：客户端应在超时（300ms）后主动断开并重连
    assert!(
        wait_until(|| server.connections() >= 2, 3000).await,
        "Pong 超时后未触发重连（connections={}）",
        server.connections()
    );
    client.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn server_close_triggers_reconnect() {
    let server = start_server().await;
    *server.state.close_after_ms.lock().unwrap() = Some(120);
    let client = WsClient::connect(ws_url(&server), WsHooks::default(), fast_opts());
    assert!(wait_until(|| client.is_connected(), 2000).await);

    // 服务端主动关闭：客户端应自动重连（无限重连策略）
    assert!(
        wait_until(|| server.connections() >= 2, 3000).await,
        "服务端关闭后未触发重连（connections={}）",
        server.connections()
    );
    client.close();
}

/// 核心回归（C# 排障教训）：慢任务不得阻塞心跳——读循环/会话循环永不等待业务处理。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slow_handler_does_not_block_heartbeat() {
    let server = start_server().await;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let hooks = WsHooks {
        on_message: Some(Arc::new(move |m: WsMessage| {
            if m.text.contains("slow") {
                // 模拟耗时业务（下载/打包），阻塞当前工作线程 700ms
                std::thread::sleep(Duration::from_millis(700));
            }
            let _ = tx.send(m.text);
        })),
        ..Default::default()
    };
    let client = WsClient::connect(ws_url(&server), hooks, fast_opts());
    assert!(wait_until(|| client.is_connected(), 2000).await);

    assert!(client.send_text("echo:slow"));
    // 慢任务执行期间：心跳（150ms 间隔）必须继续送达，连接保持、无重连
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(client.is_connected(), "慢任务阻塞了会话，连接被断开");
    let pings = server.text_count("\"Type\":\"ping\"");
    assert!(pings >= 2, "慢任务期间心跳未继续（pings={pings}）");
    assert_eq!(server.connections(), 1, "慢任务不应触发重连");

    // 慢消息最终仍送达 handler
    let got = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("慢消息未送达")
        .expect("通道关闭");
    assert_eq!(got, "echo:slow");
    client.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delayed_response_via_handle() {
    let server = start_server().await;
    let hooks = WsHooks {
        on_message: Some(Arc::new(move |m: WsMessage| {
            if m.text == "echo:need-delayed" {
                // 延迟响应：后台任务完成后经客户端句柄补发（对齐 C# Dispatcher 返回 null 语义）
                let client = m.client.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    client.send_text("delayed-ok");
                });
            }
        })),
        ..Default::default()
    };
    let client = WsClient::connect(ws_url(&server), hooks, fast_opts());
    assert!(wait_until(|| client.is_connected(), 2000).await);

    assert!(client.send_text("echo:need-delayed"));
    assert!(
        wait_until(|| server.text_count("delayed-ok") >= 1, 3000).await,
        "延迟响应未送达服务端"
    );
    client.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_sends_are_serialized() {
    let server = start_server().await;
    let client = WsClient::connect(ws_url(&server), WsHooks::default(), fast_opts());
    assert!(wait_until(|| client.is_connected(), 2000).await);

    let mut handles = Vec::new();
    for i in 0..16 {
        let c = client.clone();
        handles.push(tokio::spawn(async move {
            c.send_text_wait(format!("echo:m{i:02}")).await
        }));
    }
    for h in handles {
        assert!(h.await.unwrap().is_ok(), "并发发送存在失败项");
    }
    assert!(
        wait_until(
            || (0..16).all(|i| server.text_count(&format!("echo:m{i:02}")) >= 1),
            3000
        )
        .await,
        "并发发送消息未全部送达服务端"
    );
    client.close();
}
