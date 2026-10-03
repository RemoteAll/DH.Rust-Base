//! 网络模块：基础能力无条件可用（仅依赖标准库）；`http-client`/`net`/`stun`/`net-tls` 特性扩展 HTTP/WS/RPC/STUN。
//!
//! 基础：
//! - [`my_ip`]：本机首选局域网 IPv4 地址（UDP 出口路由法；对应 DH.NCore `NetHelper.MyIP()`）。
//! - [`framing`]：字节流分帧器（终结符切分/保活串剥除/超长保护/空闲结算；tcp-scanner-server 现场实践收编）。
//!
//! `net`（网络内核，DHDeploy.Agent Rust 迁移；选型依据迁移文档《网络层选型复核》，
//! DHDeploy 仓库 `Doc/`；两条件闸门已通过并锁定）：
//! - **HTTP**：hyper 1.x（`serve_connection` + `.with_upgrades()`）；语义层自研——
//!   `net::http`（服务端/未一兼返回）与 `net::router`（Map/Use 路由与上下文），
//!   对齐 DH.NCore `HttpServer/HttpRouter`；
//! - **WebSocket**：fastwebsockets 帧层（`after_handshake` 接管、自动行为全关），
//!   会话/心跳/重连/发送串行/延迟响应自研，对齐 C# `MyWebSocketClient` 行为；
//! - **RPC**：与 C# `WebSocketRpcModels` 字段级对齐的消息模型与 Dispatcher。
//!
//! `stun`（独立特性，仅依赖 tokio UDP；`net` 特性自动包含）：RFC 5389 Binding 服务，
//! 供浏览器 WebRTC 公网地址发现（来源：PekSendToMo 收编 2026-09-29）。
//!
//! `mqtt`（独立特性，仅依赖 tokio）：MQTT 3.1.1 客户端（连接认证 / QoS0·1 发布 /
//! 保活心跳 / 断线自动重连；互通目标 NewLife.MQTT；来源：tcp-scanner-server 对接
//! WMSMqttServer 收编 2026-09-30）。
//!
//! 设计约束（C# 排障教训固化，见《AgentRust迁移需求》第 4 节，DHDeploy 仓库 `Doc/`）：
//! 接收循环永不阻塞（长任务后台化）；发送经单一写通道串行；Pong 超时 90s 触发重连。
//!
//! 依赖矩阵与版本锁定见《AgentRust迁移架构》第 3 节（DHDeploy 仓库 `Doc/`）。

#[cfg(feature = "net")]
pub mod controller;
#[cfg(feature = "net")]
pub mod http;
#[cfg(feature = "http-client")]
pub mod http_client;
#[cfg(feature = "mqtt")]
pub mod mqtt;
#[cfg(feature = "net")]
pub mod router;
#[cfg(feature = "net")]
pub mod rpc;
#[cfg(feature = "net")]
pub mod static_files;
#[cfg(feature = "stun")]
pub mod stun;
#[cfg(feature = "net-tls")]
pub mod tls;
#[cfg(feature = "net")]
pub mod ws;

// ———— 基础能力（零依赖，无条件可用）————

/// 字节流分帧（终结符切分 + 保活串剥除 + 超长保护 + 空闲结算；tcp-scanner-server 实践沉淀收编）。
pub mod framing;

/// 登录限流（按来源的失败计数/封禁；默认 15 分钟 5 次 → 封禁 5 分钟——
/// Pek.RAgent 与 HlkProductTool 面板同款实现的收编）。
pub mod login_guard;

/// NewLife ApiClient 二进制 RPC（UDP；客户端与服务端同源，对齐 C# StarAgent 本地 RPC）。
pub mod api_rpc;

/// 获取本机首选的局域网 IPv4 地址（对应 DH.NCore `NetHelper.MyIP()`）。
///
/// 原理：向外部地址发起 UDP connect（不产生实际报文），由系统路由表选出出口网卡；
/// 纯内网无默认路由时返回 None，属正常情况。
pub fn my_ip() -> Option<std::net::Ipv4Addr> {
    use std::net::{SocketAddr, UdpSocket};

    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("223.5.5.5:80").ok()?;
    match sock.local_addr().ok()? {
        SocketAddr::V4(addr) => Some(*addr.ip()),
        _ => None,
    }
}

/// TCP 连通检查（连接成功即返回 `Ok`；探活/端口检查场景）。
///
/// - 仅建立并立即关闭连接，不发送任何数据；
/// - 解析失败/连接失败返回中文错误消息（来源：Pek.RAgent 健康检查收编）。
pub fn tcp_check(host: &str, port: u16, timeout: std::time::Duration) -> Result<(), String> {
    use std::net::{TcpStream, ToSocketAddrs};

    let addr_text = format!("{host}:{port}");
    let addr = addr_text
        .to_socket_addrs()
        .map_err(|e| format!("解析地址 {addr_text} 失败：{e}"))?
        .next()
        .ok_or_else(|| format!("无法解析地址 {addr_text}"))?;

    TcpStream::connect_timeout(&addr, timeout)
        .map(|_| ())
        .map_err(|e| format!("连接 {addr_text} 失败：{e}"))
}

/// 拆分 `host:port` 文本（按最后一个 `:` 切分；端口非法或缺省时回退 `default_port`）。
///
/// IPv6 字面量请使用 `[::1]:80` 形式（与 URL 惯例一致）。
pub fn split_host_port(text: &str, default_port: u16) -> (String, u16) {
    match text.rfind(':') {
        Some(i) => {
            let port = text[i + 1..].parse::<u16>().unwrap_or(default_port);
            (text[..i].to_string(), port)
        }
        None => (text.to_string(), default_port),
    }
}

// ———— N001 依赖闸门（防 feature 空转：编译期验证依赖版本 API 形态）————

/// 编译期自检：hyper 服务端连接构造器 API 形态（http1 + server 特性）。
#[cfg(feature = "net")]
#[allow(dead_code)]
fn _dep_gate_http(
    builder: hyper::server::conn::http1::Builder,
) -> hyper::server::conn::http1::Builder {
    builder
}

/// 编译期自检：fastwebsockets 帧会话 API 形态（任意 AsyncRead+AsyncWrite 流上接管）。
#[cfg(feature = "net")]
#[allow(dead_code)]
fn _dep_gate_ws(
    ws: fastwebsockets::WebSocket<tokio::io::DuplexStream>,
) -> fastwebsockets::WebSocket<tokio::io::DuplexStream> {
    ws
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn my_ip_never_panics() {
        // 结果依赖运行环境（可能有网卡也可能没有），这里只验证不崩溃
        let _ = my_ip();
    }

    #[test]
    fn split_host_port_variants() {
        assert_eq!(
            split_host_port("127.0.0.1:5500", 0),
            ("127.0.0.1".to_string(), 5500)
        );
        assert_eq!(
            split_host_port("127.0.0.1", 5500),
            ("127.0.0.1".to_string(), 5500)
        );
        // 非法端口回退默认值
        assert_eq!(split_host_port("host:abc", 80), ("host".to_string(), 80));
        // IPv6 字面量：按最后一个冒号切分（`[::1]:80`）
        assert_eq!(split_host_port("[::1]:80", 0), ("[::1]".to_string(), 80));
    }

    #[test]
    fn tcp_check_local_listener() {
        // 本机监听随机端口 → 连通成功；关闭后 → 连接失败
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(tcp_check("127.0.0.1", port, std::time::Duration::from_millis(1000)).is_ok());
        drop(listener);
        assert!(tcp_check("127.0.0.1", port, std::time::Duration::from_millis(500)).is_err());
    }
}
