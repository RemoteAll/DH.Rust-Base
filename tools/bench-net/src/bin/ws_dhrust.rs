//! 基准：dhrust 完整实现（hyper 升级 + fastwebsockets 帧层 + 自研会话/语义层）WS 回显服务端。
//! 与 ws_fast（裸 fastwebsockets 直写）对照：验证会话层（分发/写通道/控制帧）的叠加开销。
//! 用法: ws_dhrust [addr]（默认 127.0.0.1:18094）

use std::io;
use std::sync::Arc;

use dhrust::net::http::{
    handler, HttpOutcome, HttpRequest, HttpResponse, HttpServer, HttpServerOptions,
};
use dhrust::net::ws::{WsServerHooks, WsServerMessage, WsServerOptions};

#[tokio::main]
async fn main() -> io::Result<()> {
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:18094".into());
    let server = HttpServer::bind(&addr).await?;
    println!("ws_dhrust listening on {addr}");

    let hooks = WsServerHooks {
        on_message: Some(Arc::new(|m: WsServerMessage| {
            // 回显（与 ws_raw/ws_tungstenite/ws_fast 同一语义：文本原样回发）
            // 解构取走 text：省一次 String 克隆（回显高频路径）
            let WsServerMessage { text, conn } = m;
            conn.send_text(text);
        })),
        ..Default::default()
    };
    let svc = handler(move |req: HttpRequest| {
        let hooks = hooks.clone();
        async move {
            if req.path == "/" || req.path == "/ws" {
                HttpOutcome::WebSocket(hooks)
            } else {
                HttpOutcome::Response(HttpResponse::text(404, "not found"))
            }
        }
    });
    // 内联处理（读→处理→写同任务完成）+ 分片线程池（同线程唤醒、线程数可控）
    // 分片数可用环境变量 DHRUST_SHARDS 覆盖（默认 16——32 核机器调参最优区间）
    let shards: usize = std::env::var("DHRUST_SHARDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(16);
    let options = HttpServerOptions {
        ws: WsServerOptions {
            inline_handlers: true,
            ..Default::default()
        },
        conn_shards: shards,
        ..Default::default()
    };
    server.serve_with(svc, options).await
}
