//! 基准：自研极简 HTTP/1.1 服务器（对应 DH.NCore HttpServer 思路：原始 TCP + 手写解析）
//! 仅覆盖基准所需：GET /ping、POST /echo（Content-Length 请求体、keep-alive）。

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:18081".into());
    let listener = TcpListener::bind(&addr).await?;
    println!("http_raw listening on {addr}");
    loop {
        let (sock, _) = listener.accept().await?;
        tokio::spawn(async move {
            let _ = handle(sock).await;
        });
    }
}

async fn handle(mut sock: TcpStream) -> std::io::Result<()> {
    sock.set_nodelay(true)?;
    let mut buf: Vec<u8> = Vec::with_capacity(16 * 1024);
    let mut tmp = [0u8; 16 * 1024];

    loop {
        // 1. 读满请求头
        let head_end = loop {
            if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                break pos + 4;
            }
            let n = sock.read(&mut tmp).await?;
            if n == 0 {
                return Ok(());
            }
            buf.extend_from_slice(&tmp[..n]);
        };

        // 2. 解析请求行
        let head = &buf[..head_end];
        let mut lines = head.split(|&b| b == b'\n');
        let request_line = trim_cr(lines.next().unwrap_or(&[]));
        let mut parts = request_line.split(|&b| b == b' ');
        let method = parts.next().unwrap_or(&[]);
        let path = trim_cr(parts.next().unwrap_or(&[]));
        // 提前固化路由判定，避免在读取请求体时仍借用 buf
        let is_ping = method == b"GET" && path == b"/ping";
        let is_echo = method == b"POST" && path == b"/echo";

        // 3. 找 Content-Length
        let mut content_length = 0usize;
        for line in lines {
            let line = trim_cr(line);
            if line.is_empty() {
                break;
            }
            if line.len() >= 15 && line[..15].eq_ignore_ascii_case(b"content-length:") {
                content_length = parse_usize(&line[15..]);
            }
        }

        // 4. 补足请求体
        while buf.len() < head_end + content_length {
            let n = sock.read(&mut tmp).await?;
            if n == 0 {
                return Ok(());
            }
            buf.extend_from_slice(&tmp[..n]);
        }
        let body = &buf[head_end..head_end + content_length];

        // 5. 路由并组织响应（头体合并一次写出）
        let (status, payload): (&str, &[u8]) = if is_ping {
            ("200 OK", b"{\"code\":0,\"msg\":\"ok\"}")
        } else if is_echo {
            ("200 OK", body)
        } else {
            ("404 Not Found", b"not found")
        };

        let mut out = Vec::with_capacity(128 + payload.len());
        out.extend_from_slice(
            format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
                payload.len()
            )
            .as_bytes(),
        );
        out.extend_from_slice(payload);
        sock.write_all(&out).await?;

        // 6. 消费已处理数据
        buf.drain(..(head_end + content_length));
    }
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn trim_cr(line: &[u8]) -> &[u8] {
    if line.last() == Some(&b'\r') {
        &line[..line.len() - 1]
    } else {
        line
    }
}

fn parse_usize(v: &[u8]) -> usize {
    let mut n = 0usize;
    for &b in v {
        if b.is_ascii_digit() {
            n = n * 10 + (b - b'0') as usize;
        }
    }
    n
}
