//! net::http —— HTTP 服务端与语义层（自研，对齐 DH.NCore `HttpServer/HttpRouter`）。
//!
//! N003 已落地：监听循环、请求/响应自有类型（不泄露 hyper 类型给业务）、
//! WebSocket 升级（hyper `serve_connection().with_upgrades()` + `TokioIo` 适配，
//! 升级后交 [`crate::net::ws::run_server_session`]）。
//! N004 待落地：Map/Use 路由（前缀注册 + 中间件链）、请求上下文（会话/租户/记录）、
//! 统一返回（StateCode / DGResult 语义对齐）、静态与流式响应。
//!
//! 客户端封装（调用 DHDeploy.Server REST / 本地星尘服务）在 N006 前补齐。

use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;

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
    /// 请求体（已按上限收全）
    pub body: Vec<u8>,
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
    pub body: Vec<u8>,
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
            body: text.into().into_bytes(),
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
            body: json.into().into_bytes(),
        }
    }

    /// 二进制响应（自定 Content-Type）。
    pub fn bytes(status: u16, content_type: &str, data: Vec<u8>) -> Self {
        Self {
            status,
            headers: vec![("Content-Type".to_string(), content_type.to_string())],
            body: data,
        }
    }

    /// 空响应（无体、无 Content-Type；如 101/204）。
    pub fn empty(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
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
            .body(Full::new(Bytes::from(self.body)))
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
}

impl Default for HttpServerOptions {
    fn default() -> Self {
        Self {
            max_body_size: DEFAULT_MAX_BODY_SIZE,
            ws: WsServerOptions::default(),
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
pub struct HttpServer {
    listener: TcpListener,
}

impl HttpServer {
    /// 绑定监听地址（`127.0.0.1:0` 随机端口便于测试）。
    pub async fn bind(addr: impl tokio::net::ToSocketAddrs) -> std::io::Result<HttpServer> {
        let listener = TcpListener::bind(addr).await?;
        Ok(HttpServer { listener })
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
        loop {
            let (tcp, _peer) = self.listener.accept().await?;
            let _ = tcp.set_nodelay(true);
            let handler = handler.clone();
            let options = options.clone();
            tokio::spawn(serve_connection(tcp, handler, options));
        }
    }
}

/// 单连接服务（hyper HTTP/1.1 + 升级支持）。
async fn serve_connection(tcp: TcpStream, handler: HttpHandler, options: HttpServerOptions) {
    let service = service_fn(move |req: Request<Incoming>| {
        let handler = handler.clone();
        let options = options.clone();
        async move { Ok::<_, Infallible>(handle_http(req, handler, options).await) }
    });
    // 连接错误（含正常关闭）静默；升级路径由 with_upgrades 支撑
    let _ = http1::Builder::new()
        .serve_connection(TokioIo::new(tcp), service)
        .with_upgrades()
        .await;
}

/// 处理单个请求：收集请求 → 处理器 → 普通响应或 WS 升级。
async fn handle_http(
    mut req: Request<Incoming>,
    handler: HttpHandler,
    options: HttpServerOptions,
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
            tokio::spawn(async move {
                match on_upgrade.await {
                    Ok(upgraded) => {
                        // TokioIo<Upgraded>：hyper 升级流 → tokio 读写（保留 hyper 读缓冲）
                        let io = TokioIo::new(upgraded);
                        ws::run_server_session(io, hooks, options.ws).await;
                    }
                    Err(_e) => {
                        // 升级失败（对端断开/协议错误）：连接已不可用，静默收场
                    }
                }
            });

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

/// 收集请求体（带上限；超限即中断返回 `Err`）。
async fn collect_body(body: Incoming, limit: usize) -> Result<Vec<u8>, ()> {
    let mut collected: Vec<u8> = Vec::new();
    let mut body = body;
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| ())?;
        if let Ok(data) = frame.into_data() {
            if collected.len() + data.len() > limit {
                return Err(());
            }
            collected.extend_from_slice(&data);
        }
    }
    Ok(collected)
}
