//! 基准：axum 0.8 最小服务器（hyper 之上的路由框架）。
//! /echo 采用请求体流式透传（零拷贝），代表 axum 的最优写法。

use axum::extract::Request;
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;

#[tokio::main]
async fn main() {
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:18083".into());

    let app = Router::new()
        .route("/ping", get(|| async { "{\"code\":0,\"msg\":\"ok\"}" }))
        .route("/echo", post(echo));

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    println!("http_axum listening on {addr}");
    axum::serve(listener, app).await.unwrap();
}

async fn echo(req: Request) -> Response {
    let body = req.into_body();
    Response::builder()
        .header("content-type", "application/json")
        .body(body)
        .unwrap()
}
