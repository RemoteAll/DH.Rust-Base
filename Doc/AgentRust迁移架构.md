# DHDeploy.Agent Rust 迁移架构

> 配套《AgentRust迁移需求》；网络内核准入与性能红线见《网络层选型复核》（已锁定：HTTP=hyper、
> WS 帧层=fastwebsockets、语义层自研）。

## 1. 架构概览

> 工程位置：`F:\Project\DHDeploy\DHDeploy.Agent.Rust\`（DHDeploy 仓库内，与 C# `DHDeploy.Agent` 并列；
> `dhrust` 以路径依赖引用，Rust 工程独立构建）。dhrust 框架层（`net`/`razor` 等）在
> `F:\Code\Rust\DH.RustBase` 仓库。

```
DHDeploy.Agent.Rust（bin）
├── 宿主（配置/日志/数据目录/生命周期）
├── WS 客户端 ──────────► DHDeploy.Server（中继 RPC、心跳、部署下发）
├── HTTP 服务端（Controllers：BaoTa/Database/FileManager/Upload…）
├── Jobs ×4（磁盘/流量/检查/分片清理）
└── 依赖库 dhrust
    ├── net::ws   （fastwebsockets 帧层 + 自研会话/心跳/重连/发送串行）
    ├── net::rpc  （RPC 消息模型与 Dispatcher，对齐 WebSocketRpcModels）
    ├── net::http （hyper 服务端 + Map/Use 语义层 + 客户端封装）
    ├── razor     （页面渲染，可选承载状态页）
    ├── config / logs / sign / threading / times / io（既有能力）
```

- 运行时：tokio multi-thread；单二进制；Linux 优先（musl 交叉编译，zigbuild 已备）。
- 与 C# 共存：协议一致，混合集群允许 Rust/C# Agent 同时在线。

## 2. 模块设计（dhrust，feature `net`）

### 2.1 net::ws —— WebSocket 会话层

- 帧层：`fastwebsockets`（`after_handshake(stream, role)` 接管，握手自管以对齐 DH.NCore 语义）；
  `Role::{Client, Server}`；自动行为全部关闭（`set_auto_close/auto_pong/auto_apply_mask` 由我们控制）。
- 客户端：连接（超时 10s）→ 握手（HTTP 升级）→ 读循环 + 写任务；断线自动重连（防抖）；
  心跳看门狗：定时发送 Ping + relay_update；Pong 超时（90s）→ 主动断开重连。
- **发送模型（对齐 `_sendLock`）**：所有发送经单一 `mpsc` 写通道（单写者）——
  业务发送、心跳、延迟响应共用同一队列，天然串行、天然不阻塞调用方。
- **接收模型（对齐"接收循环永不阻塞"）**：读循环只做：控制帧处理（Ping→Pong / Pong→喂看门狗）+
  RPC 消息 `tokio::spawn` 分发；长任务（下载/部署）在任务内执行，绝不 await 在读循环里。
- 延迟响应：Dispatcher 返回 `None` 时不发包；后台任务完成后，用捕获的 requestId 经写通道补发。
- 消息大小上限：对齐服务端配置（`set_max_message_size`）。

### 2.2 net::rpc —— RPC 协议层

- 消息模型：与 C# `WebSocketRpcModels` 字段级对齐（requestId / action / payload / response / 错误）；
- Dispatcher：`action → handler` 注册表；handler 签名 `async fn(ctx) -> Option<Response>`（None=延迟响应）；
- 上下文携带 requestId 与发送句柄（显式传递，替代 C# 的 AsyncLocal）。

### 2.3 net::http —— HTTP 层

- 服务端：`hyper http1::Builder::serve_connection(...).with_upgrades()`；
  WS 升级在服务内 `hyper::upgrade::on(req)` 后交 `net::ws`。
- 语义层（自研，对齐 DH.NCore `HttpServer/HttpRouter`）：Map/Use 路由、请求上下文、
  统一返回（StateCode/DGResult 对齐）、静态/流式响应；连接调优暴露 `max_buf_size/pipeline_flush` 等。
- 客户端（调用 Server REST / 本地星尘）：轻封装（连接复用 + 超时 + 重试）；多节点负载均衡沿用
  ApiHttpClient 语义（后续按需；本地星尘调用超时 ≥60s 的教训固化）。

## 3. 依赖与特性

| 依赖 | 版本（与 bench-net 已验证一致） | 用途 |
|------|--------------------------------|------|
| tokio | 1 | 运行时/网络/时间/同步 |
| hyper | 1.11 | HTTP/1.1 服务端与升级 |
| hyper-util | 0.1 | TokioIo 适配 |
| http-body-util | 0.1 | Body 工具 |
| fastwebsockets | 0.8 | WS 帧层（Apache-2.0，可 fork） |

- 全部为 `optional` 依赖，挂 `feature = "net"`；**默认构建与 `razor` 特性零影响**。
- TLS：rustls + ring（`net-tls`，wss 必需；Windows 构建需 NASM，已备便携版）。

## 4. 关键设计决策

| 决策 | 理由（对齐 C# 排障教训） |
|------|--------------------------|
| 读循环只转发、长任务后台化 | 修复"下载/读日志阻塞 Pong → 断连"类故障的结构性根治 |
| 单写者发送通道 | WebSocket 并发 Send 不安全；串行化且顺带获得延迟响应能力 |
| 握手自管（after_handshake） | 对齐 DH.NCore 握手/认证语义；可加统计与超时 |
| action 表驱动 Dispatcher | 与 C# RpcHandler 表逐项对齐，便于对照测试 |
| 基准先行（N005） | 沿用 razor 模式：红线（≥ 裸库基线、≥ C# 对照）不达标即触发预案 |

## 5. 任务分解

见《AgentRust迁移需求》第 6 节（迭代 1=批次 5：N001-N006；迭代 2=批次 6：A101-A105；迭代 3=批次 7：A201-A206）。

## 6. 风险与缓解

| 风险 | 影响 | 缓解 |
|------|------|------|
| fastwebsockets/hyper 与 C# 行为细节差异 | 协议边角不一致 | 用例先行（razor 模式：先建冒烟用例再实现）；N006 对接真实 Server |
| 反向代理下 wss/URL 构造 | 连接失败 | N006 冒烟覆盖代理场景；下载 URL 由服务端下发，保持兼容 |
| 长稳（数月在线） | 内存/句柄泄漏 | 读循环/写通道结构简单化；压测含长稳场景；对齐 C# 修复清单 |
| SQLite/文件竞态类教训 | 部署失败 | 打包/复制跳过 `-wal/-shm/-journal` 等固化进 N00x 用例 |

## 7. 变更记录

- 2026-09-28：立项（批次 5 网络内核启动；网络选型复核通过并锁定依赖矩阵）。
- 2026-09-28：Agent 工程位置确认——实现落 `F:\Project\DHDeploy\DHDeploy.Agent.Rust\`（DHDeploy 仓库）；框架层留 DH.RustBase。
