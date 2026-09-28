//! 基准：自研极简 WebSocket 回显服务端（对应 DH.NCore `Http/WebSocket` + PacketCodec 思路）。
//! 支持：单帧 text/binary 回显、ping→pong、close 应答；不含分片重组（基准负载为单帧）。

use std::io::{self, IoSlice};

use bench_net::ws;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[tokio::main]
async fn main() -> io::Result<()> {
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:18091".into());
    let listener = TcpListener::bind(&addr).await?;
    println!("ws_raw listening on {addr}");
    loop {
        let (sock, _) = listener.accept().await?;
        tokio::spawn(async move {
            let _ = handle(sock).await;
        });
    }
}

async fn handle(mut sock: TcpStream) -> io::Result<()> {
    sock.set_nodelay(true)?;
    let mut buf: Vec<u8> = Vec::with_capacity(16 * 1024);
    let mut tmp = [0u8; 16 * 1024];

    // 1. 读取并完成握手
    let head_end = loop {
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
        read_more(&mut sock, &mut buf, &mut tmp).await?;
    };
    let key = ws::find_header(&buf[..head_end], b"sec-websocket-key").unwrap_or_default();
    let accept = ws::accept_key(&String::from_utf8_lossy(&key));
    let resp = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    sock.write_all(resp.as_bytes()).await?;
    buf.drain(..head_end);

    // 2. 帧循环（客户端帧必带掩码）
    loop {
        while buf.len() < 2 {
            read_more(&mut sock, &mut buf, &mut tmp).await?;
        }
        let b0 = buf[0];
        let b1 = buf[1];
        let fin = b0 & 0x80 != 0;
        let opcode = b0 & 0x0F;
        let masked = b1 & 0x80 != 0;
        let len7 = (b1 & 0x7F) as usize;

        let payload_len = if len7 < 126 {
            len7
        } else if len7 == 126 {
            while buf.len() < 4 {
                read_more(&mut sock, &mut buf, &mut tmp).await?;
            }
            u16::from_be_bytes([buf[2], buf[3]]) as usize
        } else {
            while buf.len() < 10 {
                read_more(&mut sock, &mut buf, &mut tmp).await?;
            }
            u64::from_be_bytes(buf[2..10].try_into().unwrap()) as usize
        };
        let header_len = if len7 < 126 {
            2
        } else if len7 == 126 {
            4
        } else {
            10
        };
        let mask_len = if masked { 4usize } else { 0 };
        let total = header_len + mask_len + payload_len;
        while buf.len() < total {
            read_more(&mut sock, &mut buf, &mut tmp).await?;
        }

        // 就地解掩码（避免复制）
        if masked {
            let m = [
                buf[header_len],
                buf[header_len + 1],
                buf[header_len + 2],
                buf[header_len + 3],
            ];
            ws::apply_mask(&mut buf[header_len + mask_len..total], m);
        }
        let payload = &buf[header_len + mask_len..total];

        match opcode {
            0x1 | 0x2 => {
                // 回显（服务端不掩码；帧头+载荷经 writev 一次发出，零中间拷贝）
                let header = ws::build_frame_header(fin, opcode, payload.len(), None);
                write_all_two(&mut sock, &header, payload).await?;
            }
            0x8 => {
                let header = ws::build_frame_header(true, 0x8, payload.len(), None);
                write_all_two(&mut sock, &header, payload).await?;
                return Ok(());
            }
            0x9 => {
                let header = ws::build_frame_header(true, 0xA, payload.len(), None);
                write_all_two(&mut sock, &header, payload).await?;
            }
            _ => {}
        }
        buf.drain(..total);
    }
}

/// 将两段缓冲以 writev 尽量一次写出（处理部分写）
async fn write_all_two(sock: &mut TcpStream, a: &[u8], b: &[u8]) -> io::Result<()> {
    let mut a = a;
    let mut b = b;
    loop {
        if a.is_empty() {
            return sock.write_all(b).await;
        }
        let n = sock
            .write_vectored(&[IoSlice::new(a), IoSlice::new(b)])
            .await?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "write zero"));
        }
        if n >= a.len() {
            let remain = n - a.len();
            b = &b[remain.min(b.len())..];
            a = &[];
        } else {
            a = &a[n..];
        }
    }
}

async fn read_more(sock: &mut TcpStream, buf: &mut Vec<u8>, tmp: &mut [u8]) -> io::Result<()> {
    let n = sock.read(tmp).await?;
    if n == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "closed"));
    }
    buf.extend_from_slice(&tmp[..n]);
    Ok(())
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}
