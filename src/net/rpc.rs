//! net::rpc —— WebSocket RPC 协议层（与 C# `WebSocketRpcModels` 字段级对齐）。
//!
//! 规划（批次 6：A103）：
//! - 消息模型：requestId / action / payload / response / 错误（JSON，字段名与 C# 完全一致）；
//! - Dispatcher：`action → handler` 表驱动；handler 返回 `None` = 后台异步执行 + 延迟响应
//!   （requestId 显式传递，替代 C# 的 AsyncLocal 机制）；
//! - 与 `net::ws` 的写通道协作：响应统一经单写者通道发出（串行、永不阻塞读循环）。
//!
//! 本文件为 N001 骨架占位（协议字段以 C# 源为准，逐项对照后实现）。
