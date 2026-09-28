//! WebSocket 基准公共组件：握手计算、帧头构造、掩码。
//! 对齐 RFC 6455（与 DH.NCore `Http/WebSocket`、`Net/WebSocketClient` 同算法）。

use base64::Engine;
use sha1::{Digest, Sha1};

/// RFC 6455 规定的握手 GUID
pub const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// 计算服务端握手应答值：base64(SHA1(key + GUID))（与 DH.NCore `WebSocket.ProcessRequest` 同算法）
pub fn accept_key(key: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(WS_GUID.as_bytes());
    let digest = hasher.finalize();
    base64::engine::general_purpose::STANDARD.encode(digest)
}

/// 从已读到的请求头中提取指定头部值（大小写不敏感，返回去空白后的值）
pub fn find_header(head: &[u8], name: &[u8]) -> Option<Vec<u8>> {
    for line in head.split(|&b| b == b'\n') {
        let line = if line.last() == Some(&b'\r') {
            &line[..line.len() - 1]
        } else {
            line
        };
        if line.is_empty() {
            break;
        }
        if let Some(colon) = line.iter().position(|&b| b == b':') {
            if line[..colon].eq_ignore_ascii_case(name) {
                let mut v = &line[colon + 1..];
                while v.first() == Some(&b' ') {
                    v = &v[1..];
                }
                return Some(v.to_vec());
            }
        }
    }
    None
}

/// 构造客户端握手请求报文
pub fn handshake_request(host: &str, path: &str, key: &str) -> Vec<u8> {
    format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
    )
    .into_bytes()
}

/// 构造帧头（服务端不掩码传 `None`；客户端出站帧必须传掩码键）
pub fn build_frame_header(fin: bool, opcode: u8, len: usize, mask: Option<[u8; 4]>) -> Vec<u8> {
    let mut header = Vec::with_capacity(14);
    header.push(if fin { 0x80 | opcode } else { opcode });
    let mask_bit = if mask.is_some() { 0x80u8 } else { 0u8 };
    if len < 126 {
        header.push(mask_bit | len as u8);
    } else if len <= 0xFFFF {
        header.push(mask_bit | 126);
        header.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        header.push(mask_bit | 127);
        header.extend_from_slice(&(len as u64).to_be_bytes());
    }
    if let Some(m) = mask {
        header.extend_from_slice(&m);
    }
    header
}

/// 就地掩码（RFC 6455 §5.3，客户端出站帧必须掩码）
pub fn apply_mask(payload: &mut [u8], mask: [u8; 4]) {
    for (i, b) in payload.iter_mut().enumerate() {
        *b ^= mask[i & 3];
    }
}
