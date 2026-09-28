//! net::ws —— WebSocket 会话层（fastwebsockets 帧层 + 自研会话语义）。
//!
//! 规划（批次 5：N002 客户端 / N003 服务端升级）：
//! - 帧层：`fastwebsockets::WebSocket::after_handshake(stream, role)` 接管（握手自管）；
//!   关闭自动行为（`set_auto_close/set_auto_pong`），掩码与大小上限由会话层控制；
//! - 客户端：连接超时 10s、防并发重连、心跳（Ping + relay_update）、Pong 超时 90s → 重连；
//! - 发送：单一 `mpsc` 写通道（单写者，对齐 C# `_sendLock`），业务/心跳/延迟响应用同一队列；
//! - 接收：读循环只做控制帧处理与 RPC 消息 `tokio::spawn` 转发（**永不阻塞**，长任务后台化）；
//! - 延迟响应：Dispatcher 返回 `None` 时不发包，后台任务完成后经写通道按 requestId 补发。
//!
//! 本文件为 N001 骨架占位（行为用例先于实现：对齐 C# 修复清单）。
