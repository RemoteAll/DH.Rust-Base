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
