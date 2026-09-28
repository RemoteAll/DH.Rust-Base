//! 基准：dhrust 完整实现（hyper 升级 + fastwebsockets 帧层 + 自研会话/语义层）WS 回显服务端。
//! 与 ws_fast（裸 fastwebsockets 直写）对照：验证会话层（分发/写通道/控制帧）的叠加开销。
//! 用法: ws_dhrust [addr]（默认 127.0.0.1:18094）

use std::io;
use std::sync::Arc;

use dhrust::net::http::{
    handler, HttpOutcome, HttpRequest, HttpResponse, HttpServer, HttpServerOptions,
};
use dhrust::net::ws::{WsServerHooks, WsServerOptions};

#[tokio::main]
async fn main() -> io::Result<()> {
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:18094".into());
    let server = HttpServer::bind(&addr).await?;
    println!("ws_dhrust listening on {addr}");

    let hooks = WsServerHooks {
        on_message: Some(Arc::new(|m| {
            // 回显（与 ws_raw/ws_tungstenite/ws_fast 同一语义：文本原样回发）
            m.conn.send_text(m.text.clone());
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
    // 内联模式：与裸库/C#/NewLife 同构（读→处理→写同任务完成），公平对比完整语义的叠加开销
    // 每连接独立线程：对齐 NewLife/IOCP 完成线程直处理模型（回环乒乓时延更低）
    let options = HttpServerOptions {
        ws: WsServerOptions {
            inline_handlers: true,
            ..Default::default()
        },
        thread_per_connection: true,
        ..Default::default()
    };
    server.serve_with(svc, options).await
}
