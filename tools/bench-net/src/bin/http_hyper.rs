//! 基准：hyper 1.x 最小 HTTP/1.1 服务器（Rust 生态标准内核）。
//! /echo 采用请求体流式透传（零拷贝），代表 hyper 的最优写法。

use std::convert::Infallible;

use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

type BenchBody = BoxBody<Bytes, hyper::Error>;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:18082".into());
    let listener = TcpListener::bind(&addr).await?;
    println!("http_hyper listening on {addr}");
    loop {
        let (stream, _) = listener.accept().await?;
        stream.set_nodelay(true)?;
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, service_fn(handle))
                .await;
        });
    }
}

async fn handle(req: Request<Incoming>) -> Result<Response<BenchBody>, Infallible> {
    let res = match (req.method().as_str(), req.uri().path()) {
        ("GET", "/ping") => ok_json(full(b"{\"code\":0,\"msg\":\"ok\"}".as_slice())),
        ("POST", "/echo") => ok_json(req.into_body().boxed()),
        _ => {
            let mut r = Response::new(full(b"not found".as_slice()));
            *r.status_mut() = StatusCode::NOT_FOUND;
            r
        }
    };
    Ok(res)
}

fn full(chunk: &[u8]) -> BenchBody {
    Full::new(Bytes::copy_from_slice(chunk))
        .map_err(|never| match never {})
        .boxed()
}

fn ok_json(body: BenchBody) -> Response<BenchBody> {
    let mut r = Response::new(body);
    r.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        "application/json".parse().unwrap(),
    );
    r
}
