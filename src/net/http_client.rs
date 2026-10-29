//! 极简 HTTP/1.1 客户端（hyper 连接层 + 自研请求/响应封装）。
//!
//! 用途：Agent 间 HTTP 调用（跨节点文件拉取 `Fetch*Transfer*`、部署包中转下载校验等），
//! 对齐 C# 侧 `HttpClient` + `ServerCertificateCustomValidationCallback`（忽略自签证书）能力。
//!
//! - [`get`] / [`post_form`] / [`request`]：全量响应（小响应、JSON 错误体解析；
//!   `request` 支持任意方法/自定义请求头与 Content-Type/原始请求体，供配置驱动转发场景）
//! - [`download_to_file`]：流式落盘（大文件/大包，不驻留内存；自动跟随 3xx 重定向）
//! - [`download_to_file_with_progress`] / [`blocking_download_to_file`]：带进度回调的流式落盘
//!   （模型权重等大文件下载场景；回调返回 false 中止）
//! - `insecure_tls`：忽略服务器证书校验（服务器间自签证书部署场景必需）
//!
//! 依赖：`http-client` 特性（hyper client + tokio；`net` 特性自动包含）；`https` 需 `http-tls` 特性（rustls；`net-tls` 已包含）。

use std::path::Path;
use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::{Method, Request};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;

/// 客户端错误（消息文本；调用方直接展示或拼接）。
#[derive(Debug)]
pub struct HttpClientError(pub String);

impl HttpClientError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl std::fmt::Display for HttpClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for HttpClientError {}

/// 客户端选项。
#[derive(Debug, Clone)]
pub struct HttpClientOptions {
    /// 整体超时（连接/发送/读体全流程；默认 30 秒）。
    pub timeout: Duration,
    /// 忽略服务器证书校验（对齐 C# `ServerCertificateCustomValidationCallback => true`）。
    pub insecure_tls: bool,
    /// 自定义根证书（PEM 内容，可含多张）——**追加**到系统/内置根之后（自签/内网 CA 的
    /// https 服务端场景，无需 `insecure_tls` 全跳过校验）；`insecure_tls` 为 true 时忽略本项。
    pub ca_pem: Option<Vec<u8>>,
}

impl Default for HttpClientOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            insecure_tls: false,
            ca_pem: None,
        }
    }
}

/// 同步请求重试策略（传输错误 + 指定 HTTP 状态码）。
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// 总尝试次数（含首次；`0` 按 1 处理）。
    pub attempts: u32,
    /// 两次尝试之间的等待（0 = 不等待）。
    pub delay: Duration,
    /// 需要重试的 HTTP 状态码；最后一次尝试仍命中时返回该响应（不报错）。
    pub retry_status: Vec<u16>,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            attempts: 3,
            delay: Duration::from_millis(200),
            retry_status: vec![408, 425, 429, 500, 502, 503, 504],
        }
    }
}

impl RetryPolicy {
    /// 指定总尝试次数（默认 200ms 间隔 + 常见 5xx/429 状态码）。
    pub fn new(attempts: u32) -> Self {
        Self {
            attempts: attempts.max(1),
            ..Self::default()
        }
    }

    /// 设置重试间隔。
    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    /// 设置需要重试的状态码。
    pub fn with_statuses(mut self, statuses: impl IntoIterator<Item = u16>) -> Self {
        self.retry_status = statuses.into_iter().collect();
        self
    }
}

/// 全量响应。
#[derive(Debug, Clone)]
pub struct HttpResponse {
    /// HTTP 状态码。
    pub status: u16,
    /// 响应头（原样保留，名称按服务端发出的大小写）。
    pub headers: Vec<(String, String)>,
    /// 响应体（全量）。
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// 按名称查响应头（忽略大小写）。
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// 是否 2xx。
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// 响应体转文本（UTF-8 有损）。
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }
}

/// 流式落盘结果（非 2xx / 3xx 时不写目标文件，错误体回读供调用方判断）。
#[derive(Debug)]
pub struct FileDownloadResult {
    /// HTTP 状态码。
    pub status: u16,
    /// 写入文件字节数（非 2xx 时为 0）。
    pub len: u64,
    /// 错误响应体（仅非 2xx 时有值；上限 64KB）。
    pub error_body: Vec<u8>,
}

type BoxStream = Box<dyn AsyncStream>;

/// 连接流（普通 TCP 或 TLS；供 hyper TokioIo 适配）。
trait AsyncStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> AsyncStream for T {}

struct ParsedUrl {
    tls: bool,
    host: String,
    port: u16,
    /// origin-form：`/path?query`（缺省 `/`）。
    path: String,
}

fn parse_url(url: &str) -> Result<ParsedUrl, HttpClientError> {
    let url = url.trim();
    let (tls, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (false, r)
    } else {
        return Err(HttpClientError::new(format!(
            "仅支持 http/https 地址: {url}"
        )));
    };
    let (authority, path) = match rest.find('/') {
        Some(pos) => (&rest[..pos], &rest[pos..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return Err(HttpClientError::new(format!("地址缺少主机名: {url}")));
    }
    let (host, port) = match authority.rfind(':') {
        Some(pos) => {
            let port_text = &authority[pos + 1..];
            let port: u16 = port_text
                .parse()
                .map_err(|_| HttpClientError::new(format!("端口非法: {port_text}")))?;
            (&authority[..pos], port)
        }
        None => (authority, if tls { 443 } else { 80 }),
    };
    Ok(ParsedUrl {
        tls,
        host: host.to_string(),
        port,
        path: path.to_string(),
    })
}

/// application/x-www-form-urlencoded 值编码（RFC 3986：不含 `A-Za-z0-9-._~` 全部转义）。
///
/// 统一实现已迁至 [`crate::web::url_encode`]；此处保留 re-export 以兼容既有调用方。
pub use crate::web::url_encode;

/// GET 请求（全量响应）。
pub async fn get(
    url: &str,
    headers: &[(&str, &str)],
    options: &HttpClientOptions,
) -> Result<HttpResponse, HttpClientError> {
    execute(Method::GET, url, headers, None, Vec::new(), options).await
}

/// POST 表单请求（application/x-www-form-urlencoded，全量响应）。
pub async fn post_form(
    url: &str,
    form: &[(&str, String)],
    headers: &[(&str, &str)],
    options: &HttpClientOptions,
) -> Result<HttpResponse, HttpClientError> {
    let body = crate::web::form_encode(form.iter().map(|(k, v)| (*k, v.as_str())));
    execute(
        Method::POST,
        url,
        headers,
        Some("application/x-www-form-urlencoded"),
        body.into_bytes(),
        options,
    )
    .await
}

/// 通用请求：任意 HTTP 方法、自定义请求头与 Content-Type、原始请求体（全量响应）。
///
/// 供“方法/请求头/请求体模板均可配置”的转发场景使用（如 tcp-scanner-server 的 WMS 上报）。
pub async fn request(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    content_type: Option<&str>,
    body: Vec<u8>,
    options: &HttpClientOptions,
) -> Result<HttpResponse, HttpClientError> {
    execute(
        parse_method(method)?,
        url,
        headers,
        content_type,
        body,
        options,
    )
    .await
}

/// 校验 HTTP 方法是否合法（配置加载时快速失败；不发起请求）。
pub fn validate_method(method: &str) -> Result<(), HttpClientError> {
    parse_method(method).map(|_| ())
}

/// GET 请求（同步版；内部创建临时 tokio 运行时）。
///
/// 供阻塞上下文低频调用（健康检查、菜单本地控制接口等）；异步上下文请直接用 [`get`]。
pub fn blocking_get_text(url: &str, timeout: Duration) -> Result<String, HttpClientError> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| HttpClientError::new(format!("创建 tokio 运行时失败: {e}")))?;

    let options = HttpClientOptions {
        timeout,
        ..Default::default()
    };
    let rsp = rt.block_on(get(url, &[], &options))?;
    Ok(rsp.body_text())
}

/// GET 请求（同步版，全量响应；内部创建临时 tokio 运行时）。
///
/// 供阻塞上下文低频调用、且需要读取状态码/响应头的场景（站点探活、健康探测等）；
/// 异步上下文请直接用 [`get`]。
pub fn blocking_get(url: &str, timeout: Duration) -> Result<HttpResponse, HttpClientError> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| HttpClientError::new(format!("创建 tokio 运行时失败: {e}")))?;

    let options = HttpClientOptions {
        timeout,
        ..Default::default()
    };
    rt.block_on(get(url, &[], &options))
}

/// 通用请求（同步版，全量响应；内部创建临时 tokio 运行时）。
///
/// 供阻塞上下文低频调用（AI 对话、外部 API 上报等；任意方法/请求头/请求体）；
/// 异步上下文请直接用 [`request`]。注意：在已有 tokio 运行时线程内调用会因
/// “运行时内 block_on”而 panic——那种场景请用 [`blocking_request_offthread`]。
pub fn blocking_request(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    content_type: Option<&str>,
    body: Vec<u8>,
    timeout: Duration,
) -> Result<HttpResponse, HttpClientError> {
    blocking_request_with(
        method,
        url,
        headers,
        content_type,
        body,
        &HttpClientOptions {
            timeout,
            ..Default::default()
        },
    )
}

/// 通用请求（同步版，**自定义选项**：`timeout` / `insecure_tls` / `ca_pem` 均生效）。
///
/// `ca_pem`：自定义根证书（PEM 可多张）**追加**到系统/内置根——自签/内网 CA 的
/// https 服务端场景（无需 `insecure_tls` 全跳过校验）。
pub fn blocking_request_with(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    content_type: Option<&str>,
    body: Vec<u8>,
    options: &HttpClientOptions,
) -> Result<HttpResponse, HttpClientError> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| HttpClientError::new(format!("创建 tokio 运行时失败: {e}")))?;

    rt.block_on(request(method, url, headers, content_type, body, options))
}

/// 通用请求（同步版，**自动选择直接/offthread**）。
///
/// - 当前线程不在 tokio 运行时内：直接 [`blocking_request_with`]（少一次线程 + 运行时转发）；
/// - 当前线程在 tokio 运行时内：转 [`blocking_request_offthread_with`]，避免“运行时内 block_on”恐慌。
///
pub fn blocking_request_auto_with(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    content_type: Option<&str>,
    body: Vec<u8>,
    options: HttpClientOptions,
) -> Result<HttpResponse, HttpClientError> {
    if tokio::runtime::Handle::try_current().is_ok() {
        blocking_request_offthread_with(method, url, headers, content_type, body, options)
    } else {
        blocking_request_with(method, url, headers, content_type, body, &options)
    }
}

/// 同步请求 + 重试（传输错误与 [`RetryPolicy::retry_status`] 命中的状态码）。
///
/// - 每次尝试都使用 [`blocking_request_auto_with`]（非 tokio 上下文直接调用）；
/// - 最后一次尝试仍返回可重试状态时，返回该响应而不是错误，便于调用方解析错误体；
/// - `body` 会在每次尝试时克隆（大请求体可考虑自行包 `Arc`，当前接口保持简单）。
pub fn blocking_request_with_retry(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    content_type: Option<&str>,
    body: Vec<u8>,
    options: HttpClientOptions,
    policy: &RetryPolicy,
) -> Result<HttpResponse, HttpClientError> {
    let attempts = policy.attempts.max(1);
    let mut last_error = None;
    for attempt in 0..attempts {
        match blocking_request_auto_with(
            method,
            url,
            headers,
            content_type,
            body.clone(),
            options.clone(),
        ) {
            Ok(response) => {
                let retryable = policy.retry_status.contains(&response.status);
                if retryable && attempt + 1 < attempts {
                    if !policy.delay.is_zero() {
                        std::thread::sleep(policy.delay);
                    }
                    continue;
                }
                return Ok(response);
            }
            Err(error) => {
                last_error = Some(error);
                if attempt + 1 < attempts && !policy.delay.is_zero() {
                    std::thread::sleep(policy.delay);
                }
            }
        }
    }
    Err(last_error.unwrap_or_else(|| HttpClientError::new("请求失败（未执行）")))
}

/// 通用请求（同步版，**可在 tokio 运行时线程内安全调用**）：
/// 在独立线程执行 [`blocking_request`] 后 join。
///
/// 背景：多个消费方（Pek.RAgent 插件源/控制探测等）曾各自实现
/// “spawn 线程 + join” 包装；本函数把这层收敛到库内（历史三次踩坑沉淀）。
pub fn blocking_request_offthread(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    content_type: Option<&str>,
    body: Vec<u8>,
    timeout: Duration,
) -> Result<HttpResponse, HttpClientError> {
    let (method, url, headers, content_type) = own_request_args(method, url, headers, content_type);
    offthread(move || {
        let refs: Vec<(&str, &str)> = headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        blocking_request(&method, &url, &refs, content_type.as_deref(), body, timeout)
    })
}

/// 通用请求（offthread 版，**自定义选项**：`timeout` / `insecure_tls` / `ca_pem` 均生效）。
pub fn blocking_request_offthread_with(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    content_type: Option<&str>,
    body: Vec<u8>,
    options: HttpClientOptions,
) -> Result<HttpResponse, HttpClientError> {
    let (method, url, headers, content_type) = own_request_args(method, url, headers, content_type);
    offthread(move || {
        let refs: Vec<(&str, &str)> = headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        blocking_request_with(
            &method,
            &url,
            &refs,
            content_type.as_deref(),
            body,
            &options,
        )
    })
}

/// 请求参数转为 `'static`（offthread 线程包装用）。
fn own_request_args(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    content_type: Option<&str>,
) -> (String, String, Vec<(String, String)>, Option<String>) {
    (
        method.to_string(),
        url.to_string(),
        headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        content_type.map(|s| s.to_string()),
    )
}

/// 在独立线程执行请求闭包后 join（offthread 家族共用）。
fn offthread<F>(f: F) -> Result<HttpResponse, HttpClientError>
where
    F: FnOnce() -> Result<HttpResponse, HttpClientError> + Send + 'static,
{
    let handle = std::thread::Builder::new()
        .name("dhrust-http-sync".to_string())
        .spawn(f)
        .map_err(|e| HttpClientError::new(format!("创建 HTTP 线程失败: {e}")))?;
    match handle.join() {
        Ok(result) => result,
        Err(_) => Err(HttpClientError::new("HTTP 线程异常退出".to_string())),
    }
}

/// 解析 HTTP 方法名（`GET`/`POST`/`PUT` 等，含自定义扩展方法）。
fn parse_method(method: &str) -> Result<Method, HttpClientError> {
    Method::from_bytes(method.trim().as_bytes())
        .map_err(|e| HttpClientError::new(format!("无效的 HTTP 方法 \"{method}\"：{e}")))
}

/// GET 请求流式落盘（2xx 时写目标文件；非 2xx 回读错误体；**自动跟随 3xx 重定向**）。
pub async fn download_to_file(
    url: &str,
    headers: &[(&str, &str)],
    options: &HttpClientOptions,
    dest: &Path,
) -> Result<FileDownloadResult, HttpClientError> {
    download_to_file_at(url, headers, options, dest, 0).await
}

/// GET 请求流式落盘（指定写入起始偏移；分片写回场景 offset>0）。
pub async fn download_to_file_at(
    url: &str,
    headers: &[(&str, &str)],
    options: &HttpClientOptions,
    dest: &Path,
    offset: u64,
) -> Result<FileDownloadResult, HttpClientError> {
    tokio::time::timeout(
        options.timeout,
        download_inner(url, headers, options, dest, offset, |_, _| true),
    )
    .await
    .map_err(|_| HttpClientError::new(format!("请求超时（{}s）", options.timeout.as_secs())))?
}

/// POST 表单请求流式落盘（源端返回 JSON / 非 2xx 时回读为错误体，不写目标文件）。
pub async fn post_form_to_file(
    url: &str,
    form: &[(&str, String)],
    headers: &[(&str, &str)],
    options: &HttpClientOptions,
    dest: &Path,
    offset: u64,
) -> Result<FileDownloadResult, HttpClientError> {
    let body = form
        .iter()
        .map(|(k, v)| format!("{}={}", url_encode(k), url_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
        .into_bytes();
    tokio::time::timeout(options.timeout, async {
        let target = parse_url(url)?;
        let (_sender, res) = send_request(
            Method::POST,
            &target,
            headers,
            Some("application/x-www-form-urlencoded"),
            body,
            options,
        )
        .await?;
        handle_response_to_file(res, dest, offset, |_, _| true).await
    })
    .await
    .map_err(|_| HttpClientError::new(format!("请求超时（{}s）", options.timeout.as_secs())))?
}

/// GET 请求流式落盘（**带进度回调**；自动跟随 3xx 重定向，上限 5 跳）。
///
/// `on_progress(bytes_written, content_length)`：已写入本目标文件的字节数
/// 与服务端 `Content-Length`（未知时 None）；返回 `false` 中止下载（错误消息“下载已取消”）。
pub async fn download_to_file_with_progress<F>(
    url: &str,
    headers: &[(&str, &str)],
    options: &HttpClientOptions,
    dest: &Path,
    on_progress: F,
) -> Result<FileDownloadResult, HttpClientError>
where
    F: FnMut(u64, Option<u64>) -> bool,
{
    tokio::time::timeout(
        options.timeout,
        download_inner(url, headers, options, dest, 0, on_progress),
    )
    .await
    .map_err(|_| HttpClientError::new(format!("请求超时（{}s）", options.timeout.as_secs())))?
}

/// GET 请求流式落盘（同步版，**带进度回调**；内部创建临时 tokio 运行时）。
///
/// 供阻塞上下文（后台线程）下载大文件（模型权重等）；异步上下文请用
/// [`download_to_file_with_progress`]。注意：在已有 tokio 运行时线程内调用会因
/// “运行时内 block_on”而 panic——那种场景请改在独立线程中调用本函数。
pub fn blocking_download_to_file<F>(
    url: &str,
    headers: &[(&str, &str)],
    options: &HttpClientOptions,
    dest: &Path,
    on_progress: F,
) -> Result<FileDownloadResult, HttpClientError>
where
    F: FnMut(u64, Option<u64>) -> bool,
{
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| HttpClientError::new(format!("创建 tokio 运行时失败: {e}")))?;

    rt.block_on(download_to_file_with_progress(
        url,
        headers,
        options,
        dest,
        on_progress,
    ))
}

/// 重定向最大跳数（防循环）。
const DOWNLOAD_MAX_REDIRECTS: usize = 5;

/// 解析重定向目标（绝对地址原样；以 `/` 开头按当前源拼接；其它相对路径按当前目录拼接）。
fn resolve_redirect(base_url: &str, location: &str) -> Result<String, HttpClientError> {
    let loc = location.trim();
    if loc.starts_with("http://") || loc.starts_with("https://") {
        return Ok(loc.to_string());
    }
    let base = parse_url(base_url)?;
    let scheme = if base.tls { "https" } else { "http" };
    let authority = if (base.tls && base.port == 443) || (!base.tls && base.port == 80) {
        base.host.clone()
    } else {
        format!("{}:{}", base.host, base.port)
    };
    if loc.starts_with('/') {
        return Ok(format!("{scheme}://{authority}{loc}"));
    }
    let dir = match base.path.rfind('/') {
        Some(pos) => &base.path[..=pos],
        None => "/",
    };
    Ok(format!("{scheme}://{authority}{dir}{loc}"))
}

async fn download_inner<F>(
    url: &str,
    headers: &[(&str, &str)],
    options: &HttpClientOptions,
    dest: &Path,
    offset: u64,
    mut on_progress: F,
) -> Result<FileDownloadResult, HttpClientError>
where
    F: FnMut(u64, Option<u64>) -> bool,
{
    let mut current = url.to_string();
    for _ in 0..=DOWNLOAD_MAX_REDIRECTS {
        let target = parse_url(&current)?;
        let (_sender, res) =
            send_request(Method::GET, &target, headers, None, Vec::new(), options).await?;
        let status = res.status().as_u16();
        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            let loc = res
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
                .ok_or_else(|| HttpClientError::new("重定向响应缺少 Location 头".to_string()))?;
            current = resolve_redirect(&current, &loc)?;
            continue;
        }
        return handle_response_to_file(res, dest, offset, &mut on_progress).await;
    }
    Err(HttpClientError::new(format!(
        "重定向次数过多（>{DOWNLOAD_MAX_REDIRECTS} 跳）: {url}"
    )))
}

/// 统一处理"响应 → 落盘或错误体回读"：
/// - 非 2xx，或 Content-Type 含 json（上游错误以 200+JSON 返回）→ 聚合 body 作错误体，不写文件；
/// - 否则流式写入目标文件（offset 定位；offset=0 新建/截断；offset>0 打开或创建并定位）。
async fn handle_response_to_file<F>(
    res: hyper::Response<hyper::body::Incoming>,
    dest: &Path,
    offset: u64,
    mut on_progress: F,
) -> Result<FileDownloadResult, HttpClientError>
where
    F: FnMut(u64, Option<u64>) -> bool,
{
    let status = res.status().as_u16();
    let content_length: Option<u64> = res
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse().ok());
    let content_type = res
        .headers()
        .get("content-type")
        .map(|v| String::from_utf8_lossy(v.as_bytes()).to_string())
        .unwrap_or_default();
    let as_error =
        !(200..300).contains(&status) || content_type.to_ascii_lowercase().contains("json");
    if as_error {
        let body = res
            .into_body()
            .collect()
            .await
            .map_err(|e| HttpClientError::new(format!("读取错误响应失败: {e}")))?;
        let bytes = body.to_bytes();
        let cut = bytes.len().min(64 * 1024);
        return Ok(FileDownloadResult {
            status,
            len: 0,
            error_body: bytes[..cut].to_vec(),
        });
    }

    let mut body = res.into_body();
    let mut file = if offset == 0 {
        tokio::fs::File::create(dest)
            .await
            .map_err(|e| HttpClientError::new(format!("创建目标文件失败: {e}")))?
    } else {
        let mut f = tokio::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(dest)
            .await
            .map_err(|e| HttpClientError::new(format!("打开目标文件失败: {e}")))?;
        tokio::io::AsyncSeekExt::seek(&mut f, std::io::SeekFrom::Start(offset))
            .await
            .map_err(|e| HttpClientError::new(format!("定位写入偏移失败: {e}")))?;
        f
    };
    let mut len: u64 = 0;
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|e| HttpClientError::new(format!("读取响应体失败: {e}")))?;
        if let Some(data) = frame.data_ref() {
            tokio::io::AsyncWriteExt::write_all(&mut file, data)
                .await
                .map_err(|e| HttpClientError::new(format!("写入目标文件失败: {e}")))?;
            len += data.len() as u64;
            if !on_progress(len, content_length) {
                return Err(HttpClientError::new("下载已取消".to_string()));
            }
        }
    }
    tokio::io::AsyncWriteExt::flush(&mut file)
        .await
        .map_err(|e| HttpClientError::new(format!("刷新目标文件失败: {e}")))?;
    Ok(FileDownloadResult {
        status,
        len,
        error_body: Vec::new(),
    })
}

async fn execute(
    method: Method,
    url: &str,
    headers: &[(&str, &str)],
    content_type: Option<&str>,
    body: Vec<u8>,
    options: &HttpClientOptions,
) -> Result<HttpResponse, HttpClientError> {
    tokio::time::timeout(
        options.timeout,
        execute_inner(method, url, headers, content_type, body, options),
    )
    .await
    .map_err(|_| HttpClientError::new(format!("请求超时（{}s）", options.timeout.as_secs())))?
}

async fn execute_inner(
    method: Method,
    url: &str,
    headers: &[(&str, &str)],
    content_type: Option<&str>,
    body: Vec<u8>,
    options: &HttpClientOptions,
) -> Result<HttpResponse, HttpClientError> {
    let target = parse_url(url)?;
    let (_sender, res) =
        send_request(method, &target, headers, content_type, body, options).await?;
    let status = res.status().as_u16();
    let resp_headers: Vec<(String, String)> = res
        .headers()
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                String::from_utf8_lossy(v.as_bytes()).to_string(),
            )
        })
        .collect();
    let body = res
        .into_body()
        .collect()
        .await
        .map_err(|e| HttpClientError::new(format!("读取响应体失败: {e}")))?
        .to_bytes()
        .to_vec();
    Ok(HttpResponse {
        status,
        headers: resp_headers,
        body,
    })
}

async fn send_request(
    method: Method,
    target: &ParsedUrl,
    headers: &[(&str, &str)],
    content_type: Option<&str>,
    body: Vec<u8>,
    options: &HttpClientOptions,
) -> Result<
    (
        hyper::client::conn::http1::SendRequest<Full<Bytes>>,
        hyper::Response<hyper::body::Incoming>,
    ),
    HttpClientError,
> {
    #[cfg(not(feature = "http-tls"))]
    let _ = &options; // insecure_tls 仅 TLS 分支使用（http-only 编译时避免未使用告警）

    let tcp = TcpStream::connect((target.host.as_str(), target.port))
        .await
        .map_err(|e| {
            HttpClientError::new(format!("连接 {}:{} 失败: {e}", target.host, target.port))
        })?;
    let _ = tcp.set_nodelay(true);

    #[cfg(feature = "http-tls")]
    let stream: BoxStream = if target.tls {
        wrap_tls(&target.host, tcp, options).await?
    } else {
        Box::new(tcp)
    };
    #[cfg(not(feature = "http-tls"))]
    let stream: BoxStream = {
        if target.tls {
            return Err(HttpClientError::new("https 需要启用 http-tls 特性"));
        }
        Box::new(tcp)
    };

    let (sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|e| HttpClientError::new(format!("HTTP 握手失败: {e}")))?;
    tokio::spawn(async move {
        let _ = conn.await;
    });

    let host_header = if (target.tls && target.port == 443) || (!target.tls && target.port == 80) {
        target.host.clone()
    } else {
        format!("{}:{}", target.host, target.port)
    };
    let mut builder = Request::builder()
        .method(method)
        .uri(&target.path)
        .header("Host", host_header);
    if let Some(ct) = content_type {
        builder = builder.header("Content-Type", ct);
    }
    if !body.is_empty() {
        builder = builder.header("Content-Length", body.len().to_string());
    }
    for (k, v) in headers {
        builder = builder.header(*k, *v);
    }
    let request = builder
        .body(Full::new(Bytes::from(body)))
        .map_err(|e| HttpClientError::new(format!("构造请求失败: {e}")))?;

    let mut sender = sender;
    let response = sender
        .send_request(request)
        .await
        .map_err(|e| HttpClientError::new(format!("发送请求失败: {e}")))?;
    Ok((sender, response))
}

#[cfg(feature = "http-tls")]
async fn wrap_tls(
    host: &str,
    tcp: TcpStream,
    options: &HttpClientOptions,
) -> Result<BoxStream, HttpClientError> {
    use std::sync::Arc;

    use tokio_rustls::rustls::pki_types::ServerName;
    use tokio_rustls::rustls::{ClientConfig, RootCertStore};

    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    let config = if options.insecure_tls {
        // 忽略证书校验：对齐 C# `ServerCertificateCustomValidationCallback => true`（服务器间自签场景）
        ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .map_err(|e| HttpClientError::new(format!("TLS 配置失败: {e}")))?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoCertificateVerification(provider)))
            .with_no_client_auth()
    } else {
        let mut roots = RootCertStore::empty();
        let native_certs = rustls_native_certs::load_native_certs();
        for cert in native_certs.certs {
            let _ = roots.add(cert);
        }
        if roots.is_empty() {
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        }
        // 自定义根证书（PEM 可多张）追加信任：自签/内网 CA 的 https 服务端
        if let Some(pem) = &options.ca_pem {
            let added = add_pem_roots(&mut roots, pem)
                .map_err(|e| HttpClientError::new(format!("根证书 PEM 解析失败: {e}")))?;
            if added == 0 {
                return Err(HttpClientError::new("根证书 PEM 未包含证书块".to_string()));
            }
        }
        ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| HttpClientError::new(format!("TLS 配置失败: {e}")))?
            .with_root_certificates(roots)
            .with_no_client_auth()
    };

    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let server_name = ServerName::try_from(host.to_string())
        .map_err(|e| HttpClientError::new(format!("TLS 服务器名非法: {e}")))?;
    let stream = connector
        .connect(server_name, tcp)
        .await
        .map_err(|e| HttpClientError::new(format!("TLS 握手失败: {e}")))?;
    Ok(Box::new(stream))
}

/// 解析 PEM 证书块并追加到根存储（返回解析成功的张数）。
#[cfg(feature = "http-tls")]
fn add_pem_roots(
    roots: &mut tokio_rustls::rustls::RootCertStore,
    pem: &[u8],
) -> Result<usize, String> {
    let mut count = 0;
    for item in rustls_pemfile::certs(&mut &pem[..]) {
        let cert = item.map_err(|e| format!("读取证书: {e}"))?;
        roots.add(cert).map_err(|e| format!("证书无效: {e}"))?;
        count += 1;
    }
    Ok(count)
}

/// 忽略服务器证书校验的验证器（`insecure_tls` 专用；签名校验仍走 provider 算法）。
#[cfg(feature = "http-tls")]
#[derive(Debug)]
struct NoCertificateVerification(std::sync::Arc<tokio_rustls::rustls::crypto::CryptoProvider>);

#[cfg(feature = "http-tls")]
impl tokio_rustls::rustls::client::danger::ServerCertVerifier for NoCertificateVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[tokio_rustls::rustls::pki_types::CertificateDer<'_>],
        _server_name: &tokio_rustls::rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: tokio_rustls::rustls::pki_types::UnixTime,
    ) -> Result<tokio_rustls::rustls::client::danger::ServerCertVerified, tokio_rustls::rustls::Error>
    {
        Ok(tokio_rustls::rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
        dss: &tokio_rustls::rustls::DigitallySignedStruct,
    ) -> Result<
        tokio_rustls::rustls::client::danger::HandshakeSignatureValid,
        tokio_rustls::rustls::Error,
    > {
        tokio_rustls::rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
        dss: &tokio_rustls::rustls::DigitallySignedStruct,
    ) -> Result<
        tokio_rustls::rustls::client::danger::HandshakeSignatureValid,
        tokio_rustls::rustls::Error,
    > {
        tokio_rustls::rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<tokio_rustls::rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

// ————— 单元测试（纯函数 + 本地 mock 服务端）————

#[cfg(test)]
mod tests {
    use super::*;

    /// 阻塞入口的 runtime 创建代价（`cargo test --release --features net --lib -- --ignored --nocapture runtime_create`）。
    ///
    /// 现状：每个 `get_blocking` / `post_blocking` / `download_to_file` / `wecom` 阻塞入口都新建一个
    /// current-thread runtime。对「每 N 秒一次」的调用无所谓；高频（逐设备推送、逐消息 webhook）时
    /// 每次几十微秒的创建/销毁就是可观开销。本基准用来量化该开销，供是否改为共享 runtime 决策。
    #[test]
    #[ignore = "性能基准，按需手动运行"]
    fn perf_blocking_runtime_creation() {
        const N: usize = 200;

        let t = std::time::Instant::now();
        for _ in 0..N {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            drop(rt);
        }
        let current_thread = t.elapsed();

        let t = std::time::Instant::now();
        for _ in 0..N {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .unwrap();
            drop(rt);
        }
        let multi_thread = t.elapsed();

        let to_us = |d: std::time::Duration| d.as_secs_f64() * 1e6 / N as f64;
        println!(
            "[runtime 创建/销毁 ×{N}] current_thread {current_thread:?}（{:.1}µs/次）/ multi_thread(1) {multi_thread:?}（{:.1}µs/次）",
            to_us(current_thread),
            to_us(multi_thread)
        );
    }

    #[test]
    fn offthread_request_is_safe_inside_tokio_runtime() {
        use std::io::Write as _;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        // 单次响应后线程自然结束（不 join：请求失败时避免测试卡死）
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                use std::io::Read as _;
                let _ = s.read(&mut buf);
                let _ = s.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                );
            }
        });

        // 关键：在 tokio 运行时线程内调用（直接 blocking_request 会 panic）
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let resp = rt
            .block_on(async {
                blocking_request_offthread(
                    "GET",
                    &format!("http://{addr}/"),
                    &[],
                    None,
                    Vec::new(),
                    Duration::from_secs(5),
                )
            })
            .expect("运行时线程内 offthread 请求应成功");
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, b"ok");
    }

    #[test]
    fn blocking_get_text_roundtrip() {
        use std::io::{Read as _, Write as _};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf);
                let body = "hello";
                let rsp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(rsp.as_bytes());
            }
        });

        let text =
            blocking_get_text(&format!("http://{addr}/ping"), Duration::from_secs(3)).unwrap();
        assert_eq!(text, "hello");
        let _ = handle.join();

        // 不可达：返回错误而非 panic
        assert!(blocking_get_text("http://127.0.0.1:1/", Duration::from_millis(300)).is_err());
    }

    #[tokio::test]
    async fn blocking_request_auto_works_inside_runtime() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                use std::io::{Read, Write};
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK",
                );
            }
        });

        let options = HttpClientOptions {
            timeout: Duration::from_secs(3),
            ..Default::default()
        };
        let rsp = blocking_request_auto_with(
            "GET",
            &format!("http://{addr}/"),
            &[],
            None,
            Vec::new(),
            options,
        )
        .unwrap();
        assert_eq!(rsp.status, 200);
        assert_eq!(rsp.body_text(), "OK");
        let _ = handle.join();
    }

    #[test]
    fn retry_policy_retries_5xx_then_returns_success() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let server_attempts = std::sync::Arc::clone(&attempts);
        let handle = std::thread::spawn(move || {
            for _ in 0..2 {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                let mut buf = [0u8; 1024];
                use std::io::{Read, Write};
                let _ = stream.read(&mut buf);
                server_attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let response = if server_attempts.load(std::sync::atomic::Ordering::SeqCst) == 1 {
                    b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 3\r\nConnection: close\r\n\r\nbusy".to_vec()
                } else {
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK".to_vec()
                };
                let _ = stream.write_all(&response);
            }
        });

        let policy = RetryPolicy::new(2).with_delay(Duration::from_millis(1));
        let rsp = blocking_request_with_retry(
            "GET",
            &format!("http://{addr}/"),
            &[],
            None,
            Vec::new(),
            HttpClientOptions {
                timeout: Duration::from_secs(3),
                ..Default::default()
            },
            &policy,
        )
        .unwrap();
        assert_eq!(rsp.status, 200);
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
        let _ = handle.join();
    }

    #[test]
    fn parse_url_variants() {
        let u = parse_url("https://10.0.0.1:8283/api/v1/x?a=1").unwrap();
        assert!(u.tls);
        assert_eq!(u.host, "10.0.0.1");
        assert_eq!(u.port, 8283);
        assert_eq!(u.path, "/api/v1/x?a=1");

        let u = parse_url("http://agent.local").unwrap();
        assert!(!u.tls);
        assert_eq!(u.host, "agent.local");
        assert_eq!(u.port, 80);
        assert_eq!(u.path, "/");

        assert!(parse_url("ftp://x").is_err());
    }

    #[test]
    fn url_encode_basic() {
        assert_eq!(url_encode("abc-._~"), "abc-._~");
        assert_eq!(url_encode("a b"), "a%20b");
        assert_eq!(
            url_encode("根目录/文件.zip"),
            "%E6%A0%B9%E7%9B%AE%E5%BD%95%2F%E6%96%87%E4%BB%B6.zip"
        );
        assert_eq!(url_encode("x&y=z"), "x%26y%3Dz");
    }

    async fn spawn_mock_server(
        response: &'static str,
        captured: std::sync::Arc<tokio::sync::Mutex<String>>,
    ) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = vec![0u8; 65536];
                let mut req = Vec::new();
                loop {
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    req.extend_from_slice(&buf[..n]);
                    if let Some(pos) = req.windows(4).position(|w| w == b"\r\n\r\n") {
                        // 判断请求体是否读全（Content-Length）
                        let head = String::from_utf8_lossy(&req[..pos]).to_string();
                        let mut need = 0usize;
                        for line in head.lines() {
                            if let Some(v) = line.strip_prefix("Content-Length:") {
                                need = v.trim().parse().unwrap_or(0);
                            }
                        }
                        if req.len() >= pos + 4 + need {
                            break;
                        }
                    }
                    if req.len() > 1024 * 1024 {
                        break;
                    }
                }
                *captured.lock().await = String::from_utf8_lossy(&req).to_string();
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        port
    }

    #[tokio::test]
    async fn get_full_response() {
        let captured = std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));
        let port = spawn_mock_server(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 5\r\n\r\nhello",
            captured.clone(),
        )
        .await;
        let resp = get(
            &format!("http://127.0.0.1:{port}/ping?x=1"),
            &[("X-Online-Token", "t0k")],
            &HttpClientOptions::default(),
        )
        .await
        .unwrap();
        assert_eq!(resp.status, 200);
        assert!(resp.is_success());
        assert_eq!(resp.body_text(), "hello");
        assert_eq!(resp.header("content-type"), Some("text/plain"));
        let req = captured.lock().await.clone();
        assert!(req.starts_with("GET /ping?x=1 HTTP/1.1"));
        // hyper 会把请求头名规范为小写；服务端侧统一按大小写不敏感读取
        let req_lower = req.to_lowercase();
        assert!(req_lower.contains("x-online-token: t0k"));
        assert!(req_lower.contains("host: 127.0.0.1"));
    }

    #[tokio::test]
    async fn post_form_sends_urlencoded_body() {
        let captured = std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));
        let port = spawn_mock_server(
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
            captured.clone(),
        )
        .await;
        let resp = post_form(
            &format!("http://127.0.0.1:{port}/api/v1/DirectFileManager/DownloadTransferFile"),
            &[
                ("path", "C:\\data 目录".to_string()),
                ("offset", "0".to_string()),
            ],
            &[],
            &HttpClientOptions::default(),
        )
        .await
        .unwrap();
        assert_eq!(resp.status, 200);
        let req = captured.lock().await.clone();
        let (head, body) = req.split_once("\r\n\r\n").unwrap();
        assert!(head.contains("application/x-www-form-urlencoded"));
        assert!(body.contains("path=C%3A%5Cdata%20%E7%9B%AE%E5%BD%95"));
        assert!(body.contains("offset=0"));
    }

    #[tokio::test]
    async fn request_custom_method_headers_and_body() {
        let captured = std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));
        let port = spawn_mock_server(
            "HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\nok",
            captured.clone(),
        )
        .await;
        let resp = request(
            "PUT",
            &format!("http://127.0.0.1:{port}/Api/V1/Order/SetBarcode"),
            &[("X-Device", "packing-3")],
            Some("application/json"),
            br#"{"barcode":"HLT01"}"#.to_vec(),
            &HttpClientOptions::default(),
        )
        .await
        .unwrap();
        assert_eq!(resp.status, 201);
        assert!(resp.is_success());

        let req = captured.lock().await.clone();
        assert!(
            req.starts_with("PUT /Api/V1/Order/SetBarcode HTTP/1.1"),
            "请求行：{req}"
        );
        // hyper 会把请求头名规范为小写；服务端侧统一按大小写不敏感读取
        let req_lower = req.to_lowercase();
        assert!(req_lower.contains("content-type: application/json"));
        assert!(req_lower.contains("x-device: packing-3"));
        assert!(req.ends_with(r#"{"barcode":"HLT01"}"#), "请求体：{req}");
    }

    #[test]
    fn validate_method_accepts_tokens_rejects_junk() {
        assert!(validate_method("POST").is_ok());
        assert!(validate_method(" put ").is_ok());
        assert!(validate_method("PROPFIND").is_ok());
        assert!(validate_method("GE T").is_err());
        assert!(validate_method("中文").is_err());
    }

    #[tokio::test]
    async fn download_to_file_streams_body() {
        let captured = std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));
        let port = spawn_mock_server(
            "HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\nbinary-data",
            captured.clone(),
        )
        .await;
        let dir = std::env::temp_dir().join(format!("httpc-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("out.bin");
        let result = download_to_file(
            &format!("http://127.0.0.1:{port}/download"),
            &[],
            &HttpClientOptions::default(),
            &dest,
        )
        .await
        .unwrap();
        assert_eq!(result.status, 200);
        assert_eq!(result.len, 11);
        assert_eq!(std::fs::read(&dest).unwrap(), b"binary-data");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn download_to_file_error_body() {
        let captured = std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));
        let port = spawn_mock_server(
            "HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\nnot found",
            captured.clone(),
        )
        .await;
        let dir = std::env::temp_dir().join(format!("httpc-test2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("out.bin");
        let result = download_to_file(
            &format!("http://127.0.0.1:{port}/download"),
            &[],
            &HttpClientOptions::default(),
            &dest,
        )
        .await
        .unwrap();
        assert_eq!(result.status, 404);
        assert_eq!(result.len, 0);
        assert_eq!(result.error_body, b"not found");
        assert!(!dest.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn download_to_file_at_offset_merges() {
        let captured = std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));
        let port = spawn_mock_server(
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nworld",
            captured.clone(),
        )
        .await;
        let dir = std::env::temp_dir().join(format!("httpc-off-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("m.bin");
        std::fs::write(&dest, b"hello-").unwrap();
        let result = download_to_file_at(
            &format!("http://127.0.0.1:{port}/part"),
            &[],
            &HttpClientOptions::default(),
            &dest,
            6,
        )
        .await
        .unwrap();
        assert_eq!(result.status, 200);
        assert_eq!(result.len, 5);
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello-world");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn download_follows_redirect_and_reports_progress() {
        use std::io::{Read as _, Write as _};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            // 第一跳：302 → 真实文件地址
            if let Ok((mut s, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf);
                let rsp = format!(
                    "HTTP/1.1 302 Found\r\nLocation: http://{addr}/real.bin\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _ = s.write_all(rsp.as_bytes());
            }
            // 第二跳：真实内容
            if let Ok((mut s, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf);
                let body: Vec<u8> = (0..65536u32).map(|i| (i % 251) as u8).collect();
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = s.write_all(head.as_bytes());
                let _ = s.write_all(&body);
            }
        });

        let dir = std::env::temp_dir().join(format!("httpc-redir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("r.bin");
        let mut pulls = 0usize;
        let mut last = 0u64;
        let mut total = None;
        let result = download_to_file_with_progress(
            &format!("http://{addr}/file.bin"),
            &[],
            &HttpClientOptions::default(),
            &dest,
            |n, t| {
                pulls += 1;
                last = n;
                total = t;
                true
            },
        )
        .await
        .unwrap();
        assert_eq!(result.status, 200);
        assert_eq!(result.len, 65536);
        assert!(pulls >= 1, "进度回调应被调用");
        assert_eq!(last, 65536);
        assert_eq!(total, Some(65536));
        assert_eq!(std::fs::read(&dest).unwrap().len(), 65536);
        let _ = handle.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn post_form_to_file_writes_binary() {
        let captured = std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));
        let port = spawn_mock_server(
            "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: 4\r\n\r\nDATA",
            captured.clone(),
        )
        .await;
        let dir = std::env::temp_dir().join(format!("httpc-post-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("f.bin");
        let result = post_form_to_file(
            &format!("http://127.0.0.1:{port}/api/v1/DirectFileManager/DownloadTransferFile"),
            &[("fileRelativePath", "a/b.txt".to_string())],
            &[("X-Online-Token", "tok")],
            &HttpClientOptions::default(),
            &dest,
            0,
        )
        .await
        .unwrap();
        assert_eq!(result.status, 200);
        assert_eq!(result.len, 4);
        assert_eq!(std::fs::read(&dest).unwrap(), b"DATA");
        let req = captured.lock().await.clone();
        assert!(req.starts_with("POST "));
        assert!(req.contains("a%2Fb.txt"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn post_form_to_file_json_treated_as_error() {
        let captured = std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));
        let port = spawn_mock_server(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: 17\r\n\r\n{\"Success\":false}",
            captured.clone(),
        )
        .await;
        let dir = std::env::temp_dir().join(format!("httpc-postj-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("f.bin");
        let result = post_form_to_file(
            &format!("http://127.0.0.1:{port}/api/v1/DirectFileManager/DownloadTransferFile"),
            &[],
            &[],
            &HttpClientOptions::default(),
            &dest,
            0,
        )
        .await
        .unwrap();
        assert_eq!(result.status, 200);
        assert_eq!(result.len, 0);
        assert_eq!(result.error_body, b"{\"Success\":false}");
        assert!(!dest.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
