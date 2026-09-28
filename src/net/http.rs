//! net::http —— HTTP 服务端与语义层（自研，对齐 DH.NCore `HttpServer/HttpRouter`）。
//!
//! 规划（批次 5：N003 升级支撑 / N004 语义层）：
//! - 连接层：hyper `http1::Builder::serve_connection(io, service).with_upgrades()`
//!   （IO 泛型，可包我们的会话对象；升级路径交给 `net::ws`）；
//! - 语义层（自研）：Map/Use 路由（前缀注册 + 中间件链）、请求上下文（会话/租户/记录）、
//!   统一返回（StateCode / DGResult 语义对齐）、静态与流式响应；
//! - 客户端封装：调用 DHDeploy.Server REST / 本地星尘服务（超时 ≥60s 等教训固化）。
//!
//! 本文件为 N001 骨架占位（API 将在 N004 落地并以用例验证）。
