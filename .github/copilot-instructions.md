# DH.RustBase（dhrust）· 项目指令

> 全项目通用规则见用户级《跨项目接续协议》（本机 `%APPDATA%\Code\User\prompts\跨项目接续协议.instructions.md`；模板在 `tools/continuity/templates/`）。
> **开工先读（存在则读，按序）**：`.github/copilot-instructions.md` → `docs/项目接续指南.md` → `docs/用户档案.md` → `docs/项目记忆.md` → `docs/决策记录.md` 最新节。

## 项目速览

- **定位**：Pek 生态的 **Rust 基础库**（crate `dhrust`），对标 C# **DH.NCore**：让 C#/.NET 项目按模块渐进迁移到 Rust——两侧**共享同一份配置文件、同一套定时与 Cron 语义**，互不冲突协同运行。
- **技术栈**：Rust edition 2021；**feature 门控**模块（未启用即不编译、不拉依赖）；`.cargo/config.toml` 已配 rsproxy 镜像（本机 crates.io 直连不稳）。
- **消费方（path 源码依赖）**：Pek.RCode / Pek.RCard / Pek.RAgent / Pek.RPanlServer / DHDeploy.Agent.Rust 等。
- 远程：`https://github.com/RemoteAll/DH.Rust-Base.git`（master）。

## 关键约定（改动前必读）

- **与 C# 语义对齐是最高红线**：时间文本格式、配置读写（XML/JSON 字段与写回顺序）、Cron 求值、日志行格式与列序——均与 DH.NCore 逐字符对齐；动这些之前先读 `scripts/interop.ps1` 覆盖的三项互通断言。
- **不轻增依赖**：新依赖一律 `optional = true` + 独立 feature；重依赖面（hyper/tokio/rustls 等）只挂在 `net` / `net-tls` / `http-client` 等特性后，勿进默认路径。
- **改公共 API 必须复测消费方编译**（至少 Pek.RCode / Pek.RCard / Pek.RAgent 中受影响者），本库是它们的编译期上游。
- **文档即规范**：`Doc/`（Razor 子集引擎架构/需求、SPA 与 MVC 一体化）是设计依据，改行为先改文档。
- 中文 `.ps1` 必须 **UTF-8 with BOM**；跑 python 前设 `$env:PYTHONIOENCODING='utf-8'`。
- **打包/发版**：`tools/version-guard.ps1` 为各消费方打包脚本共用（同版本内容变化默认拒绝；`-Bump` / `-Force` 显式放行）。

## 构建与验证

```powershell
cargo test --all-features                              # 全量单测 474 项（Cron/调度器/配置读写/日志/网络/Razor）
cargo clippy --all-targets --all-features
powershell -NoProfile -File scripts\interop.ps1        # C#↔Rust 互通：配置 13 字段 + Cron 求值 + 定时器
powershell -NoProfile -File scripts\razor_interop.ps1  # Razor 子集双端渲染一致
```

## 每轮收尾（务必）

① `docs/决策记录.md` 新增一节（**含用户原话**）；② `tools/continuity/sync-memory.ps1` 同步记忆；③ 跑回归验证（本库 + 受影响消费方）并如实汇报；④ 跨仓改动跑 `tools/continuity/commit-all.ps1` 一并提交推送。
