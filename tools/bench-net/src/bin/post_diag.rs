//! 低并发时延对照工具：单连接顺序 POST RTT —— 单写 vs 两写（Head/Body 分开写）。
//! 用途：① 复现压测器的两写行为，量化“两段到达”对服务端的影响；
//!       ② 作为“Agent 真实低并发形态”的时延对照（见 README N005 终测结果二）。
//! 用法: post_diag <addr> [iters]

use std::time::Instant;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[tokio::main]
async fn main() {
    let addr = std::env::args().nth(1).expect("addr");
    let iters: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(2000);

    for two_write in [false, true] {
        let mut sock = TcpStream::connect(&addr).await.unwrap();
        sock.set_nodelay(true).unwrap();
        let body = vec![b'a'; 1024];
        let head = format!(
            "POST /echo HTTP/1.1\r\nHost: bench\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
            body.len()
        );
        for _ in 0..100 {
            one(&mut sock, &head, &body, two_write).await;
        }
        let t0 = Instant::now();
        for _ in 0..iters {
            one(&mut sock, &head, &body, two_write).await;
        }
        let el = t0.elapsed();
        println!(
            "two_write={two_write:<5} iters={iters} avg={:.3}ms mps={:.0}",
            el.as_secs_f64() * 1000.0 / iters as f64,
            iters as f64 / el.as_secs_f64()
        );
    }
}

async fn one(sock: &mut TcpStream, head: &str, body: &[u8], two: bool) {
    if two {
        sock.write_all(head.as_bytes()).await.unwrap();
        sock.write_all(body).await.unwrap();
    } else {
        let mut req = Vec::with_capacity(head.len() + body.len());
        req.extend_from_slice(head.as_bytes());
        req.extend_from_slice(body);
        sock.write_all(&req).await.unwrap();
    }
    let mut acc: Vec<u8> = Vec::with_capacity(2048);
    let mut buf = [0u8; 8192];
    loop {
        let n = sock.read(&mut buf).await.unwrap();
        if n == 0 {
            panic!("closed");
        }
        acc.extend_from_slice(&buf[..n]);
        if let Some(pos) = acc.windows(4).position(|w| w == b"\r\n\r\n") {
            let head_txt = String::from_utf8_lossy(&acc[..pos]).to_ascii_lowercase();
            let cl: usize = head_txt
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0);
            if acc.len() >= pos + 4 + cl {
                return;
            }
        }
    }
}
