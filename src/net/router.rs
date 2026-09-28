//! net::router —— HTTP 语义层：路由与中间件（自研，对齐 ASP.NET Core 极简子集 + DH.NCore `HttpRouter`）。
//!
//! - `Router::map*`：方法 + 路径模式（`{name}` 占位捕获；段匹配大小写不敏感，对齐 ASP.NET 路由）；
//! - `Router::use_middleware`：洋葱式中间件链（先注册先进入；可改写请求/响应或短路）；
//! - [`Ctx`]：请求上下文（原始请求 + 路由参数 + urlencoded 表单 + 查询串 + 中间件状态）；
//! - 统一返回：见 [`super::http::DGResult`]（StateCode 对齐 `Pek.Helpers.StateCode`）。
//!
//! 路径匹配规则：以 `/` 分段（忽略首尾空段）；`{name}` 段捕获（值经 URL 解码）；
//! 路径命中但方法不符 → 405；全不命中 → 404（可用 [`Router::fallback`] 自定义）。
//!
//! 待办（A2xx 批次，文件管理与上传控制器迁移前补齐）：`multipart/form-data` 解析。

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use super::http::{HttpHandler, HttpOutcome, HttpRequest, HttpResponse};
use super::ws::WsServerHooks;

/// boxed 异步结果。
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// 路由处理器（[`route`] 包装普通 async 闭包）。
pub type RouteHandler = Arc<dyn Fn(Ctx) -> BoxFuture<HttpOutcome> + Send + Sync>;

/// 中间件后继（调用则进入下一层；不调用即短路）。
pub type Next = Arc<dyn Fn(Ctx) -> BoxFuture<HttpOutcome> + Send + Sync>;

/// 中间件（[`middleware`] 包装普通 async 闭包；`next` 为后续链）。
pub type Middleware = Arc<dyn Fn(Ctx, Next) -> BoxFuture<HttpOutcome> + Send + Sync>;

/// 便捷构造 [`RouteHandler`]。
pub fn route<F, Fut>(f: F) -> RouteHandler
where
    F: Fn(Ctx) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = HttpOutcome> + Send + 'static,
{
    Arc::new(move |ctx| Box::pin(f(ctx)))
}

/// 便捷构造 [`Middleware`]。
pub fn middleware<F, Fut>(f: F) -> Middleware
where
    F: Fn(Ctx, Next) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = HttpOutcome> + Send + 'static,
{
    Arc::new(move |ctx, next| Box::pin(f(ctx, next)))
}

// ————— 请求上下文 —————

/// 请求上下文（路由参数 / 表单 / 查询 / 中间件状态；随链路按值传递）。
#[derive(Debug, Clone)]
pub struct Ctx {
    /// 原始请求（中间件可修改头/路径等再传递）
    pub req: HttpRequest,
    /// 路由参数（`{name}` 捕获；值已 URL 解码）
    pub params: Vec<(String, String)>,
    /// urlencoded 表单字段（保序；仅 POST/PUT/PATCH + `application/x-www-form-urlencoded`）
    pub form: Vec<(String, String)>,
    /// 查询串键值（保序；值已 URL 解码）
    pub query: Vec<(String, String)>,
    /// 中间件状态（如鉴权结果、请求 ID 等链路传递数据）
    pub state: HashMap<String, String>,
}

impl Ctx {
    /// 由原始请求构建（预解析查询串与 urlencoded 表单）。
    pub fn build(req: HttpRequest) -> Ctx {
        let query = if req.query.is_empty() {
            Vec::new()
        } else {
            parse_kv(&req.query)
        };
        let method_ok = req.method.eq_ignore_ascii_case("POST")
            || req.method.eq_ignore_ascii_case("PUT")
            || req.method.eq_ignore_ascii_case("PATCH");
        let ct_ok = req
            .header("content-type")
            .map(|v| starts_with_ignore_ascii_case(v, "application/x-www-form-urlencoded"))
            .unwrap_or(false);
        let form = if method_ok && ct_ok {
            parse_kv(&String::from_utf8_lossy(&req.body))
        } else {
            Vec::new()
        };
        Ctx {
            req,
            params: Vec::new(),
            form,
            query,
            state: HashMap::new(),
        }
    }

    /// 路由参数（`{name}` 捕获）。
    pub fn param(&self, name: &str) -> Option<&str> {
        find_value(&self.params, name)
    }

    /// 查询参数。
    pub fn query_value(&self, name: &str) -> Option<&str> {
        find_value(&self.query, name)
    }

    /// urlencoded 表单字段。
    pub fn form_value(&self, name: &str) -> Option<&str> {
        find_value(&self.form, name)
    }

    /// 请求头（大小写不敏感）。
    pub fn header(&self, name: &str) -> Option<&str> {
        self.req.header(name)
    }

    /// 中间件状态。
    pub fn state_value(&self, name: &str) -> Option<&str> {
        self.state.get(name).map(|v| v.as_str())
    }

    /// 写入中间件状态。
    pub fn set_state(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.state.insert(name.into(), value.into());
    }

    /// 表单字段 → 字符串（缺省空串；对齐 C# `[FromForm] String` 绑定习惯）。
    pub fn form_str(&self, name: &str) -> String {
        self.form_value(name).unwrap_or("").to_string()
    }

    /// 表单字段 → `Int32`（宽松解析：空白/缺失为 0，对齐 `ToInt()` 语义）。
    pub fn form_i32(&self, name: &str) -> i32 {
        self.form_value(name)
            .and_then(|v| v.trim().parse::<i32>().ok())
            .unwrap_or(0)
    }

    /// 表单字段 → `Int64`（宽松解析：空白/缺失为 0）。
    pub fn form_i64(&self, name: &str) -> i64 {
        self.form_value(name)
            .and_then(|v| v.trim().parse::<i64>().ok())
            .unwrap_or(0)
    }

    /// 表单字段 → `Boolean`（对齐 C# `Boolean.TryParse`：`true`/`false` 忽略大小写；其余 false）。
    pub fn form_bool(&self, name: &str) -> bool {
        self.form_value(name)
            .map(|v| v.trim().eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    }
}

fn find_value<'a>(list: &'a [(String, String)], name: &str) -> Option<&'a str> {
    list.iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

// ————— 参数编解码 —————

/// URL 解码（`+` 视为空格；`%XX` 非法序列保留原样；UTF-8 宽松解码）。
pub fn url_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                match (hex_value(bytes[i + 1]), hex_value(bytes[i + 2])) {
                    (Some(hi), Some(lo)) => {
                        out.push(hi * 16 + lo);
                        i += 3;
                    }
                    _ => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[inline]
fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// 解析 `k=v&k2=v2` 形式（表单/查询串通用；值 URL 解码，键也解码）。
pub fn parse_kv(text: &str) -> Vec<(String, String)> {
    text.split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((k, v)) => (url_decode(k), url_decode(v)),
            None => (url_decode(pair), String::new()),
        })
        .collect()
}

// ————— 路由 —————

/// 路径段。
enum Seg {
    /// 字面段（大小写不敏感比较）
    Literal(String),
    /// `{name}` 占位捕获
    Param(String),
}

/// 单条路由。
struct RouteEntry {
    /// 方法（`*` = 任意）
    method: String,
    segments: Vec<Seg>,
    handler: RouteHandler,
}

/// HTTP 路由器（`map` 注册 + `use_middleware` 洋葱链 → [`Router::into_handler`] 交给服务端）。
#[derive(Default)]
pub struct Router {
    middlewares: Vec<Middleware>,
    routes: Vec<RouteEntry>,
    /// 静态路由加速表（模式不含 `{param}`）：lower(path) → (method 或 "*", handler)。
    /// 注册时预建；请求时一次哈希命中，免逐路由分段匹配/临时分配（高频接口关键路径）
    static_routes: HashMap<String, Vec<(String, RouteHandler)>>,
    fallback: Option<RouteHandler>,
}

impl Router {
    /// 新建空路由器。
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册中间件（先注册先进入；调用 `next` 进入后续链，不调用即短路）。
    pub fn use_middleware(&mut self, mw: Middleware) -> &mut Self {
        self.middlewares.push(mw);
        self
    }

    /// 注册路由（`method` 大小写不敏感；`*` 匹配任意方法）。
    pub fn map(&mut self, method: &str, pattern: &str, handler: RouteHandler) -> &mut Self {
        // 无 `{param}` 的模式同步进静态加速表（键为小写路径）
        if !pattern.contains('{') {
            let key = normalize_static_path(pattern);
            self.static_routes
                .entry(key)
                .or_default()
                .push((method.to_string(), handler.clone()));
        }
        self.routes.push(RouteEntry {
            method: method.to_string(),
            segments: parse_pattern(pattern),
            handler,
        });
        self
    }

    /// 注册 GET 路由。
    pub fn map_get(&mut self, pattern: &str, handler: RouteHandler) -> &mut Self {
        self.map("GET", pattern, handler)
    }

    /// 注册 POST 路由。
    pub fn map_post(&mut self, pattern: &str, handler: RouteHandler) -> &mut Self {
        self.map("POST", pattern, handler)
    }

    /// 注册 PUT 路由。
    pub fn map_put(&mut self, pattern: &str, handler: RouteHandler) -> &mut Self {
        self.map("PUT", pattern, handler)
    }

    /// 注册 DELETE 路由。
    pub fn map_delete(&mut self, pattern: &str, handler: RouteHandler) -> &mut Self {
        self.map("DELETE", pattern, handler)
    }

    /// 注册 WebSocket 端点（GET 升级；非升级请求回 426）。
    pub fn map_ws(&mut self, pattern: &str, hooks: WsServerHooks) -> &mut Self {
        self.map_get(
            pattern,
            route(move |ctx: Ctx| {
                let hooks = hooks.clone();
                async move {
                    if ctx.req.is_websocket_upgrade() {
                        HttpOutcome::WebSocket(hooks)
                    } else {
                        HttpOutcome::Response(
                            HttpResponse::text(
                                426,
                                "需要 WebSocket 升级（Sec-WebSocket-Version: 13）",
                            )
                            .with_header("Sec-WebSocket-Version", "13"),
                        )
                    }
                }
            }),
        )
    }

    /// 自定义 404 处理器（未命中任何路由时）。
    pub fn fallback(&mut self, handler: RouteHandler) -> &mut Self {
        self.fallback = Some(handler);
        self
    }

    /// 冻结为 [`HttpHandler`]（交给 `HttpServer::serve`）。
    pub fn into_handler(self) -> HttpHandler {
        let this = Arc::new(self);
        // 无中间件快径：省一层 Box::pin + Arc 跳转（高频接口默认配置）
        if this.middlewares.is_empty() {
            return Arc::new(move |req: HttpRequest| {
                let this = this.clone();
                Box::pin(async move { this.dispatch(Ctx::build(req)).await })
            });
        }
        let mut next: Next = {
            let this = this.clone();
            Arc::new(move |ctx| {
                let this = this.clone();
                Box::pin(async move { this.dispatch(ctx).await })
            })
        };
        // 洋葱包裹：最后注册的最贴近核心
        for mw in this.middlewares.iter().rev() {
            let mw = mw.clone();
            let inner = next.clone();
            next = Arc::new(move |ctx| (mw)(ctx, inner.clone()));
        }
        Arc::new(move |req: HttpRequest| {
            let next = next.clone();
            Box::pin(async move { next(Ctx::build(req)).await })
        })
    }

    /// 核心分派：路由匹配 → handler；未命中 → 405 / 404 / fallback。
    async fn dispatch(&self, mut ctx: Ctx) -> HttpOutcome {
        let mut path_matched = false;
        // 静态加速表先命中（无 {param} 模式）：一次哈希 + 小写转换，免逐条分段匹配
        if !self.static_routes.is_empty() {
            let lower = ctx.req.path.to_ascii_lowercase();
            if let Some(entries) = self.static_routes.get(lower.as_str()) {
                for (method, handler) in entries {
                    if method == "*" || method.eq_ignore_ascii_case(&ctx.req.method) {
                        return (handler)(ctx).await;
                    }
                }
                // 路径命中、方法不符：继续参数化路由扫描（保持注册序语义），最终 405
                path_matched = true;
            }
        }
        for entry in &self.routes {
            if let Some(params) = match_pattern(&entry.segments, &ctx.req.path) {
                if entry.method == "*" || entry.method.eq_ignore_ascii_case(&ctx.req.method) {
                    ctx.params = params;
                    return (entry.handler)(ctx).await;
                }
                path_matched = true;
            }
        }
        if path_matched {
            return HttpOutcome::Response(HttpResponse::text(405, "Method Not Allowed"));
        }
        if let Some(fallback) = &self.fallback {
            return (fallback)(ctx).await;
        }
        HttpOutcome::Response(HttpResponse::text(404, "Not Found"))
    }
}

/// 编译路径模式（`api/FileManager/{action}` → 段列表）。
fn parse_pattern(pattern: &str) -> Vec<Seg> {
    pattern
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|s| {
            if s.starts_with('{') && s.ends_with('}') && s.len() > 2 {
                Seg::Param(s[1..s.len() - 1].to_string())
            } else {
                Seg::Literal(s.to_string())
            }
        })
        .collect()
}

/// 静态路由键：规范化（去首尾空白、保证前导 `/`）后整体小写（查询时路径同样小写比较）。
fn normalize_static_path(pattern: &str) -> String {
    let p = pattern.trim();
    let p = p.strip_prefix('/').unwrap_or(p);
    format!("/{p}").to_ascii_lowercase()
}

/// ASCII 前缀比较（大小写不敏感；无分配）。
fn starts_with_ignore_ascii_case(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len() && s.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

/// 路径匹配（返回捕获的路由参数；`None` = 不匹配）。
fn match_pattern(segments: &[Seg], path: &str) -> Option<Vec<(String, String)>> {
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() != segments.len() {
        return None;
    }
    let mut params = Vec::new();
    for (seg, part) in segments.iter().zip(parts.iter()) {
        match seg {
            Seg::Literal(lit) => {
                if !lit.eq_ignore_ascii_case(part) {
                    return None;
                }
            }
            Seg::Param(name) => {
                params.push((name.clone(), url_decode(part)));
            }
        }
    }
    Some(params)
}
