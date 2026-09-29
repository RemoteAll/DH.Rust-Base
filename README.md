# DH.RustBase（dhrust）

Pek 生态的 Rust 基础库，对应 C# 的 **DH.NCore**：为 Rust 项目提供与 DH 框架语义一致的基础设施，
让 C#/.NET 项目能够按模块渐进迁移到 Rust——两边可以**共享同一份配置文件、同一套定时与 Cron 语义**，
互不冲突地协同运行。

## 模块

| 模块 | 说明 | 对应 DH.NCore |
|------|------|----------------|
| `times` | 时间戳、与 C# 兼容的时间文本格式化/解析 | `Runtime` / `Convert.Trim` 等 |
| `io` | 文本文件读写（含 BOM 处理） | `File.ReadAllText/WriteAllText` 语义 |
| `sign` | 签名（SHA1 排序拼接）、随机串、MD5、FNV-1a | `Security` 扩展 |
| `random` | 安全随机（OS 熵）：URL 安全令牌、十六进制短标识 | — |
| `threading` | 不可重入定时器、调度器、Cron 表达式 | `NewLife.Threading`（`TimerX`/`TimerScheduler`/`Cron`） |
| `config` | 核心设置 `Setting`，读写 `Config/Core.config`（XML）/ `Core.json` | `NewLife.Setting` / `Configuration` |
| `logs` | 分级日志：控制台/文本文件/复合输出、全局门面与 `info!` 等宏 | `NewLife.Log`（`XTrace`/`ILog`/`TextFileLog`/`ConsoleLog`/`CompositeLog`） |
| `zip` | 极简 ZIP 打包器（store 法；内存 + 流式落盘） | — |
| `net` | 网络内核：HTTP 服务端/客户端、WebSocket（文本/二进制、可选服务端 Ping 保活与慢消费者断开）、RPC（feature `net`） | `Http` / `Net` / `Remoting`（部分） |
| `stun` | STUN 服务（RFC 5389 Binding；feature `stun`，`net` 自动包含） | — |
| `razor` | Razor 子集模板引擎（feature `razor`） | — |

## 定时与 Cron（threading）

```rust
use dhrust::threading::{Timer, TimerScheduler};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;

let count = Arc::new(AtomicI32::new(0));
let c = count.clone();

// 1 秒后开始，每 2 秒执行一次（不可重入，执行完再计时）
let timer = Timer::new(1000, 2000, move |_| {
    c.fetch_add(1, Ordering::SeqCst);
});

// Cron 定时器：每天 2:30
let cron_timer = Timer::new_cron("0 30 2 * * *", |_| {
    // ...
}).unwrap();

// 延迟执行一次
let delay = Timer::delay(500, |_| {
    // ...
});

// 结束时销毁（对应 C# TimerX.Dispose）
timer.cancel();
cron_timer.cancel();
delay.cancel();
TimerScheduler::clear_all();
```

语义与 C# `TimerX` 对齐的要点：

- **不可重入**：上一次回调未结束时不会再次触发；
- **执行完成后计时**（非绝对定时器），回调耗时不会导致堆积；
- **异常隔离**：回调 panic 会被捕获，调度线程继续运行；
- **一次性任务**：`period <= 0` 执行一次后自动销毁；
- **小于 10ms 的周期任务**会被直接销毁（与 C# 一致，避免占用 CPU）；
- 绝对定时器与 Cron 定时器通过 `TimerScheduler::set_time_source` 可注入时间源（测试友好）。

Cron 表达式支持 `*`、`?`、`,`、`-`、`/`、`#`（第几个星期几）、`L`（倒数），
星期 `0` 表示周日，可通过 `sunday` 字段切换偏移，求值结果与 C# 逐项一致。

## 配置（config）

```rust
use dhrust::config::Setting;

// 读取 Config/Core.config，文件不存在或损坏时返回默认值
let mut setting = Setting::load();

// 修改后保存（原子替换；内容无变化时跳过写入）
setting.log_level = "Debug".to_string();
setting.save().unwrap();
```

- 默认文件与 C# 一致：`Config/Core.config`（XML），同时支持 `Config/Core.json`（JSON）；
- 兼容 C# `Description` 注释、空元素、大小写差异，以及 JSON 中的注释与“全字符串值”；
- 支持 C# `Setting.OnLoaded` 的目录补全逻辑（`Web/Api/Server/Service/Job` 应用共享上级目录）。

## 日志（logs）

对齐 DH.NCore `NewLife.Log`（`XTrace`/`ILog`/`TextFileLog`/`ConsoleLog`/`CompositeLog`）：
分级（All/Debug/Info/Warn/Error/Fatal/Off，默认 Info）、多路输出、异步队列落盘。

```rust
// 控制台输出；RUST_LOG=debug 调级别（默认 Info）
dhrust::logs::use_console();
dhrust::logs::set_level(dhrust::logs::level_from_env());

dhrust::logs::info!("服务已启动：端口 {port}", port = 8080);
dhrust::logs::warn!("磁盘空间不足");

// 文件日志：按天一个文件（yyyy_MM_dd.log），单文件 10MB 拆分，最多保留 100 份
dhrust::logs::use_file("Log");
dhrust::logs::info!("写入 Log 目录");

// 控制台 + 文件同时输出（对应 XTrace.UseConsole）
dhrust::logs::use_console_options(true, true);
```

- 未设置时首次写入懒初始化为控制台日志（自动着色：终端且未设置 `NO_COLOR` 时启用）；
- 文件日志自带进程日志头（进程号/命令行/OS/CPU 等，对齐 `GetHead`）；
- 队列积压超过 1024 条时丢弃新日志（对齐 DH.NCore，防磁盘故障时内存无界）。

## 互操作校验

`scripts/interop.ps1` 会成对调用 Rust 示例与 C# 工具（引用 DH.NCore NuGet 包），验证：

1. **配置互通**：C# 写 → Rust 读、Rust 写 → C# 读（XML 与 JSON 各一轮，13 个字段全量比对）；
2. **Cron 求值一致**：同一表达式与起点，双方 `next/previous` 输出逐字符相同；
3. **定时器冒烟**：两侧定时任务均按期触发。

```powershell
powershell -ExecutionPolicy Bypass -File scripts\interop.ps1
# 通过时输出：=== INTEROP PASSED ===
```

## 构建与测试

```powershell
cargo test --all-features    # 全部单元测试（含 Cron/调度器/配置读写/日志/网络）
cargo clippy --all-targets --all-features
```

> 依赖镜像：工程内 `.cargo/config.toml` 已配置 rsproxy（本机 crates.io 直连不稳定）。
