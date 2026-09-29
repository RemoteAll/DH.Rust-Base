#![cfg(feature = "net")]
//! N004 验收：HTTP 语义层（路由/中间件/参数绑定/统一返回）。
//!
//! 覆盖：路由分发与 `{param}` 捕获（大小写不敏感）、405/404 语义、自定义 fallback、
//! urlencoded 表单/查询/请求头绑定、中间件洋葱顺序与短路、DGResult 序列化形态
//! （含 .NET 风格转义）、路由级 WebSocket 端点端到端。

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dhrust::net::http::{json_escape, state, DGResult, HttpOutcome, HttpResponse, HttpServer};
use dhrust::net::router::{middleware, route, Ctx, Next, Router};
use dhrust::net::ws::{WsClient, WsClientOptions, WsHooks, WsServerHooks};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

// ————— 工具 —————

async fn start(router: Router) -> SocketAddr {
    let server = HttpServer::bind("127.0.0.1:0").await.unwrap();
    let addr = server.local_addr().unwrap();
    let svc = router.into_handler();
    tokio::spawn(async move {
        let _ = server.serve(svc).await;
    });
    addr
}

/// 裸 TCP 发 HTTP 请求（`Connection: close`）→（状态码, 响应头文本, 响应体文本）。
async fn http_call(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> (u16, String, String) {
    let mut tcp = TcpStream::connect(addr).await.unwrap();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    tcp.write_all(req.as_bytes()).await.unwrap();
    tcp.write_all(body).await.unwrap();

    let mut buf: Vec<u8> = Vec::new();
    tcp.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    match text.split_once("\r\n\r\n") {
        Some((head, body)) => (status, head.to_string(), body.to_string()),
        None => (status, text, String::new()),
    }
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

// ————— 用例 —————

/// 路由分发：`{param}` 捕获、大小写不敏感、405（路径命中方法不符）、404（无命中）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn route_dispatch_with_params() {
    let mut r = Router::new();
    r.map_get(
        "/api/Item/{id}",
        route(|ctx: Ctx| async move {
            HttpOutcome::Response(HttpResponse::text(
                200,
                format!("id={}", ctx.param("id").unwrap_or("")),
            ))
        }),
    );
    r.map_post(
        "/api/FileManager/List",
        route(|_ctx: Ctx| async move { HttpOutcome::Response(HttpResponse::text(200, "posted")) }),
    );
    let addr = start(r).await;

    let (s, _h, b) = http_call(addr, "GET", "/api/Item/42", &[], b"").await;
    assert_eq!((s, b.as_str()), (200, "id=42"));

    // 大小写不敏感（对齐 ASP.NET 路由）
    let (s, _h, b) = http_call(addr, "get", "/API/item/42", &[], b"").await;
    assert_eq!((s, b.as_str()), (200, "id=42"));

    // URL 编码参数值
    let (s, _h, b) = http_call(addr, "GET", "/api/Item/%E4%B8%AD%E6%96%87", &[], b"").await;
    assert_eq!((s, b.as_str()), (200, "id=中文"));

    // 路径命中但方法不符 → 405
    let (s, _h, _b) = http_call(addr, "GET", "/api/FileManager/List", &[], b"").await;
    assert_eq!(s, 405);

    // 全不命中 → 404
    let (s, _h, _b) = http_call(addr, "GET", "/nope", &[], b"").await;
    assert_eq!(s, 404);
}

/// 自定义 fallback。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn custom_fallback() {
    let mut r = Router::new();
    r.fallback(route(|ctx: Ctx| async move {
        HttpOutcome::Response(HttpResponse::text(
            200,
            format!("fallback:{}", ctx.req.path),
        ))
    }));
    let addr = start(r).await;
    let (s, _h, b) = http_call(addr, "GET", "/whatever", &[], b"").await;
    assert_eq!((s, b.as_str()), (200, "fallback:/whatever"));
}

/// 参数绑定：urlencoded 表单（中文/数值/布尔）、查询串、请求头。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn form_query_header_binding() {
    let mut r = Router::new();
    r.map_post(
        "/api/Echo",
        route(|ctx: Ctx| async move {
            let text = format!(
                "name={}; count={}; flag={}; q={}; token={}",
                ctx.form_str("name"),
                ctx.form_i32("count"),
                ctx.form_bool("flag"),
                ctx.query_value("q").unwrap_or(""),
                ctx.header("x-token").unwrap_or(""),
            );
            HttpOutcome::Response(HttpResponse::text(200, text))
        }),
    );
    let addr = start(r).await;

    let form = "name=%E4%B8%AD%E6%96%87&count=7&flag=true";
    let (s, _h, b) = http_call(
        addr,
        "POST",
        "/api/Echo?q=hello+world",
        &[
            ("Content-Type", "application/x-www-form-urlencoded"),
            ("X-Token", "abc"),
        ],
        form.as_bytes(),
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(b, "name=中文; count=7; flag=true; q=hello world; token=abc");
}

/// 中间件：洋葱顺序（先注册先进入）、状态传递、响应改写、短路。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn middleware_order_and_short_circuit() {
    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let mut r = Router::new();

    // A：进入/离开记录 + 响应加头 + 写入状态
    let log_a = log.clone();
    r.use_middleware(middleware(move |mut ctx: Ctx, next: Next| {
        log_a.lock().unwrap().push("A-in".into());
        let log_leave = log_a.clone();
        async move {
            ctx.set_state("a", "1");
            let mut out = next(ctx).await;
            if let HttpOutcome::Response(resp) = &mut out {
                resp.headers.push(("X-MW-A".into(), "1".into()));
            }
            log_leave.lock().unwrap().push("A-out".into());
            out
        }
    }));

    // B：/admin 前缀短路 401
    r.use_middleware(middleware(|ctx: Ctx, next: Next| async move {
        if ctx.req.path.starts_with("/admin") {
            return HttpOutcome::Response(HttpResponse::text(401, "unauthorized"));
        }
        next(ctx).await
    }));

    r.map_get(
        "/ping",
        route(|ctx: Ctx| async move {
            HttpOutcome::Response(HttpResponse::text(
                200,
                format!("state={}", ctx.state_value("a").unwrap_or("")),
            ))
        }),
    );
    r.map_get(
        "/admin/x",
        route(|_ctx: Ctx| async move {
            HttpOutcome::Response(HttpResponse::text(200, "should not reach"))
        }),
    );
    let addr = start(r).await;

    // 正常链路：状态传递 + 响应头改写 + 顺序
    let (s, head, b) = http_call(addr, "GET", "/ping", &[], b"").await;
    assert_eq!((s, b.as_str()), (200, "state=1"));
    assert!(
        head.contains("x-mw-a: 1") || head.contains("X-MW-A: 1"),
        "响应头未改写: {head}"
    );
    assert_eq!(
        *log.lock().unwrap(),
        vec!["A-in".to_string(), "A-out".to_string()]
    );

    // 短路：不进核心
    let (s, _h, b) = http_call(addr, "GET", "/admin/x", &[], b"").await;
    assert_eq!((s, b.as_str()), (401, "unauthorized"));
}

/// DGResult 序列化形态（字段名 PascalCase——对齐 Pek 系 C# 客户端按 Code/Message 解析；
/// 字段顺序 / null 输出 / .NET 风格转义）。
#[test]
fn dgresult_json_shapes() {
    let ok = DGResult::ok(serde_json::json!(["db1", "db2"]));
    let j = ok.to_json();
    assert!(
        j.starts_with("{\"Code\":1,\"ErrCode\":0,\"Message\":null,\"Data\":[\"db1\",\"db2\"],\"ExtData\":null,\"OperationTime\":\""),
        "字段顺序/形态不符: {j}"
    );
    assert!(j.ends_with("\",\"Id\":null}"), "结尾不符: {j}");
    // operationTime：ISO 8601 本地时间（含 T 与偏移）
    let start = j.find("\"OperationTime\":\"").unwrap() + "\"OperationTime\":\"".len();
    let end = j[start..].find('"').unwrap() + start;
    let time = &j[start..end];
    assert!(
        time.contains('T') && time.len() >= 19,
        "时间格式不符: {time}"
    );

    let fail = DGResult::fail("中文错误");
    let j = fail.to_json();
    assert!(j.contains("\"Code\":2"), "fail code 应为 2: {j}");
    assert!(
        j.contains("\"Message\":\"\\u4E2D\\u6587\\u9519\\u8BEF\""),
        "中文应转义: {j}"
    );

    // 状态码常量对齐 Pek.Helpers.StateCode
    assert_eq!(state::OK, 1);
    assert_eq!(state::FAIL, 2);
    assert_eq!(state::PARAMETER_ERROR, 400);
    assert_eq!(state::ERROR, 500);
    assert_eq!(state::PARTIAL_ERROR, 999);

    let resp = DGResult::ok_empty().to_response();
    assert_eq!(resp.status, 200);
    assert!(resp
        .headers
        .iter()
        .any(|(k, v)| k == "Content-Type" && v.contains("application/json")));
}

/// JSON 转义（对齐 .NET `JavaScriptEncoder.Default`）。
#[test]
fn json_escape_matches_dotnet_encoder() {
    assert_eq!(json_escape("plain"), "\"plain\"");
    assert_eq!(json_escape("a\"b\\c"), "\"a\\\"b\\\\c\"");
    assert_eq!(
        json_escape("x<y>&'z"),
        "\"x\\u003Cy\\u003E\\u0026\\u0027z\""
    );
    assert_eq!(json_escape("line\nbreak\ttab"), "\"line\\nbreak\\ttab\"");
    assert_eq!(json_escape("\u{1}"), "\"\\u0001\"");
    assert_eq!(json_escape("中"), "\"\\u4E2D\"");
    assert_eq!(json_escape("😀"), "\"\\uD83D\\uDE00\"");
}

/// 路由级 WebSocket 端点：非升级请求 426；dhrust 客户端升级后回显。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn router_ws_endpoint() {
    let mut r = Router::new();
    r.map_ws(
        "/ws",
        WsServerHooks {
            on_message: Some(Arc::new(|m| {
                m.conn.send_text(format!("echo:{}", m.text));
            })),
            ..Default::default()
        },
    );
    let addr = start(r).await;

    // 非升级请求 → 426 + 版本头
    let (s, head, _b) = http_call(addr, "GET", "/ws", &[], b"").await;
    assert_eq!(s, 426);
    assert!(
        head.to_lowercase().contains("sec-websocket-version: 13"),
        "426 应携带版本提示: {head}"
    );

    // dhrust 客户端升级 → 回显
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(16);
    let client = WsClient::connect(
        format!("ws://127.0.0.1:{}/ws", addr.port()),
        WsHooks {
            on_message: Some(Arc::new(move |m| {
                let _ = tx.try_send(m.text);
            })),
            ..Default::default()
        },
        WsClientOptions {
            connect_timeout: Duration::from_millis(800),
            ..Default::default()
        },
    );
    assert!(
        wait_until(|| client.is_connected(), 2000).await,
        "未连接到路由 WS 端点"
    );
    assert!(client.send_text("hi"));
    let got = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("2 秒内未收到回显")
        .unwrap();
    assert_eq!(got, "echo:hi");
    client.close();
}
