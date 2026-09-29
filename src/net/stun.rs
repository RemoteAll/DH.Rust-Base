//! STUN 服务（RFC 5389 Binding 请求；UDP）——内置 WebRTC 公网地址发现。
//!
//! 用途：WebRTC 的 ICE 依赖 STUN 让客户端得知自己的公网映射地址；公共 STUN
//! （尤其 Google）在国内网络常不可达。本服务与业务服务同机部署后，客户端使用
//! `stun:<服务域名>:<端口>` 即可获得「永远可达」的 STUN——能连上业务服务就必然能连上它，
//! 显著提升跨网络 P2P 直连成功率（对称 NAT 场景仍需 TURN 才能保证）。
//!
//! 来源：PekSendToMo 内置 STUN（2026-09-29 收编）；支持 IPv4/IPv6 的
//! XOR-MAPPED-ADDRESS（RFC 5389 §15.2），非 STUN 数据静默丢弃。
//!
//! # 示例
//!
//! ```no_run
//! # async fn demo() {
//! // 与业务服务同进程部署：独立 Tokio 任务运行
//! if let Err(err) = dhrust::net::stun::serve("0.0.0.0", 3478).await {
//!     dhrust::logs::warn!("STUN 服务启动失败: {err}");
//! }
//! # }
//! ```

use std::net::{IpAddr, SocketAddr};

use tokio::net::UdpSocket;

/// RFC 5389 magic cookie
const MAGIC_COOKIE: u32 = 0x2112_A442;
/// Binding Request 消息类型
const MSG_BINDING_REQUEST: u16 = 0x0001;
/// Binding Success Response 消息类型
const MSG_BINDING_SUCCESS: u16 = 0x0101;
/// XOR-MAPPED-ADDRESS 属性类型
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;

/// 启动 STUN 服务（阻塞循环，应作为独立 Tokio 任务运行）。
///
/// 绑定失败（端口占用等）立即返回错误；运行期单包收发异常静默跳过，
/// 不中断服务（UDP 错误多为对端 ICMP 相关，属正常现象）。
pub async fn serve(bind: &str, port: u16) -> std::io::Result<()> {
    let socket = UdpSocket::bind((bind, port)).await?;
    let mut buf = [0u8; 1500];
    loop {
        let Ok((len, from)) = socket.recv_from(&mut buf).await else {
            // 单包接收失败（如对端 ICMP 相关错误）不中断服务
            continue;
        };
        if let Some(response) = handle_request(&buf[..len], from) {
            let _ = socket.send_to(&response, from).await;
        }
    }
}

/// 解析 Binding 请求并构造成功响应；非 STUN 数据返回 `None`（静默丢弃）
fn handle_request(request: &[u8], from: SocketAddr) -> Option<Vec<u8>> {
    if request.len() < 20 {
        return None;
    }
    let msg_type = u16::from_be_bytes([request[0], request[1]]);
    if msg_type != MSG_BINDING_REQUEST {
        return None;
    }
    let cookie = u32::from_be_bytes([request[4], request[5], request[6], request[7]]);
    if cookie != MAGIC_COOKIE {
        return None;
    }
    let transaction_id = &request[8..20];

    let mut response = Vec::with_capacity(48);
    response.extend_from_slice(&MSG_BINDING_SUCCESS.to_be_bytes());
    response.extend_from_slice(&[0, 0]); // 消息长度占位，稍后回填
    response.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
    response.extend_from_slice(transaction_id);

    // XOR-MAPPED-ADDRESS：告知客户端其在本 STUN 视角下的映射地址
    response.extend_from_slice(&ATTR_XOR_MAPPED_ADDRESS.to_be_bytes());
    let xor_port = from.port() ^ (MAGIC_COOKIE >> 16) as u16;
    match from.ip() {
        IpAddr::V4(ip) => {
            response.extend_from_slice(&8u16.to_be_bytes());
            response.push(0); // 保留字节
            response.push(0x01); // 地址族：IPv4
            response.extend_from_slice(&xor_port.to_be_bytes());
            let xor_ip = u32::from(ip) ^ MAGIC_COOKIE;
            response.extend_from_slice(&xor_ip.to_be_bytes());
        }
        IpAddr::V6(ip) => {
            response.extend_from_slice(&20u16.to_be_bytes());
            response.push(0); // 保留字节
            response.push(0x02); // 地址族：IPv6
            response.extend_from_slice(&xor_port.to_be_bytes());
            // IPv6 异或掩码 = magic cookie(4B) || transaction id(12B)（RFC 5389 §15.2）
            let mut mask = [0u8; 16];
            mask[..4].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
            mask[4..].copy_from_slice(transaction_id);
            let octets = ip.octets();
            let mut xored = [0u8; 16];
            for i in 0..16 {
                xored[i] = octets[i] ^ mask[i];
            }
            response.extend_from_slice(&xored);
        }
    }

    // 回填消息体长度（头部之后的部分）
    let body_len = (response.len() - 20) as u16;
    response[2..4].copy_from_slice(&body_len.to_be_bytes());

    Some(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    /// 构造一个合法的 Binding 请求（20 字节头部，无属性）
    fn binding_request() -> Vec<u8> {
        let mut request = Vec::new();
        request.extend_from_slice(&MSG_BINDING_REQUEST.to_be_bytes());
        request.extend_from_slice(&0u16.to_be_bytes());
        request.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        request.extend_from_slice(&[1u8; 12]); // transaction id
        request
    }

    #[test]
    fn responds_with_xor_mapped_address_ipv4() {
        let from: SocketAddr = "203.0.113.7:54321".parse().unwrap();
        let response = handle_request(&binding_request(), from).expect("应生成响应");

        assert_eq!(
            u16::from_be_bytes([response[0], response[1]]),
            MSG_BINDING_SUCCESS
        );
        assert_eq!(
            u16::from_be_bytes([response[2], response[3]]),
            12,
            "属性区长度应为 12"
        );
        assert_eq!(&response[8..20], &[1u8; 12], "transaction id 应原样回显");
        assert_eq!(
            u16::from_be_bytes([response[20], response[21]]),
            ATTR_XOR_MAPPED_ADDRESS
        );
        assert_eq!(response[25], 0x01, "地址族应为 IPv4");

        let xor_port = u16::from_be_bytes([response[26], response[27]]);
        assert_eq!(xor_port ^ (MAGIC_COOKIE >> 16) as u16, 54321);

        let xor_ip = u32::from_be_bytes([response[28], response[29], response[30], response[31]]);
        assert_eq!(
            Ipv4Addr::from(xor_ip ^ MAGIC_COOKIE),
            Ipv4Addr::new(203, 0, 113, 7)
        );
    }

    #[test]
    fn responds_with_xor_mapped_address_ipv6() {
        let from: SocketAddr = "[2001:db8::7]:54321".parse().unwrap();
        let request = binding_request();
        let response = handle_request(&request, from).expect("应生成响应");

        assert_eq!(
            u16::from_be_bytes([response[2], response[3]]),
            24,
            "属性区长度应为 24"
        );
        assert_eq!(response[25], 0x02, "地址族应为 IPv6");

        let xor_port = u16::from_be_bytes([response[26], response[27]]);
        assert_eq!(xor_port ^ (MAGIC_COOKIE >> 16) as u16, 54321);

        // 逐字节还原：地址 = 异或值 XOR (cookie || transaction id)
        let mut mask = [0u8; 16];
        mask[..4].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
        mask[4..].copy_from_slice(&request[8..20]);
        let mut octets = [0u8; 16];
        for i in 0..16 {
            octets[i] = response[28 + i] ^ mask[i];
        }
        assert_eq!(
            Ipv6Addr::from(octets),
            Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 7)
        );
    }

    #[test]
    fn ignores_non_stun_packets() {
        let from: SocketAddr = "127.0.0.1:1234".parse().unwrap();
        assert!(handle_request(b"hello", from).is_none(), "短包应被忽略");

        let mut request = binding_request();
        request[4..8].copy_from_slice(&0u32.to_be_bytes());
        assert!(
            handle_request(&request, from).is_none(),
            "magic cookie 错误应被忽略"
        );
    }
}
