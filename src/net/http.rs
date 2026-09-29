//! net::http —— HTTP 服务端与语义层（自研，对齐 DH.NCore `HttpServer/HttpRouter`）。
//!
//! - N003：监听循环、请求/响应自有类型（不泄露 hyper 类型给业务）、WebSocket 升级
//!   （hyper `serve_connection().with_upgrades()` + `TokioIo` 适配，升级后交
//!   [`crate::net::ws::run_server_session`]）；
//! - N004：统一返回 [`DGResult`]（StateCode 对齐 `Pek.Helpers.StateCode`；序列化对齐
//!   .NET `System.Text.Json` 字段顺序与转义）；路由与中间件见 [`crate::net::router`]；
//! - 待办：HTTP 客户端封装（调用 DHDeploy.Server REST，N006 前补齐）。

use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::{TcpListener, TcpStream};

use super::ws::{self, WsServerHooks, WsServerOptions};

/// 请求体读取上限默认值（64MB；超出返回 413）。
pub const DEFAULT_MAX_BODY_SIZE: usize = 64 * 1024 * 1024;

// ————— 请求 / 响应（自有类型）—————

/// HTTP 请求（语义层类型；不暴露 hyper 泛型）。
#[derive(Debug, Clone)]
pub struct HttpRequest {
    /// 方法（GET/POST/...）
    pub method: String,
    /// 路径（不含查询串）
    pub path: String,
    /// 查询串（原始形态，未解码；无则为空串）
    pub query: String,
    /// 请求头（保序；名称保持原样）
    pub headers: Vec<(String, String)>,
    /// 请求体（已按上限收全；单帧请求为引用计数共享的零拷贝字节，带引用计数下为共享分片）
    pub body: Bytes,
}

impl HttpRequest {
    /// 头查询（大小写不敏感；首个匹配）。
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// 是否 WebSocket 升级请求（`Upgrade: websocket` + `Connection` 含 `upgrade`）。
    pub fn is_websocket_upgrade(&self) -> bool {
        let upgrade_ok = self
            .header("upgrade")
            .map(|v| v.eq_ignore_ascii_case("websocket"))
            .unwrap_or(false);
        let conn_ok = self
            .header("connection")
            .map(|v| v.to_ascii_lowercase().contains("upgrade"))
            .unwrap_or(false);
        upgrade_ok && conn_ok
    }
}

/// HTTP 响应（语义层类型）。
#[derive(Debug, Clone)]
pub struct HttpResponse {
    /// 状态码
    pub status: u16,
    /// 响应头
    pub headers: Vec<(String, String)>,
    /// 响应体
    pub body: Bytes,
}

impl HttpResponse {
    /// 纯文本响应（`text/plain; charset=utf-8`）。
    pub fn text(status: u16, text: impl Into<String>) -> Self {
        Self {
            status,
            headers: vec![(
                "Content-Type".to_string(),
                "text/plain; charset=utf-8".to_string(),
            )],
            body: Bytes::from(text.into().into_bytes()),
        }
    }

    /// JSON 响应（`application/json; charset=utf-8`）。
    pub fn json(status: u16, json: impl Into<String>) -> Self {
        Self {
            status,
            headers: vec![(
                "Content-Type".to_string(),
                "application/json; charset=utf-8".to_string(),
            )],
            body: Bytes::from(json.into().into_bytes()),
        }
    }

    /// 二进制响应（自定 Content-Type；`Bytes` 直通为零拷贝）。
    pub fn bytes(status: u16, content_type: &str, data: impl Into<Bytes>) -> Self {
        Self {
            status,
            headers: vec![("Content-Type".to_string(), content_type.to_string())],
            body: data.into(),
        }
    }

    /// 空响应（无体、无 Content-Type；如 101/204）。
    pub fn empty(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Bytes::new(),
        }
    }

    /// 追加响应头（链式）。
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    /// 转 hyper 响应。
    fn into_hyper(self) -> Response<Full<Bytes>> {
        let mut builder = Response::builder().status(self.status);
        for (k, v) in &self.headers {
            builder = builder.header(k.as_str(), v.as_str());
        }
        builder
            .body(Full::new(self.body))
            .unwrap_or_else(|_| Response::new(Full::new(Bytes::new())))
    }
}

/// 处理结果：普通响应，或升级为 WebSocket（101 由服务端自动应答）。
#[derive(Clone)]
pub enum HttpOutcome {
    /// 普通 HTTP 响应
    Response(HttpResponse),
    /// 升级为 WebSocket 会话（需请求本身是合法升级请求，否则回 400）
    WebSocket(WsServerHooks),
}

/// 请求处理器（普通 async 闭包经 [`handler`] 包装为 boxed future）。
pub type HttpHandler =
    Arc<dyn Fn(HttpRequest) -> Pin<Box<dyn Future<Output = HttpOutcome> + Send>> + Send + Sync>;

/// 便捷构造 [`HttpHandler`]（`async` 闭包 → boxed）。
pub fn handler<F, Fut>(f: F) -> HttpHandler
where
    F: Fn(HttpRequest) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = HttpOutcome> + Send + 'static,
{
    Arc::new(move |req| Box::pin(f(req)))
}

// ————— 服务端 —————

/// 服务端配置。
#[derive(Clone, Debug)]
pub struct HttpServerOptions {
    /// 请求体上限（超出返回 413）
    pub max_body_size: usize,
    /// WebSocket 会话配置（升级后使用）
    pub ws: WsServerOptions,
    /// 每连接独立线程（current_thread 运行时）——对齐 NewLife/IOCP 的
    /// “完成线程直处理”模型：数据到达直接唤醒该连接自己的线程处理，
    /// 省去多线程运行时的跨线程序列化，消息往返时延更低
    /// （基准实测回环乒乓吞吐 +~15%，见 tools/bench-net）；
    /// 代价是每连接一线程（默认每线程保留 2MB 虚拟栈），适合连接数在
    /// 数百以内的场景（Agent 服务端典型规模）；大规模连接保持默认 `false`
    pub thread_per_connection: bool,
    /// 分片线程池容量（`0` = 关闭）。`>0` 时启用 N 个分片线程，每片一个
    /// current_thread 运行时承载多连接：数据到达唤醒所属分片线程本身
    /// （同线程任务调度、无跨线程任务移交）——高并发下线程数比
    /// “每连接一线程”少一个量级，CPU 开销与尾时延同时占优
    /// （基准实测；与 `thread_per_connection` 同时开启时后者优先）
    pub conn_shards: usize,
}

impl Default for HttpServerOptions {
    fn default() -> Self {
        Self {
            max_body_size: DEFAULT_MAX_BODY_SIZE,
            ws: WsServerOptions::default(),
            thread_per_connection: false,
            conn_shards: 0,
        }
    }
}

/// HTTP 服务端（hyper HTTP/1.1；WS 升级支持）。
///
/// ```no_run
/// # async fn run() -> std::io::Result<()> {
/// use dhrust::net::http::{handler, HttpOutcome, HttpRequest, HttpResponse, HttpServer};
///
/// let server = HttpServer::bind("0.0.0.0:8080").await?;
/// let svc = handler(|req: HttpRequest| async move {
///     HttpOutcome::Response(HttpResponse::text(200, format!("路径 {}", req.path)))
/// });
/// server.serve(svc).await
/// # }
/// ```
/// 服务端 TLS 接受器（无 `net-tls` 特性时为占位类型，恒为 None）。
#[cfg(feature = "net-tls")]
type ServerTlsAcceptor = tokio_rustls::TlsAcceptor;
#[cfg(not(feature = "net-tls"))]
type ServerTlsAcceptor = ();

pub struct HttpServer {
    listener: TcpListener,
    tls: Option<ServerTlsAcceptor>,
}

impl HttpServer {
    /// 绑定监听地址（`127.0.0.1:0` 随机端口便于测试）。
    pub async fn bind(addr: impl tokio::net::ToSocketAddrs) -> std::io::Result<HttpServer> {
        let listener = TcpListener::bind(addr).await?;
        Ok(HttpServer {
            listener,
            tls: None,
        })
    }

    /// 绑定 HTTPS 监听地址（TLS 1.2/1.3；PEM 证书链 + 私钥；需 `net-tls` 特性）。
    #[cfg(feature = "net-tls")]
    pub async fn bind_tls(
        addr: impl tokio::net::ToSocketAddrs,
        cert_pem: &[u8],
        key_pem: &[u8],
    ) -> std::io::Result<HttpServer> {
        let config = super::tls::server_config(cert_pem, key_pem)?;
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind(addr).await?;
        Ok(HttpServer {
            listener,
            tls: Some(acceptor),
        })
    }

    /// 实际监听地址。
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// 运行服务循环（默认配置；每连接独立任务，永不返回除非 accept 失败）。
    pub async fn serve(self, handler: HttpHandler) -> std::io::Result<()> {
        self.serve_with(handler, HttpServerOptions::default()).await
    }

    /// 运行服务循环（自定义配置）。
    pub async fn serve_with(
        self,
        handler: HttpHandler,
        options: HttpServerOptions,
    ) -> std::io::Result<()> {
        // 分片线程池（可选）：每片一个 current_thread 运行时承载多连接
        let shards = if options.conn_shards > 0 && !options.thread_per_connection {
            Some(ShardPool::start(
                options.conn_shards,
                handler.clone(),
                options.clone(),
                self.tls.clone(),
            ))
        } else {
            None
        };
        let mut rr: usize = 0;
        loop {
            let (tcp, _peer) = self.listener.accept().await?;
            let _ = tcp.set_nodelay(true);
            let handler = handler.clone();
            let options = options.clone();
            let tls = self.tls.clone();
            if options.thread_per_connection {
                // 每连接独立线程（current_thread 运行时）：数据到达直接唤醒本线程
                // 处理（对齐 NewLife/IOCP 完成线程直处理模型）；基准实测回环乒乓
                // 吞吐 +~20%（见 tools/bench-net README）
                //
                // 关键：tokio TcpStream 绑定在主运行时的 reactor 上，禁止跨运行时
                // 使用——必须先 `into_std` 脱钩，进入连接线程后再 `from_std` 注册
                // 到该线程自己的 current_thread 运行时（曾直接跨线程使用导致负载
                // 下连接被中断：WSAECONNABORTED/WSAECONNRESET）
                let std_tcp = match tcp.into_std() {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let _ = std_tcp.set_nonblocking(true);
                let _ = std::thread::Builder::new()
                    .name("dhrust-conn".to_string())
                    .spawn(move || {
                        let rt = match tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                        {
                            Ok(rt) => rt,
                            Err(_) => return,
                        };
                        rt.block_on(async move {
                            let tcp = match TcpStream::from_std(std_tcp) {
                                Ok(t) => t,
                                Err(_) => return,
                            };
                            run_connection(tcp, tls, handler, options).await;
                        });
                    });
            } else if let Some(shards) = &shards {
                // 分片池：连接按轮次分发到分片线程（socket 脱钩后重注册到分片运行时）
                if let Ok(std_tcp) = tcp.into_std() {
                    let _ = std_tcp.set_nonblocking(true);
                    shards.dispatch(std_tcp, rr);
                    rr = rr.wrapping_add(1);
                }
            } else {
                tokio::spawn(run_connection(tcp, tls, handler, options));
            }
        }
    }
}

/// 分片线程池：N 个线程各持一个 current_thread 运行时，承载多连接。
///
/// 唤醒模型：socket 注册在其所属分片线程的 reactor 上，数据到达时唤醒的
/// 就是该分片线程本身（同线程任务调度，无跨线程 IPI）；线程数 = 分片数
/// （而非连接数），高并发下 CPU 开销与尾时延同时占优。
struct ShardPool {
    senders: Vec<tokio::sync::mpsc::UnboundedSender<std::net::TcpStream>>,
}

impl ShardPool {
    /// 启动分片线程池（连接经无界通道移交；`send` 为同步调用，可在运行时外使用）。
    fn start(
        n: usize,
        handler: HttpHandler,
        options: HttpServerOptions,
        tls: Option<ServerTlsAcceptor>,
    ) -> ShardPool {
        let mut senders = Vec::with_capacity(n);
        for i in 0..n {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<std::net::TcpStream>();
            senders.push(tx);
            let handler = handler.clone();
            let options = options.clone();
            let tls = tls.clone();
            let _ = std::thread::Builder::new()
                .name(format!("dhrust-shard-{i}"))
                .spawn(move || {
                    let rt = match tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    {
                        Ok(rt) => rt,
                        Err(_) => return,
                    };
                    rt.block_on(async move {
                        // 收连接任务：std 流重注册到本分片运行时，再逐连接托管
                        tokio::spawn(async move {
                            while let Some(std_tcp) = rx.recv().await {
                                let _ = std_tcp.set_nonblocking(true);
                                let handler = handler.clone();
                                let options = options.clone();
                                let tls = tls.clone();
                                tokio::spawn(async move {
                                    if let Ok(tcp) = TcpStream::from_std(std_tcp) {
                                        run_connection(tcp, tls, handler, options).await;
                                    }
                                });
                            }
                        });
                        // 保持运行时活跃，直至进程退出（发送端随服务循环存活）
                        std::future::pending::<()>().await;
                    });
                });
        }
        ShardPool { senders }
    }

    /// 分发连接（轮次由调用方维护）。
    fn dispatch(&self, std_tcp: std::net::TcpStream, idx: usize) {
        if let Some(tx) = self.senders.get(idx % self.senders.len()) {
            let _ = tx.send(std_tcp);
        }
    }
}

/// 连接入口：配置 TLS 时先完成握手（失败静默丢弃——扫描/探测流量不产生噪声日志）；
/// 随后交由单连接服务。无 `net-tls` 特性时 `ServerTlsAcceptor` 为占位类型，恒直通。
async fn run_connection(
    tcp: TcpStream,
    tls: Option<ServerTlsAcceptor>,
    handler: HttpHandler,
    options: HttpServerOptions,
) {
    #[cfg(feature = "net-tls")]
    if let Some(acceptor) = tls {
        if let Ok(stream) = acceptor.accept(tcp).await {
            serve_connection(stream, handler, options).await;
        }
        return;
    }
    #[cfg(not(feature = "net-tls"))]
    let _ = tls;
    serve_connection(tcp, handler, options).await;
}

/// 单连接服务（hyper HTTP/1.1 + 升级支持）。
///
/// 升级后的 WS 会话由**本任务就地继续驱动**（不 spawn 独立任务）：在
/// “每连接独立线程”模式下 `block_on` 返回即销毁运行时，会让被 spawn 的会话
/// 任务被连带取消（曾表现为 101 后连接被 RST）；就地续跑同时少一次任务跳转。
async fn serve_connection<S>(stream: S, handler: HttpHandler, options: HttpServerOptions)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let upgrade_slot: Arc<Mutex<Option<(hyper::upgrade::OnUpgrade, WsServerHooks)>>> =
        Arc::new(Mutex::new(None));
    // 会话配置先取出（options 会被闭包 move）
    let ws_options = options.ws.clone();
    let slot = upgrade_slot.clone();
    let service = service_fn(move |req: Request<Incoming>| {
        let handler = handler.clone();
        let options = options.clone();
        let slot = slot.clone();
        async move { Ok::<_, Infallible>(handle_http(req, handler, options, slot).await) }
    });
    // 连接错误（含正常关闭）静默；升级路径由 with_upgrades 支撑
    let _ = http1::Builder::new()
        .serve_connection(TokioIo::new(stream), service)
        .with_upgrades()
        .await;

    // 若本次连接请求过升级：等待 hyper 完成 IO 移交并就地驱动会话
    let pending = {
        let mut guard = upgrade_slot.lock().unwrap_or_else(|e| e.into_inner());
        guard.take()
    };
    if let Some((on_upgrade, hooks)) = pending {
        if let Ok(upgraded) = on_upgrade.await {
            let io = TokioIo::new(upgraded);
            ws::run_server_session(io, hooks, ws_options).await;
        }
    }
}

/// 处理单个请求：收集请求 → 处理器 → 普通响应或 WS 升级。
async fn handle_http(
    mut req: Request<Incoming>,
    handler: HttpHandler,
    options: HttpServerOptions,
    upgrade_slot: Arc<Mutex<Option<(hyper::upgrade::OnUpgrade, WsServerHooks)>>>,
) -> Response<Full<Bytes>> {
    // 升级意图必须先登记（handler 返回 WebSocket 时由 hyper 完成移交）
    let ws_candidate = is_ws_candidate(req.headers());
    let on_upgrade = if ws_candidate {
        Some(hyper::upgrade::on(&mut req))
    } else {
        None
    };

    let (parts, body) = req.into_parts();
    let body_bytes = match collect_body(body, options.max_body_size).await {
        Ok(b) => b,
        Err(_) => return HttpResponse::text(413, "请求体过大").into_hyper(),
    };

    let http_req = HttpRequest {
        method: parts.method.to_string(),
        path: parts.uri.path().to_string(),
        query: parts.uri.query().unwrap_or("").to_string(),
        headers: parts
            .headers
            .iter()
            .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
            .collect(),
        body: body_bytes,
    };

    match (handler)(http_req).await {
        HttpOutcome::Response(r) => r.into_hyper(),
        HttpOutcome::WebSocket(hooks) => {
            // 合法性校验（对端可能伪造 Upgrade 头或版本不符）
            let (Some(on_upgrade), Some(key)) = (
                on_upgrade,
                parts
                    .headers
                    .get("sec-websocket-key")
                    .and_then(|v| v.to_str().ok()),
            ) else {
                return HttpResponse::text(400, "非法的 WebSocket 升级请求").into_hyper();
            };
            let version_ok = parts
                .headers
                .get("sec-websocket-version")
                .map(|v| v.as_bytes() == b"13")
                .unwrap_or(false);
            if !version_ok {
                return HttpResponse::text(426, "仅支持 WebSocket 版本 13")
                    .with_header("Sec-WebSocket-Version", "13")
                    .into_hyper();
            }

            let accept = ws::ws_accept(key);
            // 升级点交给 serve_connection 在连接任务上继续驱动（不可 spawn 独立任务，
            // 理由见 serve_connection 注释）
            *upgrade_slot.lock().unwrap_or_else(|e| e.into_inner()) = Some((on_upgrade, hooks));

            HttpResponse::empty(101)
                .with_header("Upgrade", "websocket")
                .with_header("Connection", "Upgrade")
                .with_header("Sec-WebSocket-Accept", &accept)
                .into_hyper()
        }
    }
}

/// 升级候选判定（hyper 头映射版）。
fn is_ws_candidate(headers: &hyper::HeaderMap) -> bool {
    let upgrade_ok = headers
        .get(hyper::header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false);
    let conn_ok = headers
        .get(hyper::header::CONNECTION)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_ascii_lowercase().contains("upgrade"))
        .unwrap_or(false);
    upgrade_ok && conn_ok
}

/// 收集请求体（带上限；超出即中断返回 `Err`）。单帧请求零拷贝直通（引用计数共享）。
async fn collect_body(body: Incoming, limit: usize) -> Result<Bytes, ()> {
    let mut body = body;
    let mut first: Option<Bytes> = None;
    let mut extra: Vec<Bytes> = Vec::new();
    let mut total: usize = 0;
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| ())?;
        if let Ok(data) = frame.into_data() {
            total += data.len();
            if total > limit {
                return Err(());
            }
            if first.is_none() {
                first = Some(data);
            } else {
                extra.push(data);
            }
        }
    }
    match first {
        None => Ok(Bytes::new()),
        Some(first) if extra.is_empty() => Ok(first),
        Some(first) => {
            let mut v: Vec<u8> = Vec::with_capacity(total);
            v.extend_from_slice(&first);
            for chunk in &extra {
                v.extend_from_slice(chunk);
            }
            Ok(Bytes::from(v))
        }
    }
}

// ————— 状态码与统一返回（对齐 C# `Pek.Helpers.StateCode` / `Pek.Models.DGResult`）—————

/// 状态码常量（数值与 `Pek.Helpers.StateCode` 枚举一致）。
pub mod state {
    /// 成功
    pub const OK: i32 = 1;
    /// 失败
    pub const FAIL: i32 = 2;
    /// 限流繁忙
    pub const BUSY: i32 = 99;
    /// 请求（或处理）成功
    pub const STATUS: i32 = 200;
    /// 内部请求出错
    pub const ERROR: i32 = 500;
    /// 未授权标识
    pub const UNAUTHORIZED: i32 = 401;
    /// 请求参数不完整或不正确
    pub const PARAMETER_ERROR: i32 = 400;
    /// 请求 TOKEN 失效
    pub const TOKEN_INVALID: i32 = 403;
    /// HTTP 请求类型不合法
    pub const HTTP_METHOD_ERROR: i32 = 405;
    /// HTTP 请求不合法
    pub const HTTP_REQUEST_ERROR: i32 = 406;
    /// URL 已经失效
    pub const URL_EXPIRE_ERROR: i32 = 407;
    /// 部分出错
    pub const PARTIAL_ERROR: i32 = 999;
}

/// 统一返回（对齐 C# `Pek.Models.DGResult`）。
///
/// 序列化为 .NET `System.Text.Json` 兼容形态：字段顺序
/// `code / errCode / message / data / extData / operationTime / id`，
/// `null` 字段照常输出（对齐 `JsonSerializerDefaults.Web` 默认行为）。
#[derive(Clone, Debug)]
pub struct DGResult {
    /// 状态码（见 [`state`] 常量）
    pub code: i32,
    /// 错误码
    pub err_code: i32,
    /// 消息
    pub message: Option<String>,
    /// 数据（任意 JSON 值，如 `serde_json::json!({...})`）
    pub data: Option<serde_json::Value>,
    /// 其他数据
    pub ext_data: Option<serde_json::Value>,
    /// 操作时间（本地时间 ISO 8601）
    pub operation_time: String,
    /// 标识
    pub id: Option<String>,
}

impl DGResult {
    /// 成功（携带数据；对齐 `new DGResult { Code = StateCode.Ok, Data = ... }`）。
    pub fn ok(data: serde_json::Value) -> Self {
        Self {
            code: state::OK,
            err_code: 0,
            message: None,
            data: Some(data),
            ext_data: None,
            operation_time: now_iso8601(),
            id: None,
        }
    }

    /// 成功（无数据）。
    pub fn ok_empty() -> Self {
        Self {
            code: state::OK,
            err_code: 0,
            message: None,
            data: None,
            ext_data: None,
            operation_time: now_iso8601(),
            id: None,
        }
    }

    /// 失败（携带消息；对齐 `new DGResult { Code = StateCode.Fail, Message = ... }`）。
    pub fn fail(message: impl Into<String>) -> Self {
        Self {
            code: state::FAIL,
            err_code: 0,
            message: Some(message.into()),
            data: None,
            ext_data: None,
            operation_time: now_iso8601(),
            id: None,
        }
    }

    /// 指定状态码与消息。
    pub fn error(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            err_code: 0,
            message: Some(message.into()),
            data: None,
            ext_data: None,
            operation_time: now_iso8601(),
            id: None,
        }
    }

    /// 序列化为 JSON 文本（字段顺序与转义对齐 .NET `System.Text.Json`）。
    pub fn to_json(&self) -> String {
        let mut out = String::with_capacity(192);
        out.push_str("{\"code\":");
        out.push_str(&self.code.to_string());
        out.push_str(",\"errCode\":");
        out.push_str(&self.err_code.to_string());
        out.push_str(",\"message\":");
        match &self.message {
            Some(m) => push_json_string(&mut out, m),
            None => out.push_str("null"),
        }
        out.push_str(",\"data\":");
        match &self.data {
            Some(v) => push_json_value(&mut out, v),
            None => out.push_str("null"),
        }
        out.push_str(",\"extData\":");
        match &self.ext_data {
            Some(v) => push_json_value(&mut out, v),
            None => out.push_str("null"),
        }
        out.push_str(",\"operationTime\":");
        push_json_string(&mut out, &self.operation_time);
        out.push_str(",\"id\":");
        match &self.id {
            Some(id) => push_json_string(&mut out, id),
            None => out.push_str("null"),
        }
        out.push('}');
        out
    }

    /// 转 HTTP 响应（`200` + `application/json; charset=utf-8`）。
    pub fn to_response(&self) -> HttpResponse {
        HttpResponse::json(200, self.to_json())
    }
}

/// 本地时间 ISO 8601（对齐 .NET 序列化形态 `yyyy-MM-ddTHH:mm:ss.FFFFFFFK`，尾零裁剪）。
fn now_iso8601() -> String {
    let now = chrono::Local::now();
    let mut frac = format!("{:07}", now.timestamp_subsec_nanos() / 100);
    while frac.ends_with('0') {
        frac.pop();
    }
    if frac.is_empty() {
        format!("{}{}", now.format("%Y-%m-%dT%H:%M:%S"), now.format("%:z"))
    } else {
        format!(
            "{}.{}{}",
            now.format("%Y-%m-%dT%H:%M:%S"),
            frac,
            now.format("%:z")
        )
    }
}

/// JSON 字符串转义（对齐 .NET `JavaScriptEncoder.Default`：非 ASCII → `\uXXXX`（大写十六进制）、
/// HTML 敏感字符 `< > & '` 转义、控制字符短转义 `\b \t \n \f \r`）。
pub fn json_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    push_json_string(&mut out, text);
    out
}

/// 写入带引号的 JSON 字符串（快速路径：无需转义时整段拷贝）。
fn push_json_string(out: &mut String, text: &str) {
    use std::fmt::Write as _;
    out.push('"');
    if !text.bytes().any(needs_json_escape) {
        out.push_str(text);
        out.push('"');
        return;
    }
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            '<' => out.push_str("\\u003C"),
            '>' => out.push_str("\\u003E"),
            '&' => out.push_str("\\u0026"),
            '\'' => out.push_str("\\u0027"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04X}", c as u32);
            }
            c if (c as u32) > 0x7F => {
                let u = c as u32;
                if u <= 0xFFFF {
                    let _ = write!(out, "\\u{:04X}", u);
                } else {
                    // 非 BMP：UTF-16 代理对（对齐 .NET 字符串形态）
                    let v = u - 0x10000;
                    let _ = write!(
                        out,
                        "\\u{:04X}\\u{:04X}",
                        0xD800 + (v >> 10),
                        0xDC00 + (v & 0x3FF)
                    );
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[inline]
fn needs_json_escape(b: u8) -> bool {
    b >= 0x80 || b < 0x20 || matches!(b, b'"' | b'\\' | b'<' | b'>' | b'&' | b'\'')
}

/// 递归写入 JSON 值（对象保持插入序；转义对齐 .NET）。
fn push_json_value(out: &mut String, value: &serde_json::Value) {
    use serde_json::Value;
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => push_json_string(out, s),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                push_json_value(out, item);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (i, (k, v)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                push_json_string(out, k);
                out.push(':');
                push_json_value(out, v);
            }
            out.push('}');
        }
    }
}
