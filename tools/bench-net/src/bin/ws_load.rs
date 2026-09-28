//! WebSocket 压测器（自研帧编解码，与 ws_raw 同一实现路径）。
//! 对三种服务端施加同一客户端实现，保证对比公平。
//! 用法: ws_load <addr> <conns> <msgs_per_conn> <payload_size>

use std::time::{Duration, Instant};

use bench_net::ws;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let addr = args.next().unwrap_or_else(|| "127.0.0.1:18091".into());
    let conns: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(64);
    let msgs: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(2000);
    let payload_size: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(256);

    let start = Instant::now();
    let mut handles = Vec::with_capacity(conns);
    for _ in 0..conns {
        let addr = addr.clone();
        handles.push(tokio::spawn(
            async move { run_conn(&addr, msgs, payload_size).await },
        ));
    }

    let mut all: Vec<Duration> = Vec::with_capacity(conns * msgs);
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
        "payload={payload_size}B conns={conns} msgs/conn={msgs} total={} mps={:.0} p50={:.2}ms p90={:.2}ms p99={:.2}ms max={:.2}ms",
        all.len(),
        all.len() as f64 / elapsed.as_secs_f64(),
        pct(0.50),
        pct(0.90),
        pct(0.99),
        all.last().unwrap().as_micros() as f64 / 1000.0
    );
}

async fn run_conn(addr: &str, msgs: usize, payload_size: usize) -> Vec<Duration> {
    let mut sock = TcpStream::connect(addr).await.expect("connect failed");
    sock.set_nodelay(true).expect("set nodelay");

    // 握手
    let key = "dGhlIHNhbXBsZSBub25jZQ==";
    let req = ws::handshake_request(addr, "/", key);
    sock.write_all(&req).await.unwrap();

    let mut buf = vec![0u8; 64 * 1024];
    let mut acc: Vec<u8> = Vec::with_capacity(256);
    loop {
        if find_subslice(&acc, b"\r\n\r\n").is_some() {
            break;
        }
        let n = sock.read(&mut buf).await.unwrap();
        if n == 0 {
            panic!("handshake closed");
        }
        acc.extend_from_slice(&buf[..n]);
    }
    if find_subslice(&acc, b" 101 ").is_none() {
        panic!("unexpected handshake response");
    }

    let payload = vec![b'x'; payload_size];
    let mut lat = Vec::with_capacity(msgs);
    for _ in 0..msgs {
        let t0 = Instant::now();
        // 客户端出站帧必须掩码
        let mask: [u8; 4] = rand::random();
        let mut masked = payload.clone();
        ws::apply_mask(&mut masked, mask);
        let mut out = ws::build_frame_header(true, 0x1, masked.len(), Some(mask));
        out.extend_from_slice(&masked);
        sock.write_all(&out).await.unwrap();

        read_frame(&mut sock, &mut buf).await;
        lat.push(t0.elapsed());
    }
    lat
}

/// 读取一个完整回显帧（服务端不掩码）
async fn read_frame(sock: &mut TcpStream, buf: &mut [u8]) {
    let mut acc: Vec<u8> = Vec::with_capacity(1024);
    loop {
        if acc.len() >= 2 {
            let b1 = acc[1];
            let masked = b1 & 0x80 != 0;
            let len7 = (b1 & 0x7F) as usize;
            let (header_len, payload_len) = if len7 < 126 {
                (2usize, len7)
            } else if len7 == 126 {
                if acc.len() >= 4 {
                    (4usize, u16::from_be_bytes([acc[2], acc[3]]) as usize)
                } else {
                    (0usize, 0usize)
                }
            } else if acc.len() >= 10 {
                (
                    10usize,
                    u64::from_be_bytes(acc[2..10].try_into().unwrap()) as usize,
                )
            } else {
                (0usize, 0usize)
            };
            if header_len > 0 {
                let mask_len = if masked { 4usize } else { 0 };
                let total = header_len + mask_len + payload_len;
                if acc.len() >= total {
                    return;
                }
            }
        }
        let n = sock.read(buf).await.unwrap();
        if n == 0 {
            panic!("closed mid-frame");
        }
        acc.extend_from_slice(&buf[..n]);
    }
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}
