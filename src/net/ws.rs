//! net::ws —— WebSocket 会话层（fastwebsockets 帧层 + 自研会话语义）。
//!
//! 客户端（N002）语义对齐 C# `MyWebSocketClient`（DHDeploy.Agent）：
//! - 连接超时 10s；无限重连 + 指数退避（1s 起、×2、上限 10 分钟；连续失败 20 次转长期模式 5 分钟）；
//! - 心跳：客户端每 45s 发文本 Ping `{"Type":"ping","Timestamp":<Ticks>}`，服务端回
//!   `{"Type":"pong",...}`（服务端 `WebSocketHelper.HandlePingPongMessageAsync`）；
//!   Pong 超时 90s → 主动断开重连（服务端接收超时同为 90s，客户端必须按时 Ping）；
//! - 发送：单一 `tokio::sync::mpsc` 写通道（**单写者**）——业务消息、心跳、Pong 帧、
//!   延迟响应共用，天然串行（对齐 C# `_sendLock`；WebSocket 不允许并发发送）；
//! - 接收：读任务只做「控制帧处理 + 消息分类」；应用消息经 `tokio::spawn` 分发到
//!   handler（**读循环永不阻塞**——C# 排障教训的根治：慢任务曾阻塞 Pong 处理导致断连）；
//! - 延迟响应：handler 拿到 [`WsMessage`]（内含 [`WsClient`] 句柄），可在后台任务中
//!   随时 `send_text` 补发响应（对齐 C#「Dispatcher 返回 null 时延迟发送」机制）。
//!
//! 握手：自管（HTTP/1.1 升级 + `Sec-WebSocket-Accept` 校验）；帧层自动行为全部关闭，
//! 控制帧由会话层处理。wss/TLS 待 `net-tls`（rustls + ring）。
//! 服务端（N003）：hyper 升级后交 [`run_server_session`]——ping json 自动回
//! pong json（对齐 C# `WebSocketHelper.HandlePingPongMessageAsync`）、消息在
//! blocking 池分发、发送经单一写通道串行化。

use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fastwebsockets::{FragmentCollectorRead, Frame, OpCode, Payload, Role, WebSocket};
use rand::Rng as _;
use sha1::{Digest, Sha1};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc, oneshot, Notify};

pub use fastwebsockets::WebSocketError;

// ————— 错误 —————

/// WS 会话错误（连接/握手/协议/关闭）。
#[derive(Debug)]
pub enum WsError {
    /// IO 错误（TCP/读写）
    Io(std::io::Error),
    /// 握手失败（状态非 101、Accept 校验失败、URL 非法等）
    Handshake(String),
    /// 帧层协议错误
    Protocol(String),
    /// 客户端已关闭或连接不可用
    Closed,
}

impl std::fmt::Display for WsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WsError::Io(e) => write!(f, "IO 错误: {e}"),
            WsError::Handshake(m) => write!(f, "握手失败: {m}"),
            WsError::Protocol(m) => write!(f, "协议错误: {m}"),
            WsError::Closed => write!(f, "连接不可用"),
        }
    }
}

impl std::error::Error for WsError {}

impl From<std::io::Error> for WsError {
    fn from(e: std::io::Error) -> Self {
        WsError::Io(e)
    }
}

// ————— 配置 —————

/// 客户端配置（默认值对齐 C# `MyWebSocketClient` 常量与服务端 `WebSocketConstants`）。
#[derive(Clone, Debug)]
pub struct WsClientOptions {
    /// 连接（TCP + 升级握手）超时；C# 教训：30s → 10s，避免卡在 isConnecting 状态
    pub connect_timeout: Duration,
    /// 心跳间隔（服务端 `ClientPingInterval` = 45s）
    pub ping_interval: Duration,
    /// Pong 超时（服务端 `ReceiveTimeout` = 90s）：距最近一次收到 Pong 的时长
    /// 超过该值即判定连接失效并触发重连（看门狗按 1/3 周期巡检）
    pub pong_timeout: Duration,
    /// 帧层单条消息上限（聚合后；超出即断开）
    pub max_message_size: usize,
    /// 重连基础间隔（指数退避起点）
    pub reconnect_base: Duration,
    /// 重连间隔上限
    pub reconnect_max: Duration,
    /// 连续失败达到该值后转长期重连模式
    pub long_term_after: u32,
    /// 长期重连模式固定间隔
    pub long_term_interval: Duration,
}

impl Default for WsClientOptions {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            ping_interval: Duration::from_secs(45),
            pong_timeout: Duration::from_secs(90),
            max_message_size: 16 * 1024 * 1024,
            reconnect_base: Duration::from_secs(1),
            reconnect_max: Duration::from_secs(600),
            long_term_after: 20,
            long_term_interval: Duration::from_secs(300),
        }
    }
}

// ————— 事件与钩子 —————

/// 会话事件（可多订阅，见 [`WsClient::subscribe`]）。
#[derive(Debug, Clone)]
pub enum WsEvent {
    /// 连接建立（每次成功连接各发一次）
    Connected,
    /// 连接断开（含原因；随后自动重连）
    Disconnected {
        /// 断开原因（人类可读）
        reason: String,
    },
    /// 已发送心跳 Ping
    PingSent,
    /// 已收到 Pong（应用层文本或帧层）
    PongReceived,
}

/// 应用消息（文本帧；Pong 已在会话层消化，不会投递给 handler）。
#[derive(Clone)]
pub struct WsMessage {
    /// 消息文本
    pub text: String,
    /// 客户端句柄（可在后台任务中延迟发送响应）
    pub client: WsClient,
}

impl std::fmt::Debug for WsMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsMessage")
            .field("text", &self.text)
            .finish()
    }
}

/// 会话钩子（可选注册；回调均在 tokio 任务中调用，实现方不得阻塞）。
#[derive(Clone, Default)]
pub struct WsHooks {
    /// 应用消息处理器（每条消息 `tokio::spawn` 分发——读循环永不阻塞）
    pub on_message: Option<Arc<dyn Fn(WsMessage) + Send + Sync>>,
    /// 连接成功回调（发送注册消息等；对齐 C# 连接后 `SendRegistrationMessageAsync`）
    pub on_connected: Option<Arc<dyn Fn(WsClient) + Send + Sync>>,
    /// 连接断开回调（重连前）
    pub on_disconnected: Option<Arc<dyn Fn(String) + Send + Sync>>,
    /// 心跳节拍回调（每次发 Ping 后；C# 用于检测 relay_update 配置变化）
    pub on_ping_tick: Option<Arc<dyn Fn(WsClient) + Send + Sync>>,
}

// ————— 客户端句柄 —————

/// WebSocket 客户端（可克隆句柄；会话由后台任务托管，断线无限自动重连）。
#[derive(Clone)]
pub struct WsClient {
    url: Arc<str>,
    cmd_tx: mpsc::Sender<Cmd>,
    events_tx: broadcast::Sender<WsEvent>,
    shared: Arc<Shared>,
}

struct Shared {
    options: WsClientOptions,
    hooks: WsHooks,
    connected: AtomicBool,
    /// 最后收到 Pong 的时刻（会话层更新）
    last_pong: Mutex<Instant>,
    /// 关闭信号（close() 置位并唤醒）
    shutdown: AtomicBool,
    shutdown_notify: Notify,
}

enum Cmd {
    Text {
        text: String,
        ack: Option<oneshot::Sender<bool>>,
    },
}

/// 读任务 → 会话任务的事件。
enum FrameEvent {
    /// 文本消息（已聚合分片）
    Text(String),
    /// 帧层 Ping（需回 Pong）
    Ping(Vec<u8>),
    /// 帧层 Pong（存活信号）
    Pong,
    /// 对端关闭帧
    Closed,
    /// 读循环终止（含原因）
    Ended(String),
}

// ————— 处理器执行器（专用线程池：阻塞隔离 + 低入队成本）—————

/// 消息处理器执行器：专用 std 线程池。
///
/// 语义（对齐 C# `Task.Run`）：handler 在工作线程执行——既不阻塞异步运行时
/// 工作线程（慢/阻塞 handler 不影响心跳与读循环），也不走 tokio blocking 池
/// （后者每消息一次全局队列调度，高吞吐场景实测差 ~15%——基准见 tools/bench-net）。
struct HandlerExecutor {
    tx: std::sync::mpsc::Sender<HandlerJob>,
}

type HandlerJob = Box<dyn FnOnce() + Send + 'static>;

impl HandlerExecutor {
    /// 进程级全局执行器（惰性初始化；线程数 = CPU 核数，界于 4~64）。
    fn global() -> &'static HandlerExecutor {
        static EXECUTOR: std::sync::OnceLock<HandlerExecutor> = std::sync::OnceLock::new();
        EXECUTOR.get_or_init(|| {
            let (tx, rx) = std::sync::mpsc::channel::<HandlerJob>();
            let rx = Arc::new(Mutex::new(rx));
            let threads = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(8)
                .clamp(4, 64);
            for i in 0..threads {
                let rx = rx.clone();
                let _ = std::thread::Builder::new()
                    .name(format!("dhrust-handler-{i}"))
                    .spawn(move || loop {
                        // 持锁 recv：同一时刻仅一个线程在队首等待；取到任务后立即释放锁再执行
                        let job = {
                            let guard = rx.lock().unwrap_or_else(|e| e.into_inner());
                            guard.recv()
                        };
                        match job {
                            Ok(job) => {
                                // 隔绝 panic：单个 handler 崩溃不得杀死池线程
                                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
                            }
                            Err(_) => break, // 发送端销毁（进程退出）——线程退出
                        }
                    });
            }
            HandlerExecutor { tx }
        })
    }

    /// 提交任务（fire-and-forget；std mpsc 入队，无异步调度开销）。
    fn spawn(&self, job: HandlerJob) {
        let _ = self.tx.send(job);
    }
}

impl WsClient {
    /// 建立客户端并后台托管会话（立即返回；连接与重连由后台任务驱动）。
    pub fn connect(url: impl Into<String>, hooks: WsHooks, options: WsClientOptions) -> WsClient {
        let (cmd_tx, cmd_rx) = mpsc::channel(1024);
        let (events_tx, _) = broadcast::channel(64);
        let shared = Arc::new(Shared {
            options,
            hooks,
            connected: AtomicBool::new(false),
            last_pong: Mutex::new(Instant::now()),
            shutdown: AtomicBool::new(false),
            shutdown_notify: Notify::new(),
        });
        let client = WsClient {
            url: Arc::from(url.into()),
            cmd_tx,
            events_tx,
            shared,
        };
        tokio::spawn(supervise(client.clone(), cmd_rx));
        client
    }

    /// 当前是否已连接（Open）。
    pub fn is_connected(&self) -> bool {
        self.shared.connected.load(Ordering::Acquire)
    }

    /// 订阅会话事件。
    pub fn subscribe(&self) -> broadcast::Receiver<WsEvent> {
        self.events_tx.subscribe()
    }

    /// 发送文本（fire-and-forget；未连接或队列满返回 `false`——对齐 C#「未打开即放弃」）。
    pub fn send_text(&self, text: impl Into<String>) -> bool {
        if !self.is_connected() {
            return false;
        }
        self.cmd_tx
            .try_send(Cmd::Text {
                text: text.into(),
                ack: None,
            })
            .is_ok()
    }

    /// 发送文本并等待写入结果（真实送达判定；对齐 C# `SendTrafficReportAsync` 语义）。
    pub async fn send_text_wait(&self, text: impl Into<String>) -> Result<(), WsError> {
        if !self.is_connected() {
            return Err(WsError::Closed);
        }
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(Cmd::Text {
                text: text.into(),
                ack: Some(tx),
            })
            .await
            .is_err()
        {
            return Err(WsError::Closed);
        }
        match rx.await {
            Ok(true) => Ok(()),
            _ => Err(WsError::Closed),
        }
    }

    /// 关闭客户端（不再重连；对齐 C# `Dispose` 语义）。
    pub fn close(&self) {
        self.shared.shutdown.store(true, Ordering::Release);
        self.shared.shutdown_notify.notify_waiters();
        self.shared.shutdown_notify.notify_one();
    }
}

// ————— 会话监管（连接 → 会话 → 重连）—————

/// 后台监管：连接 → 会话 → 断线退避 → 重连（无限循环，直至 close）。
async fn supervise(client: WsClient, mut cmd_rx: mpsc::Receiver<Cmd>) {
    let mut failures: u32 = 0;
    // 阻塞分发的运行时句柄：handler 在 blocking 池执行，不占用异步工作线程
    let rt = tokio::runtime::Handle::current();
    loop {
        if client.shared.shutdown.load(Ordering::Acquire) {
            break;
        }

        // 连接（TCP + 升级握手，带超时；握手自管以对齐 DH.NCore 语义）
        let result = tokio::time::timeout(
            client.shared.options.connect_timeout,
            connect_handshake(&client.url),
        )
        .await;

        match result {
            Ok(Ok(stream)) => {
                failures = 0;
                client.shared.connected.store(true, Ordering::Release);
                let _ = client.events_tx.send(WsEvent::Connected);
                if let Some(cb) = &client.shared.hooks.on_connected {
                    cb(client.clone());
                }

                let reason = run_session(&client, &rt, stream, &mut cmd_rx).await;

                client.shared.connected.store(false, Ordering::Release);
                let _ = client.events_tx.send(WsEvent::Disconnected {
                    reason: reason.clone(),
                });
                if let Some(cb) = &client.shared.hooks.on_disconnected {
                    cb(reason);
                }
                // 断线后丢弃排队中的消息（对齐 C#：未连接时发送直接放弃）
                drain_pending(&mut cmd_rx);
            }
            Ok(Err(_e)) => {
                failures += 1;
            }
            Err(_timeout) => {
                failures += 1;
            }
        }

        if client.shared.shutdown.load(Ordering::Acquire) {
            break;
        }

        // 退避等待（可被 close() 立刻打断）
        let delay = reconnect_delay(failures, &client.shared.options);
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = client.shared.shutdown_notify.notified() => break,
        }
    }
}

/// 重连延迟：失败 0 次（刚断开）用基础间隔；之后指数退避；超阈值转长期模式。
fn reconnect_delay(failures: u32, opts: &WsClientOptions) -> Duration {
    if failures >= opts.long_term_after {
        return opts.long_term_interval;
    }
    let exp = failures.saturating_sub(1).min(10);
    let secs = opts.reconnect_base.as_secs_f64() * 2f64.powi(exp as i32);
    Duration::from_secs_f64(secs.min(opts.reconnect_max.as_secs_f64()))
}

/// 丢弃排队命令（断线后调用；带 ack 的回复失败）。
fn drain_pending(cmd_rx: &mut mpsc::Receiver<Cmd>) {
    while let Ok(cmd) = cmd_rx.try_recv() {
        if let Cmd::Text { ack: Some(ack), .. } = cmd {
            let _ = ack.send(false);
        }
    }
}

// ————— 单次会话 —————

/// 运行一段已建立的会话，返回断开原因（正常关闭/超时/IO 错误等）。
async fn run_session(
    client: &WsClient,
    rt: &tokio::runtime::Handle,
    stream: PrefixedStream<TcpStream>,
    cmd_rx: &mut mpsc::Receiver<Cmd>,
) -> String {
    let opts = client.shared.options.clone();

    // 帧层接管（握手已完成；自动行为全关——控制帧由会话层处理）
    let mut ws = WebSocket::after_handshake(stream, Role::Client);
    ws.set_auto_close(false);
    ws.set_auto_pong(false);
    ws.set_max_message_size(opts.max_message_size);
    let (rx, mut tx) = ws.split(tokio::io::split);
    let mut reader = FragmentCollectorRead::new(rx);

    // 读任务：只分类与转发（永不阻塞；应用消息交给会话任务 spawn 分发）
    let (frame_tx, mut frame_rx) = mpsc::channel::<FrameEvent>(256);
    let read_task = tokio::spawn(async move {
        loop {
            match reader.read_frame(&mut noop_send).await {
                Ok(frame) => match frame.opcode {
                    OpCode::Text => {
                        let text = String::from_utf8_lossy(&frame.payload).into_owned();
                        if frame_tx.send(FrameEvent::Text(text)).await.is_err() {
                            break;
                        }
                    }
                    OpCode::Ping => {
                        let payload = frame.payload.to_vec();
                        if frame_tx.send(FrameEvent::Ping(payload)).await.is_err() {
                            break;
                        }
                    }
                    OpCode::Pong => {
                        if frame_tx.send(FrameEvent::Pong).await.is_err() {
                            break;
                        }
                    }
                    OpCode::Close => {
                        let _ = frame_tx.send(FrameEvent::Closed).await;
                        break;
                    }
                    _ => {}
                },
                Err(e) => {
                    let _ = frame_tx.send(FrameEvent::Ended(e.to_string())).await;
                    break;
                }
            }
        }
    });

    // 心跳与看门狗
    let mut ping_timer = tokio::time::interval_at(
        tokio::time::Instant::now() + opts.ping_interval,
        opts.ping_interval,
    );
    ping_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let watchdog_every = std::cmp::max(opts.pong_timeout / 3, Duration::from_millis(100));
    let mut watchdog = tokio::time::interval(watchdog_every);
    watchdog.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_ping_sent: Option<Instant> = None;

    // 新连接：重置 Pong 时刻，避免旧值误判（对齐 C# 每次连接后的全新状态）
    *client.shared.last_pong.lock().unwrap() = Instant::now();

    let reason = 'session: loop {
        if client.shared.shutdown.load(Ordering::Acquire) {
            let _ = tx
                .write_frame(Frame::close(1000, "关闭连接".as_bytes()))
                .await;
            break 'session "客户端关闭".to_string();
        }

        let step: Option<String> = tokio::select! {
            _ = client.shared.shutdown_notify.notified() => {
                let _ = tx.write_frame(Frame::close(1000, "关闭连接".as_bytes())).await;
                Some("客户端关闭".to_string())
            }
            cmd = cmd_rx.recv() => match cmd {
                Some(Cmd::Text { text, ack }) => {
                    let res = tx
                        .write_frame(Frame::text(Payload::Owned(text.into_bytes())))
                        .await;
                    if let Some(ack) = ack {
                        let _ = ack.send(res.is_ok());
                    }
                    match res {
                        Ok(()) => None,
                        Err(e) => Some(format!("发送失败: {e}")),
                    }
                }
                None => Some("命令通道关闭".to_string()),
            },
            ev = frame_rx.recv() => match ev {
                Some(FrameEvent::Text(text)) => {
                    if is_pong_text(&text) {
                        *client.shared.last_pong.lock().unwrap() = Instant::now();
                        let _ = client.events_tx.send(WsEvent::PongReceived);
                    } else if let Some(handler) = &client.shared.hooks.on_message {
                        // 关键：handler 在专用执行器线程执行（对齐 C# Task.Run 语义）——
                        // 即使 handler 内部阻塞（同步 IO/长耗时），也不会占用异步
                        // 工作线程，心跳与读循环永不受影响（C# 教训固化）；
                        // enter() 让 handler 内可直接 tokio::spawn / 使用异步定时器。
                        let handler = handler.clone();
                        let msg = WsMessage {
                            text,
                            client: client.clone(),
                        };
                        let rt = rt.clone();
                        HandlerExecutor::global().spawn(Box::new(move || {
                            let _guard = rt.enter();
                            handler(msg);
                        }));
                    }
                    None
                }
                Some(FrameEvent::Ping(payload)) => {
                    match tx.write_frame(Frame::pong(Payload::Owned(payload))).await {
                        Ok(()) => None,
                        Err(e) => Some(format!("Pong 回复失败: {e}")),
                    }
                }
                Some(FrameEvent::Pong) => {
                    *client.shared.last_pong.lock().unwrap() = Instant::now();
                    let _ = client.events_tx.send(WsEvent::PongReceived);
                    None
                }
                Some(FrameEvent::Closed) => {
                    let _ = tx.write_frame(Frame::close(1000, b"")).await;
                    Some("服务端关闭连接".to_string())
                }
                Some(FrameEvent::Ended(r)) => Some(format!("读取结束: {r}")),
                None => Some("读任务已结束".to_string()),
            },
            _ = ping_timer.tick() => {
                let ping = format!("{{\"Type\":\"ping\",\"Timestamp\":{}}}", csharp_ticks_now());
                match tx
                    .write_frame(Frame::text(Payload::Owned(ping.into_bytes())))
                    .await
                {
                    Ok(()) => {
                        last_ping_sent = Some(Instant::now());
                        let _ = client.events_tx.send(WsEvent::PingSent);
                        if let Some(cb) = &client.shared.hooks.on_ping_tick {
                            cb(client.clone());
                        }
                        None
                    }
                    Err(e) => Some(format!("Ping 发送失败: {e}")),
                }
            }
            _ = watchdog.tick() => {
                // 仅在已开始心跳后判活；判据为“距最近一次 Pong 的时长”
                // （对齐 C# `_lastPongReceived` 检查；不能用“距最近一次 Ping”
                //   判——Ping 每周期重发会不断重置计时，超时永远无法触发）
                if last_ping_sent.is_some() {
                    let pong_at = *client.shared.last_pong.lock().unwrap();
                    if pong_at.elapsed() > opts.pong_timeout {
                        Some(format!(
                            "Pong 超时（超过 {:.1} 秒未收到任何 Pong）",
                            opts.pong_timeout.as_secs_f64()
                        ))
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
        };
        if let Some(r) = step {
            break 'session r;
        }
    };

    // 收尾：终止读任务（写半部随 tx 一起释放）
    read_task.abort();
    reason
}

/// `read_frame` 的占位回调（自动行为已关闭，协议不会触发 obligated send）。
fn noop_send(_f: Frame<'_>) -> std::future::Ready<Result<(), std::io::Error>> {
    std::future::ready(Ok(()))
}

/// 是否 Pong 文本（对齐 C# `IsPongMessage`：序数包含检查）。
#[inline]
fn is_pong_text(text: &str) -> bool {
    text.contains("\"Type\":\"pong\"")
}

/// C# `DateTime` Ticks（100ns；0001-01-01 纪元），对齐客户端 Ping 报文数值形态。
fn csharp_ticks_now() -> u64 {
    const EPOCH_DIFF_TICKS: u64 = 621_355_968_000_000_000;
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => EPOCH_DIFF_TICKS + d.as_nanos() as u64 / 100,
        Err(_) => EPOCH_DIFF_TICKS,
    }
}

// ————— 服务端会话（N003：hyper 升级后交此接管）—————

/// 服务端配置。
#[derive(Clone, Debug)]
pub struct WsServerOptions {
    /// 帧层单条消息上限（聚合后；超出即断开）
    pub max_message_size: usize,
    /// 连接空闲超时（`None` = 不超时；收到任何帧即刷新活动时间）
    pub idle_timeout: Option<Duration>,
    /// 发送队列容量（满时业务发送返回失败）
    pub send_queue: usize,
    /// 消息处理器内联执行（会话循环内就地处理——对齐 C#/NewLife 处理模型：
    /// 读→处理→写同一任务完成，最低时延；仅适合纯内存快速处理。阻塞型
    /// 业务保持默认 `false`，走专用处理器线程池——对齐 C# `Task.Run` 隔离语义）
    pub inline_handlers: bool,
}

impl Default for WsServerOptions {
    fn default() -> Self {
        Self {
            max_message_size: 16 * 1024 * 1024,
            idle_timeout: None,
            send_queue: 1024,
            inline_handlers: false,
        }
    }
}

/// 服务端会话钩子（回调均在会话/blocking 池中调用，实现方不得长时间阻塞）。
#[derive(Clone, Default)]
pub struct WsServerHooks {
    /// 连接建立（可立即下发欢迎/注册响应）
    pub on_open: Option<Arc<dyn Fn(WsServerConn) + Send + Sync>>,
    /// 应用消息（在 blocking 池分发——读循环永不阻塞；ping/pong 已在会话层消化）
    pub on_message: Option<Arc<dyn Fn(WsServerMessage) + Send + Sync>>,
    /// 连接关闭（原因）
    pub on_close: Option<Arc<dyn Fn(WsServerConn, String) + Send + Sync>>,
}

/// 服务端视角的应用消息（Pong 已在会话层消化，不会投递）。
#[derive(Clone)]
pub struct WsServerMessage {
    /// 消息文本
    pub text: String,
    /// 连接句柄（可在后台任务中延迟发送响应）
    pub conn: WsServerConn,
}

impl std::fmt::Debug for WsServerMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsServerMessage")
            .field("text", &self.text)
            .finish()
    }
}

/// 服务端发送命令（单一写通道串行化）。
enum ServerCmd {
    Text {
        text: String,
        ack: Option<oneshot::Sender<bool>>,
    },
    Close,
}

/// 服务端连接句柄（可克隆；多任务并发发送经单一写通道天然串行）。
#[derive(Clone)]
pub struct WsServerConn {
    cmd_tx: mpsc::Sender<ServerCmd>,
    connected: Arc<AtomicBool>,
}

impl WsServerConn {
    /// 当前是否已连接。
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    /// 发送文本（fire-and-forget；未连接或队列满返回 `false`）。
    pub fn send_text(&self, text: impl Into<String>) -> bool {
        if !self.is_connected() {
            return false;
        }
        self.cmd_tx
            .try_send(ServerCmd::Text {
                text: text.into(),
                ack: None,
            })
            .is_ok()
    }

    /// 发送文本并等待写入结果（真实送达判定）。
    pub async fn send_text_wait(&self, text: impl Into<String>) -> Result<(), WsError> {
        if !self.is_connected() {
            return Err(WsError::Closed);
        }
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(ServerCmd::Text {
                text: text.into(),
                ack: Some(tx),
            })
            .await
            .is_err()
        {
            return Err(WsError::Closed);
        }
        match rx.await {
            Ok(true) => Ok(()),
            _ => Err(WsError::Closed),
        }
    }

    /// 请求关闭连接（发送 Close 帧后结束会话）。
    pub fn close(&self) {
        let _ = self.cmd_tx.try_send(ServerCmd::Close);
    }
}

/// 服务端 Pong 报文（对齐 C# `WebSocketHelper`：`Timestamp`=UtcNow.Ticks，
/// `ServerTime`=本地时间 `yyyy-MM-dd HH:mm:ss.fff`）。
fn pong_json() -> String {
    let server_time = crate::times::format_datetime_ms(&chrono::Local::now().naive_local());
    format!(
        "{{\"Type\":\"pong\",\"Timestamp\":{},\"ServerTime\":\"{}\"}}",
        csharp_ticks_now(),
        server_time
    )
}

/// 是否 Ping 文本（对齐 C# 服务端 `IsPingMessage`：序数包含检查）。
#[inline]
fn is_ping_text(text: &str) -> bool {
    text.contains("\"Type\":\"ping\"")
}

/// 运行服务端会话（`stream` = 已升级的 IO；返回断开原因）。
///
/// 语义：ping json 自动回 pong json（不进入业务分发）；应用消息在 blocking 池
/// 分发（handler 即使阻塞也不影响心跳/读循环）；业务发送与 Pong 回复共用单一
/// 写通道（WebSocket 不允许并发发送）。
pub async fn run_server_session<S>(
    stream: S,
    hooks: WsServerHooks,
    options: WsServerOptions,
) -> String
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let rt = tokio::runtime::Handle::current();
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<ServerCmd>(options.send_queue.max(1));
    let connected = Arc::new(AtomicBool::new(true));
    let conn = WsServerConn {
        cmd_tx,
        connected: connected.clone(),
    };

    // 帧层接管（Role::Server：读帧自动解掩码、出帧不加掩码）
    let mut ws = WebSocket::after_handshake(stream, Role::Server);
    ws.set_auto_close(false);
    ws.set_auto_pong(false);
    ws.set_max_message_size(options.max_message_size);
    let (rx, mut tx) = ws.split(tokio::io::split);
    let mut reader = FragmentCollectorRead::new(rx);

    // 空闲看门狗（可选；任何帧到达即刷新）
    let idle_every = options
        .idle_timeout
        .map(|d| std::cmp::max(d / 3, Duration::from_millis(100)))
        .unwrap_or(Duration::from_secs(3600));
    let mut idle_timer = tokio::time::interval(idle_every);
    idle_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_activity = Instant::now();
    let mut send_fn = noop_send;

    if let Some(cb) = &hooks.on_open {
        cb(conn.clone());
    }

    let reason = loop {
        let step: Option<String> = tokio::select! {
            cmd = cmd_rx.recv() => match cmd {
                Some(ServerCmd::Text { text, ack }) => {
                    let res = tx
                        .write_frame(Frame::text(Payload::Owned(text.into_bytes())))
                        .await;
                    if let Some(ack) = ack {
                        let _ = ack.send(res.is_ok());
                    }
                    match res {
                        Ok(()) => None,
                        Err(e) => Some(format!("发送失败: {e}")),
                    }
                }
                Some(ServerCmd::Close) => {
                    let _ = tx
                        .write_frame(Frame::close(1000, "服务端关闭".as_bytes()))
                        .await;
                    Some("服务端关闭连接".to_string())
                }
                None => Some("命令通道关闭".to_string()),
            },
            frame = reader.read_frame(&mut send_fn) => match frame {
                Ok(frame) => {
                    last_activity = Instant::now();
                    match frame.opcode {
                        OpCode::Text => {
                            let text = String::from_utf8_lossy(&frame.payload).into_owned();
                            if is_ping_text(&text) {
                                // 对齐 C# 服务端：ping json → pong json（不进入业务分发）
                                let pong = pong_json();
                                match tx
                                    .write_frame(Frame::text(Payload::Owned(pong.into_bytes())))
                                    .await
                                {
                                    Ok(()) => None,
                                    Err(e) => Some(format!("Pong 回复失败: {e}")),
                                }
                            } else if is_pong_text(&text) {
                                // 客户端主动 Pong：服务端无需处理
                                None
                            } else if let Some(handler) = &hooks.on_message {
                                let handler = handler.clone();
                                let msg = WsServerMessage {
                                    text,
                                    conn: conn.clone(),
                                };
                                if options.inline_handlers {
                                    // 内联模式：会话循环内就地处理（对齐 C#/NewLife 模型）——
                                    // 读→处理→写同一任务完成；处理后立即就地冲刷排队发送，
                                    // 省掉一次任务唤醒（高吞吐场景关键路径，基准见 tools/bench-net）
                                    handler(msg);
                                    flush_pending(&mut cmd_rx, &mut tx).await
                                } else {
                                    // 隔离模式：handler 在专用执行器线程执行（对齐 C# Task.Run 语义）——
                                    // 即使阻塞也不占用异步工作线程（读循环与发送不受影响）
                                    let rt = rt.clone();
                                    HandlerExecutor::global().spawn(Box::new(move || {
                                        let _guard = rt.enter();
                                        handler(msg);
                                    }));
                                    None
                                }
                            } else {
                                None
                            }
                        }
                        OpCode::Ping => {
                            let payload = frame.payload.to_vec();
                            match tx.write_frame(Frame::pong(Payload::Owned(payload))).await {
                                Ok(()) => None,
                                Err(e) => Some(format!("Pong 回复失败: {e}")),
                            }
                        }
                        OpCode::Pong => None,
                        OpCode::Close => {
                            let _ = tx.write_frame(Frame::close(1000, b"")).await;
                            Some("客户端关闭连接".to_string())
                        }
                        _ => None,
                    }
                }
                Err(e) => Some(format!("读取结束: {e}")),
            },
            _ = idle_timer.tick() => {
                if let Some(d) = options.idle_timeout {
                    if last_activity.elapsed() > d {
                        Some(format!(
                            "空闲超时（超过 {:.1} 秒无任何数据）",
                            d.as_secs_f64()
                        ))
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
        };
        if let Some(r) = step {
            break r;
        }
    };

    connected.store(false, Ordering::Release);
    if let Some(cb) = &hooks.on_close {
        cb(conn, reason.clone());
    }
    reason
}

/// 就地冲刷已排队的发送命令（内联模式：同任务内完成读→处理→写；返回 `Some` 即应结束会话）。
async fn flush_pending<W>(
    cmd_rx: &mut mpsc::Receiver<ServerCmd>,
    tx: &mut fastwebsockets::WebSocketWrite<W>,
) -> Option<String>
where
    W: AsyncWrite + Unpin,
{
    while let Ok(cmd) = cmd_rx.try_recv() {
        match cmd {
            ServerCmd::Text { text, ack } => {
                let res = tx
                    .write_frame(Frame::text(Payload::Owned(text.into_bytes())))
                    .await;
                if let Some(ack) = ack {
                    let _ = ack.send(res.is_ok());
                }
                if let Err(e) = res {
                    return Some(format!("发送失败: {e}"));
                }
            }
            ServerCmd::Close => {
                let _ = tx
                    .write_frame(Frame::close(1000, "服务端关闭".as_bytes()))
                    .await;
                return Some("服务端关闭连接".to_string());
            }
        }
    }
    None
}

// ————— 握手（自管：TCP + HTTP/1.1 升级 + Accept 校验）—————

/// 计算 `Sec-WebSocket-Accept`（RFC 6455；客户端校验与服务端应答 N003 共用）。
pub fn ws_accept(key: &str) -> String {
    const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
    let mut h = Sha1::new();
    h.update(key.as_bytes());
    h.update(GUID.as_bytes());
    base64_encode(&h.finalize())
}

/// 连接并完成升级握手（超时由调用方施加；返回值可能携带预读的首帧字节）。
async fn connect_handshake(url: &str) -> Result<PrefixedStream<TcpStream>, WsError> {
    let target = parse_ws_url(url)?;
    let stream = TcpStream::connect((target.host.as_str(), target.port))
        .await
        .map_err(WsError::Io)?;
    let _ = stream.set_nodelay(true);
    let mut stream = stream;

    // Sec-WebSocket-Key：16 随机字节 → base64
    let mut key_bytes = [0u8; 16];
    rand::thread_rng().fill(&mut key_bytes);
    let key = base64_encode(&key_bytes);

    let request = format!(
        "GET {path} HTTP/1.1\r\n\
         Host: {host}:{port}\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: {key}\r\n\
         Sec-WebSocket-Version: 13\r\n\r\n",
        path = target.path,
        host = target.host,
        port = target.port,
    );
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(WsError::Io)?;
    stream.flush().await.map_err(WsError::Io)?;

    // 读响应头（上限 8KB）
    let mut buf: Vec<u8> = Vec::with_capacity(512);
    let mut chunk = [0u8; 512];
    let header_end = loop {
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
        if buf.len() > 8192 {
            return Err(WsError::Handshake("响应头超过 8KB".into()));
        }
        let n = stream.read(&mut chunk).await.map_err(WsError::Io)?;
        if n == 0 {
            return Err(WsError::Handshake(format!(
                "服务端提前关闭（已读 {} 字节）",
                buf.len()
            )));
        }
        buf.extend_from_slice(&chunk[..n]);
    };

    let head = String::from_utf8_lossy(&buf[..header_end]);
    let status_line = head.lines().next().unwrap_or("");
    if !status_line.contains(" 101") {
        return Err(WsError::Handshake(format!("升级失败: {status_line}")));
    }
    let expect = ws_accept(&key);
    let mut accept_ok = false;
    for line in head.lines().skip(1) {
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("sec-websocket-accept") {
                accept_ok = value.trim() == expect;
                break;
            }
        }
    }
    if !accept_ok {
        return Err(WsError::Handshake("Sec-WebSocket-Accept 校验失败".into()));
    }

    // 响应头之后的任何字节都可能是首个帧（服务端提前发送/合并写）——包装保留，勿丢
    Ok(PrefixedStream::new(stream, buf[header_end..].to_vec()))
}

/// 握手后剩余字节 + 底层流包装（保证首个帧的预读字节不丢失）。
struct PrefixedStream<S> {
    prefix: Vec<u8>,
    offset: usize,
    inner: S,
}

impl<S> PrefixedStream<S> {
    fn new(inner: S, prefix: Vec<u8>) -> Self {
        Self {
            prefix,
            offset: 0,
            inner,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for PrefixedStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if this.offset < this.prefix.len() {
            let remaining = &this.prefix[this.offset..];
            let n = std::cmp::min(buf.remaining(), remaining.len());
            buf.put_slice(&remaining[..n]);
            this.offset += n;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for PrefixedStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write_vectored(cx, bufs)
    }
}

/// 已解析的 ws URL。
struct WsTarget {
    host: String,
    port: u16,
    path: String,
}

/// 解析 `ws://host[:port][/path][?query]`（wss 待 net-tls）。
fn parse_ws_url(url: &str) -> Result<WsTarget, WsError> {
    let rest = url.strip_prefix("ws://").ok_or_else(|| {
        if url.starts_with("wss://") {
            WsError::Handshake("wss 暂未支持（待 net-tls）".into())
        } else {
            WsError::Handshake(format!("URL 必须以 ws:// 开头: {url}"))
        }
    })?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.parse()
                .map_err(|_| WsError::Handshake(format!("端口非法: {p}")))?,
        ),
        None => (authority.to_string(), 80),
    };
    if host.is_empty() {
        return Err(WsError::Handshake("缺少主机名".into()));
    }
    Ok(WsTarget {
        host,
        port,
        path: path.to_string(),
    })
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// 标准 base64 编码（含 `=` 填充；握手 Key 与 Accept 用）。
fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((n >> 6) & 63) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(n & 63) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

// ————— 单元测试（纯函数）—————

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_accept_matches_rfc6455_example() {
        // RFC 6455 §1.3 已知向量
        assert_eq!(
            ws_accept("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn base64_encoding_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_encode(&[0u8; 16]).len(), 24);
    }

    #[test]
    fn parse_ws_url_forms() {
        let t = parse_ws_url("ws://127.0.0.1:8080/ws?a=1").unwrap();
        assert_eq!(
            (t.host.as_str(), t.port, t.path.as_str()),
            ("127.0.0.1", 8080, "/ws?a=1")
        );
        let t2 = parse_ws_url("ws://example.com").unwrap();
        assert_eq!(
            (t2.host.as_str(), t2.port, t2.path.as_str()),
            ("example.com", 80, "/")
        );
        assert!(parse_ws_url("wss://example.com").is_err());
        assert!(parse_ws_url("http://example.com").is_err());
    }

    #[test]
    fn ticks_like_csharp() {
        assert!(csharp_ticks_now() > 621_355_968_000_000_000);
    }
}
