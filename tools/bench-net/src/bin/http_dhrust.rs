//! 基准：dhrust 完整实现（hyper + 自研语义层 Router）HTTP 服务端。
//! 与 http_hyper（裸 hyper 最优写法）对照：验证语义层（路由匹配/body 收集/响应构造）的叠加开销。
//! 用法: http_dhrust [addr]（默认 127.0.0.1:18084）

use std::io;

use dhrust::net::http::{HttpOutcome, HttpResponse, HttpServer, HttpServerOptions};
use dhrust::net::router::{route, Ctx, Router};

#[tokio::main]
async fn main() -> io::Result<()> {
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:18084".into());
    let server = HttpServer::bind(&addr).await?;
    println!("http_dhrust listening on {addr}");

    let mut router = Router::new();
    // 与 http_hyper 的 /ping 响应体一致（16 字节 JSON）
    router.map_get(
        "/ping",
        route(|_ctx: Ctx| async move {
            HttpOutcome::Response(HttpResponse::json(200, "{\"code\":0,\"msg\":\"ok\"}"))
        }),
    );
    // 回显请求体（http_load POST 模式；move 取走 body——零额外拷贝）
    router.map_post(
        "/echo",
        route(|ctx: Ctx| async move {
            let body = ctx.req.body;
            HttpOutcome::Response(HttpResponse::bytes(200, "application/octet-stream", body))
        }),
    );

    // 每连接独立线程：对齐 NewLife/IOCP 完成线程直处理模型（与 C# 对照同构）
    let options = HttpServerOptions {
        thread_per_connection: true,
        ..Default::default()
    };
    server.serve_with(router.into_handler(), options).await
}
