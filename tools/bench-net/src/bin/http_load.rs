//! HTTP 压测器（自研原始 TCP + keep-alive 顺序请求）。
//! 对三种服务端施加同一客户端实现，保证对比公平。
//! 用法: http_load <addr> <conns> <reqs_per_conn> <get|post> <body_size>

use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let addr = args.next().unwrap_or_else(|| "127.0.0.1:18081".into());
    let conns: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(64);
    let reqs: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(2000);
    let mode = args.next().unwrap_or_else(|| "get".into());
    let body_size: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(1024);

    // 预热：先建一次连接确认服务已就绪
    {
        let s = TcpStream::connect(&addr).await.expect("connect failed");
        drop(s);
    }

    let start = Instant::now();
    let mut handles = Vec::with_capacity(conns);
    for _ in 0..conns {
        let addr = addr.clone();
        let mode = mode.clone();
        handles.push(tokio::spawn(async move {
            run_conn(&addr, reqs, &mode, body_size).await
        }));
    }

    let mut all: Vec<Duration> = Vec::with_capacity(conns * reqs);
    for h in handles {
        let mut lat = h.await.expect("task panicked");
        all.append(&mut lat);
    }
    let elapsed = start.elapsed();
    all.sort_unstable();

    let pct = |p: f64| -> f64 {
        let idx = ((all.len() - 1) as f64 * p).round() as usize;
        all[idx].as_micros() as f64 / 1000.0
    };
    println!(
        "mode={mode} conns={conns} reqs/conn={reqs} total={} rps={:.0} p50={:.2}ms p90={:.2}ms p99={:.2}ms max={:.2}ms",
        all.len(),
        all.len() as f64 / elapsed.as_secs_f64(),
        pct(0.50),
        pct(0.90),
        pct(0.99),
        all.last().unwrap().as_micros() as f64 / 1000.0
    );
}

async fn run_conn(addr: &str, reqs: usize, mode: &str, body_size: usize) -> Vec<Duration> {
    let mut sock = TcpStream::connect(addr).await.expect("connect failed");
    sock.set_nodelay(true).expect("set nodelay");
    let body = vec![b'a'; body_size];
    let mut lat = Vec::with_capacity(reqs);
    let mut buf = vec![0u8; 64 * 1024];

    for _ in 0..reqs {
        let t0 = Instant::now();
        if mode == "post" {
            let head = format!(
                "POST /echo HTTP/1.1\r\nHost: bench\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
                body.len()
            );
            sock.write_all(head.as_bytes()).await.unwrap();
            sock.write_all(&body).await.unwrap();
        } else {
            sock.write_all(b"GET /ping HTTP/1.1\r\nHost: bench\r\nConnection: keep-alive\r\n\r\n")
                .await
                .unwrap();
        }
        read_response(&mut sock, &mut buf).await;
        lat.push(t0.elapsed());
    }
    lat
}

/// 读取一个完整响应（响应头 + Content-Length 正文）
async fn read_response(sock: &mut TcpStream, buf: &mut [u8]) {
    let mut acc: Vec<u8> = Vec::with_capacity(256);
    let mut total_needed: usize = usize::MAX;
    loop {
        if acc.len() >= total_needed {
            return;
        }
        let n = sock.read(buf).await.unwrap();
        if n == 0 {
            panic!("connection closed early");
        }
        acc.extend_from_slice(&buf[..n]);
        if total_needed == usize::MAX {
            if let Some(pos) = find_subslice(&acc, b"\r\n\r\n") {
                total_needed = pos + 4 + parse_content_length(&acc[..pos]);
            }
        }
    }
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn parse_content_length(head: &[u8]) -> usize {
    for line in head.split(|&b| b == b'\n') {
        let line = trim_cr(line);
        if line.len() >= 15 && line[..15].eq_ignore_ascii_case(b"content-length:") {
            let mut n = 0usize;
            for &b in &line[15..] {
                if b.is_ascii_digit() {
                    n = n * 10 + (b - b'0') as usize;
                }
            }
            return n;
        }
    }
    0
}

fn trim_cr(line: &[u8]) -> &[u8] {
    if line.last() == Some(&b'\r') {
        &line[..line.len() - 1]
    } else {
        line
    }
}
