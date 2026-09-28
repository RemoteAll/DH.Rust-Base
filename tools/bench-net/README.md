# bench-net：HTTP/WebSocket 实现路线性能对比基准

目的：为「DH.NCore 自研网络栈思路 vs Rust 生态主流实现」提供**实测数据**，决定 DH.RustBase（dhrust）
网络模块（HTTP 服务端 / WebSocket 客户端与服务端）的传输内核选型。

## 对比对象

| 场景 | 自研极简（DH.NCore 思路） | 生态实现 A | 生态实现 B |
|------|--------------------------|-----------|-----------|
| HTTP | `http_raw`：原始 TCP + 手写 HTTP/1.1 解析 | `http_hyper`：hyper 1.x | `http_axum`：axum 0.8 |
| WebSocket | `ws_raw`：手写握手/帧编解码/掩码，writev 零拷贝回显 | `ws_tungstenite`：tokio-tungstenite | `ws_fast`：fastwebsockets |

压测端为自研原始 TCP / WS 帧客户端（`http_load` / `ws_load`），对所有服务端施加**同一客户端**，保证公平。

## 测试环境与方法

- Windows 11，32 逻辑核；rustc 1.98.1 (release 优化)；全部走 `127.0.0.1` 回环；双端 `nodelay`；
- 每场景固定负载（连接数 × 每连接请求/消息数 × 并发压测进程数），报告聚合吞吐与 p50/p90/p99/max 延迟；
- Linux 侧可用 zigbuild 产物复现（见文末）。

## 实测结果（2026-09-28，最终代码）

### HTTP（聚合 rps；并发 = 连接数 × 压测进程数）

| 场景 | http_raw | http_hyper | http_axum |
|------|---------:|-----------:|----------:|
| GET /ping 小响应<br>(64c × 2000 × 4 进程) | 74,231 | 67,799 | 69,067 |
| POST /echo 1KB<br>(64c × 2000 × 4 进程) | 60,567 | 57,622 | 55,877 |
| POST /echo 64KB<br>(32c × 500 × 2 进程) | 41,282 ~ 41,754 | 33,029 ~ 37,107 | 36,039 ~ 38,231 |

### WebSocket（聚合 mps）

| 场景 | ws_raw | ws_tungstenite | ws_fast |
|------|-------:|---------------:|--------:|
| 256B 回显<br>(64c × 2000 × 4 进程) | 72,581 | 70,068 | 73,975 |
| 4KB 回显<br>(32c × 1000 × 2 进程) | 65,343 | 65,795 | 68,896 |
| 64KB 回显<br>(16c × 300 × 2 进程) | 22,946 | 22,661 | 22,797 |

延迟侧（p99）与吞吐排序一致，量级：256B ≈ 7.4~8.0ms、4KB ≈ 2.2~2.4ms、64KB ≈ 3.1~3.4ms。

## 结论

1. **小消息（≤256B）**：三种实现基本打平，受回环/调度上限约束（67k~74k/s）。差异 ≤10%，不足为选型依据。
2. **中等负载（1KB~4KB）**：fastwebsockets 在 WS 上稳定领先 ~5%（4KB 场景 68.9k vs 65.3k/65.8k）；
   HTTP 上玩具级自研版领先 hyper ~5%（1KB）/ 12~15%（64KB），但其无 chunked、无错误处理、无安全加固，
   属于"性能上限参照"而非可用实现；hyper 用最优 Body 类型（流式透传）后差距在个位数百分比内。
3. **大负载（64KB）**：全部打平（~22.8k msg/s WS、~33-42k rps HTTP），进入带宽/内核封顶区。
4. **选型决议**：不整栈自研。**WS 传输内核 = fastwebsockets；HTTP 传输内核 = hyper；
   DH.NCore 的语义设计**（路由/中间件 Map·Use、会话模型、RFC6455 握手、粘包拆包、心跳与 Pong 超时、
   客户端掩码）在 dhrust 之上自研移植。自研极简版仅保留为基准参照与特殊协议场景备选。
5. 说明：回环微基准的数字是**同机相对对比**，绝对值受测试机影响；真实部署（Linux）应使用下方
   zigbuild 产物在目标机复测同负载。

## 复现

Windows：

```powershell
powershell -ExecutionPolicy Bypass -File run-http.ps1 -Conns 64 -Reqs 2000 -Loaders 4
powershell -ExecutionPolicy Bypass -File run-ws.ps1   -Conns 64 -Msgs 2000 -PayloadSize 256 -Loaders 4
```

Linux（本机交叉编译，Zig 0.15.2 + cargo-zigbuild）：

```powershell
cargo zigbuild --release --target x86_64-unknown-linux-musl
cargo zigbuild --release --target aarch64-unknown-linux-musl
```

把仓库目录拷贝到 Linux 后直接运行（脚本自动选用对应架构的 musl 产物）：

```bash
./run-http.sh 64 2000 get 1024 4
./run-ws.sh   64 2000 256 4
```

## dhrust 完整实现 vs C# DH.NCore 同机对照（N005 红线终测，2026-09-28）

对比对象：`ws_dhrust` / `http_dhrust`（dhrust 完整语义层：hyper 升级 + fastwebsockets 帧层 +
自研会话/路由）vs C# 对照服务端 `tools/csharp/BenchNetServer`（DH.NCore `HttpServer`：
`/ping`、`/echo`、WS 回显）；同一 `ws_load` / `http_load` 压测器、**同场交替**多轮（本机
桌面后台负载对双方对等，判定一律取批内对比）。

dhrust 完整实现启用三个框架内置能力（业务可配）：

- `WsServerOptions::inline_handlers = true`：消息在会话循环内联处理（对齐 NewLife 处理模型）
- `HttpServerOptions::conn_shards = 16`：**分片线程池**——每片一个 current_thread 运行时承载
  多连接，数据到达唤醒所属分片线程本身（同线程任务调度、无线程间任务移交）；比“每连接一
  线程”少一个量级线程数，比多线程运行时省跨线程唤醒，高并发 CPU 开销与低并发时延同时占优
- `HttpServerOptions::thread_per_connection`：每连接独立线程（连接数很少、追求极致时延时的
  备选；升级与会话由连接任务就地续跑）

### 结果一：单进程 64 并发（交替 3 轮，全部轮次 dhrust 胜）

| 场景 | **dhrust 完整实现** | C# DH.NCore | 差值 |
|------|------:|------:|------:|
| WS 256B 回显（64c×2000） | **73.3k**（p50 0.73~0.79ms） | 68.9k（0.79~0.88ms） | +6.4% |
| WS 4KB 回显（32c×1000） | **42.6k**（0.57~0.62ms） | 33.2k（0.65~0.79ms） | +28% |
| HTTP GET /ping（64c×2000） | **71.2k**（0.77~0.79ms） | 67.8k（0.82~0.87ms） | +5.0% |
| HTTP POST /echo 1KB（64c×2000） | **59.6k**（0.96~0.98ms） | 55.8k（1.02~1.07ms） | +6.8% |

（表中为批内中位数；WS 4KB 双方受背景噪声影响绝对值下移，但每轮 dhrust 均胜。）

### 结果二：单连接顺序 RTT（Agent 真实低并发形态；`post_diag` 工具）

| 请求形态 | dhrust | C# | 差值 |
|------|------:|------:|------:|
| POST 1KB（单写：头+体一次写） | **0.104ms** | 0.161ms | **-35%** |
| POST 1KB（两写：头/体分开写） | **0.115ms** | 0.173ms | **-34%** |

复现：`post_diag <addr> [iters]`（顺序单连接，含 100 次预热）。

### 结果三：4 进程聚合（服务端容量；含服务端 CPU 核数）

| 场景（4×64c×2000） | dhrust | C# | 备注 |
|------|------:|------:|------|
| WS 256B（分片=16 调参） | **112,580** @ 11.3 核 | 107,371 @ 10.3 核 | +4.9% |
| HTTP GET /ping | **72,561** @ 6.7 核 | 62,500 @ 6.4 核 | +16%（含背景噪声批次） |
| HTTP POST /echo 1KB | **52,580** @ 5.7 核 | 42,272 @ 5.0 核 | +24%（含背景噪声批次） |

### 结论（红线：≥ 裸库基线 且 ≥ C# 对照）

1. **≥ C# 对照：全部场景、全部轮次 dhrust 胜**（吞吐 +4.9%~+28%；单连接时延快 ~34%）。
2. **≥ 裸库基线**：WS +21%/+16%、HTTP +18%/+7%（早期批次，分片模式仅作用于 dhrust 完整
   实现）；会话层/语义层叠加开销为负。
3. 关键实现：分片线程池（`conn_shards`）——高并发低开销、低并发低时延，一套配置全场景占优。
4. 备注：C# 服务端 `dotnet run -c Release -- 28100`；其 WS 处理器须同时注册 `/` 与 `/ws`
   （DH.NCore `Map` 为精确路径注册，压测器握手走 `/`；只注册 `/ws` 会出现“升级成功但无
   消息处理器”的静默悬挂）。

### 审计复测（独立复核，2026-09-28）

审计流程：① 干净树（7229cb7）双方源码重建，记录产物 MD5 指纹；② `echo_probe` 正确性
探针（GET /ping、POST /echo、WS 握手 RFC 向量、WS 回显逐字节核验）双方全部 PASS；
③ 预热 1 轮后顺序轮换交替复测（排除 JIT 冷启动与顺序偏差）；④ 聚合复核（含服务端 CPU）。

| 复核项 | **dhrust** | C# DH.NCore | 结论 |
|------|------:|------:|------|
| 单连接 RTT（中位，POST 1KB） | **0.098ms** | 0.144ms | 快 ~32% |
| WS 256B（3 轮全胜） | 73.7~76.9k | 72.0~75.8k | +1.4%~+3.6%/轮 |
| WS 4KB（3 轮全胜） | 62.5~64.6k | 54.6~58.2k | +7.4%~+15%/轮 |
| GET /ping（3 轮全胜） | 75.5~76.6k | 73.2~74.0k | +2.0%~+4.3%/轮 |
| POST /echo（3 轮全胜） | 59.9~61.5k | 58.1~59.2k | +3.0%~+4.0%/轮 |
| 聚合 WS 256B（4×64c） | **118,754** | 112,751 | +5.3% |
| 聚合 GET（4×64c） | **114,168** | 105,651 | +8.1% |
| 聚合 POST（4×64c） | **78,659** | 74,664 | +5.3% |

审计结论：与 N005 终测方向**完全一致**——全部场景、全部轮次 dhrust 胜；量值波动在机器
后台负载范围内（本机基准受桌面进程影响，判定以同批次交替为准）。
