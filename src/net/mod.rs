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
//! 设计约束（C# 排障教训固化，见《AgentRust迁移需求》第 4 节，DHDeploy 仓库 `Doc/`）：
//! 接收循环永不阻塞（长任务后台化）；发送经单一写通道串行；Pong 超时 90s 触发重连。
//!
//! 依赖矩阵与版本锁定见《AgentRust迁移架构》第 3 节（DHDeploy 仓库 `Doc/`）。

#[cfg(feature = "net")]
pub mod http;
#[cfg(feature = "http-client")]
pub mod http_client;
#[cfg(feature = "net")]
pub mod router;
#[cfg(feature = "net")]
pub mod rpc;
#[cfg(feature = "stun")]
pub mod stun;
#[cfg(feature = "net-tls")]
pub mod tls;
#[cfg(feature = "net")]
pub mod ws;

// ———— 基础能力（零依赖，无条件可用）————

/// 字节流分帧（终结符切分 + 保活串剥除 + 超长保护 + 空闲结算；tcp-scanner-server 实践沉淀收编）。
pub mod framing;

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
}
