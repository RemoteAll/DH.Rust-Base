//! 基准：fastwebsockets 回显服务端（Rust 生态中宣称为最快的 WS 实现）。
//! 握手复用公共模块完成，随后交给 fastwebsockets 处理帧编解码。

use std::io;

use bench_net::ws;
use fastwebsockets::{Frame, OpCode, Role, WebSocket};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[tokio::main]
async fn main() -> io::Result<()> {
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:18093".into());
    let listener = TcpListener::bind(&addr).await?;
    println!("ws_fast listening on {addr}");
    loop {
        let (sock, _) = listener.accept().await?;
        tokio::spawn(async move {
            let _ = handle(sock).await;
        });
    }
}

async fn handle(mut sock: TcpStream) -> io::Result<()> {
    sock.set_nodelay(true)?;

    // 手工握手（与 ws_raw 同一套逻辑；压测客户端在收到 101 前不会发送帧）
    let mut buf: Vec<u8> = Vec::with_capacity(16 * 1024);
    let mut tmp = [0u8; 16 * 1024];
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
    let key = ws::find_header(&buf[..head_end], b"sec-websocket-key").unwrap_or_default();
    let accept = ws::accept_key(&String::from_utf8_lossy(&key));
    let resp = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    sock.write_all(resp.as_bytes()).await?;

    let mut ws = WebSocket::after_handshake(sock, Role::Server);
    loop {
        let frame = match ws.read_frame().await {
            Ok(f) => f,
            Err(_) => return Ok(()),
        };
        match frame.opcode {
            OpCode::Text | OpCode::Binary => {
                ws.write_frame(Frame::new(true, frame.opcode, None, frame.payload))
                    .await
                    .map_err(|e| io::Error::other(e.to_string()))?;
            }
            OpCode::Close => return Ok(()),
            _ => {}
        }
    }
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}
