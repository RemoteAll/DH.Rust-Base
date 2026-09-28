# DHDeploy.Agent Rust 迁移需求

> 目标：用 Rust 复刻 DHDeploy.Agent（C#）同等功能，与现有 DHDeploy.Server / DHDeploy.Client 及
> C# Agent **协议完全互通、可混合部署**；性能沿用「基准先行、双端对照」验收（原则：不劣于 C#，力争更优）。
>
> 关联文档：《网络层选型复核》（网络内核准入与性能红线）、《Razor子集模板引擎需求/架构》（页面渲染，已完成）。

## 1. 背景与目标

- DHDeploy 体系：Client ↔ Server ↔ Agent（WebSocket 中继 RPC + HTTP 下发/上传）。
- 迁移动机：单二进制部署（无 .NET 运行时）、资源占用下降、性能提升；复用 dhrust 既有能力
  （razor 视图引擎、配置/日志/签名/定时器等）。
- 优先级：**功能等价 ＞ 协议互通 ＞ 性能不劣 ＞ 部署简化**。

### 1.1 实现位置（仓库分工，2026-09-28 用户确认）

| 内容 | 位置 | 说明 |
|------|------|------|
| dhrust 框架层（`net` 网络内核、razor、config/logs 等能力） | `F:\Code\Rust\DH.RustBase` | 批次 5 在此进行；提交至 dhrust 库 |
| **Agent 产品工程** | `F:\Project\DHDeploy\DHDeploy.Agent.Rust\` | **实现落 DHDeploy 仓库**，与 C# `DHDeploy.Agent` 并列；`dhrust` 以路径依赖引用；Rust 工程独立构建（不并入 DHDeploy.slnx） |
| 迁移文档（本文档 + 架构 + 网络选型复核） | dhrust 库 `Doc/` | 与选型复核配套存放；批次产出（验收记录等）回填本文档 |

## 2. 成功标准（可度量）

| 编号 | 标准 | 验收方式 |
|------|------|---------|
| S1 | 协议互通 | 与现网 Server/Client 混合部署跑通：连接、心跳（Ping + relay_update）、RPC 全表（app.restart / deploy.start / online.* / file.* / …） |
| S2 | 行为等价 | 重连防抖（isConnecting）、连接超时 10s、Pong 超时 90s 触发重连、发送串行、接收循环永不阻塞、延迟响应（后台执行 + 延迟发包）逐项对齐 |
| S3 | 性能 | WS 中继与 HTTP API 同机对照 C# Agent：吞吐与 p99 不劣；网络内核 ≥ 裸库基线（见《网络层选型复核》红线；不达标触发强化自研预案） |
| S4 | 部署 | 单可执行文件 + 配置/数据目录；兼容宝塔/星尘/守护进程重启场景 |

## 3. 功能需求（对照 C# 模块逐项迁移）

### 3.1 WebSocket 客户端（MyWebSocketClient 等价）

- 连接/自动重连：连接超时 10s；防并发重连；失败退避重试；
- 心跳：Ping + relay_update（间隔可配）；Pong 超时 90s → 断线重连；
- **发送串行化**（等价 `_sendLock`；WebSocket 不允许并发 Send）；
- **接收循环永不阻塞**：RPC 消息一律后台任务分发（长任务不阻塞 Pong 处理）；
- RPC 分发：requestId 透传；handler 返回空 = 后台异步执行 + **延迟响应**；
- 消息大小上限与服务端一致。

### 3.2 HTTP 服务端（Controllers 等价）

- BaoTa / Database / DirectFileManager / FileManager / Upload（分片上传）；
- 站点检查（CheckSiteSettings）、磁盘信息（DiskInfo）；
- 返回结构对齐（StateCode / DGResult 语义）。

### 3.3 定时任务（Jobs 等价）

- CheckAgentService / DiskUsageJob / NetTrafficJob / ChunkCleanupJob。

### 3.4 配置与事件

- Settings（appsettings 等价语义）、InstallConsumer（安装事件）。

## 4. 非功能需求

- **稳定性教训固化**（源自 C# 端排障史，全部转为设计约束/测试项）：
  接收循环永不阻塞；发送必须串行；长任务（下载/部署）后台化 + 延迟响应；
  打包/复制跳过 SQLite `-wal/-shm/-journal`；连接超时 10s 防重连卡死；本地星尘调用超时 ≥60s。
- 性能红线：批次 5 基准未达标 → 触发强化自研预案（见《网络层选型复核》）。
- 安全：wss/TLS（rustls + ring）；日志不输出凭据/内部地址。

## 5. 边界与约束

- **协议冻结**：以 C# 端为参照实现，兼容现网 Server；协议变更需双端同步。
- 不迁移（首版）：Cube 框架与 WEB 管理界面（如需网页管理，使用已完成的 dhrust razor 引擎自行承载）。
- 平台：Linux 优先（宝塔环境部署），Windows 兼容（开发机）。

## 6. 功能清单与迭代计划

### 迭代 1：网络内核（批次 5，dhrust `net` 特性）

| 编号 | 任务 | 验收 |
|------|------|------|
| N001 | 骨架与依赖闸门（feature `net`；tokio/hyper/fastwebsockets 版本锁定） | `cargo check --features net` 通过；默认构建零影响 |
| N002 | WS 客户端（握手/重连/心跳/串行发送/非阻塞接收/延迟响应） | 单测 + 基准 |
| N003 | WS 服务端 + HTTP 升级（hyper `.with_upgrades()`） | 单测 |
| N004 | HTTP 语义层（Map/Use 路由/请求上下文/统一返回） | 单测 |
| N005 | 基准工程（≥ 裸库基线 + 后续 C# 对照位） | 红线判定 |
| N006 | 与 DHDeploy.Server 协议冒烟（relay_update/RPC/延迟响应） | 冒烟通过 |

### 迭代 2：Agent 骨架（批次 6）

| 编号 | 任务 | 验收 |
|------|------|------|
| A101 | 工程骨架（bin + 配置/日志/数据目录） | 可启动 |
| A102 | WS 客户端接入（凭据/节点配置/重连） | 与 Server 连通 |
| A103 | RPC 全表注册（对齐 C# RpcHandler 表） | 报文互通 |
| A104 | Jobs ×4（Cron/间隔调度） | 单测 |
| A105 | 安装/事件（InstallConsumer） | 冒烟 |

### 迭代 3：能力面与验收（批次 7）

| 编号 | 任务 | 验收 |
|------|------|------|
| A201 | Controllers ×5（BaoTa/Database/DirectFileManager/FileManager/Upload） | 接口对照 |
| A202 | 分片上传（ChunkUpload 全流程） | 端到端 |
| A203 | 部署/重启链路（deploy.start / app.restart 含延迟响应） | 端到端 |
| A204 | 磁盘/流量/站点检查 | 对照 |
| A205 | C# 对照基准（同场景吞吐/p99） | 红线判定 |
| A206 | 验收记录（本文档第 7 节） | —— |

## 7. 验收记录

（批次完成后填写）

## 8. 术语表

| 术语 | 定义 |
|------|------|
| 中继（relay） | Agent 经由 Server 与 Client 之间的 RPC/文件转发 |
| 延迟响应 | RPC handler 后台执行、完成后按 requestId 补发响应（发包与接收循环解耦） |
| Pong 超时 | 90s 未收到服务端 Pong 即判定链路失效并重连 |
