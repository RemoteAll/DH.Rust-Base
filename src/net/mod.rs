//! 网络模块（feature `net`）：HTTP 服务端与 WebSocket 会话——DHDeploy.Agent Rust 迁移内核。
//!
//! 选型（依据迁移文档《网络层选型复核》，DHDeploy 仓库 `Doc/`；两条件闸门已通过并锁定）：
//! - **HTTP**：hyper 1.x（`serve_connection` + `.with_upgrades()`）；语义层自研——
//!   `net::http`（服务端/未一兼返回）与 `net::router`（Map/Use 路由与上下文），
//!   对齐 DH.NCore `HttpServer/HttpRouter`；
//! - **WebSocket**：fastwebsockets 帧层（`after_handshake` 接管、自动行为全关），
//!   会话/心跳/重连/发送串行/延迟响应自研，对齐 C# `MyWebSocketClient` 行为；
//! - **RPC**：与 C# `WebSocketRpcModels` 字段级对齐的消息模型与 Dispatcher。
//!
//! 设计约束（C# 排障教训固化，见《AgentRust迁移需求》第 4 节，DHDeploy 仓库 `Doc/`）：
//! 接收循环永不阻塞（长任务后台化）；发送经单一写通道串行；Pong 超时 90s 触发重连。
//!
//! 依赖矩阵与版本锁定见《AgentRust迁移架构》第 3 节（DHDeploy 仓库 `Doc/`）。

pub mod http;
pub mod router;
pub mod rpc;
pub mod ws;

// ———— N001 依赖闸门（防 feature 空转：编译期验证依赖版本 API 形态）————

/// 编译期自检：hyper 服务端连接构造器 API 形态（http1 + server 特性）。
#[allow(dead_code)]
fn _dep_gate_http(
    builder: hyper::server::conn::http1::Builder,
) -> hyper::server::conn::http1::Builder {
    builder
}

/// 编译期自检：fastwebsockets 帧会话 API 形态（任意 AsyncRead+AsyncWrite 流上接管）。
#[allow(dead_code)]
fn _dep_gate_ws(
    ws: fastwebsockets::WebSocket<tokio::io::DuplexStream>,
) -> fastwebsockets::WebSocket<tokio::io::DuplexStream> {
    ws
}
