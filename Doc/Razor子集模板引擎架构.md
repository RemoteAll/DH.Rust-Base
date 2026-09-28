# Razor 子集模板引擎架构

> 所属库：DH.RustBase（crate `dhrust`），模块 `dhrust::razor`，feature `razor`
> 参照语义：ASP.NET Core Razor（.NET 10）；解析器技术参照开放规范（Razor 语言文档、minijinja 实现经验）
> 关联需求：`Doc/Razor子集模板引擎需求.md`

## 1. 架构概览

- 定位：不做通用模板语言，只做 **Razor 子集**——「同一份 `.cshtml` 双端渲染」合同的 Rust 端执行者。
- 流水线（单 crate 内、纯 Rust、零运行时第三方依赖）：

```
.cshtml 源文件
   │  lexer.rs（混合扫描器：HTML/代码切换、@、@@、@* *@、区块边界）
   ▼
Token 流
   │  parser.rs / expr.rs（递归下降：节点 + C# 子集表达式，优先级对齐 C#）
   ▼
AST（Node 树）
   │  render.rs（求值 + 转义 + 输出缓冲；错误带位置）
   ▼
HTML 字符串（预留 RenderWriter 流式；F014 预留 IR/Codegen）
```

- 与 C# 端关系（同源双执行）：
  - C# 端：ASP.NET Core 原生 Razor（互操作验证用 RazorEngineCore 免宿主编译）；
  - Rust 端：本引擎；
  - 一致性由 F011 用例集 + `scripts/razor_interop.ps1` 逐字节证明。
- 模块文件规划（`src/razor/`）：`mod.rs`（Engine/缓存/公共 API）、`error.rs`、`value.rs`、`lexer.rs`、`expr.rs`、`parser.rs`、`render.rs`。

## 2. 数据模型

### 2.1 模型值 `Value`
- 枚举：`Null / Bool(bool) / Int(i64) / Float(f64) / Str(String) / List(Vec<Value>) / Object(保留插入序)`
- 适配：`From<serde_json::Value>`（数据主通道），以及 `&str/String/i64/f64/bool/Vec<T>/BTreeMap`；
  Object 采用 builder 风格构造（`Value::object().set("k", v)`），插入序与 serde_json `preserve_order` 行为一致。
- 文本化规则（对齐 C#）：`null`→空串；`Bool`→`True/False`；`Int`→十进制；`Float`→最短往返（常见值用例覆盖）。

### 2.2 模板 AST

```
Template { nodes: Vec<Node> }
Node  = Text(String)
      | Write(Expr)                              // 转义输出
      | Raw(Expr)                                // 不转义输出（@Raw/@Html.Raw）
      | If { branches: Vec<(Expr, Vec<Node>)>, else_: Option<Vec<Node>> }
      | ForEach { var: String, iter: Expr, body: Vec<Node> }
      | Code(Vec<Stmt>)                          // @{ var x = expr; }
Stmt  = VarDecl { name: String, value: Expr }
Expr  = Lit(Literal) | Path(Vec<Seg>) | Unary(UnOp, Box<Expr>)
      | Bin(BinOp, Box<Expr>, Box<Expr>) | Ternary(Box, Box, Box) | Coalesce(Box, Box)
Seg   = Prop(String) | Index(Box<Expr>)
```

### 2.3 错误模型
- `ParseError { line, col, message, hint: Option<String> }`——超集语法给出「不支持 + 建议写法」提示。
- `RenderError { path, message }`——缺失属性/空链访问 = 错误（对齐 C# 的编译错/NullReferenceException，不静默输出空串）。

## 3. 接口设计

### 3.1 Rust API

| 接口 | 签名 | 说明 |
|------|------|------|
| 解析 | `Template::parse(&str) -> Result<Template, ParseError>` | 解析一次 |
| 渲染 | `Template::render(&Value) -> Result<String, RenderError>` | 纯函数、无状态 |
| 文件渲染（带缓存） | `Engine::render_file(&Path, &Value) -> Result<String, RenderError>` | 缓存键：路径 + mtime/长度 |
| 选项 | `Options { escape: bool = true, max_depth: usize = 32 }` | max_depth 防嵌套炸弹 |
| HTTP 集成（F013） | `http::Resp::view(&Engine, path, model)` | 依赖 feature `net` |

### 3.2 子集规约 v0.1（双端合同）

**标记**
- 隐式表达式 `@expr`；显式表达式 `@(expr)`；`@@` 输出字面 `@`；注释 `@* ... *@`。

**表达式（C# 子集）**
- 字面量：字符串（双引号+转义）、整数、浮点、`true/false`、`null`。
- 路径：`A.B`、`A[i]`；v0 不支持自由方法调用（`?.`/字符串插值列入迭代 3 评估）。
- 运算符与优先级（对齐 C#）：`! -`(单目) > `* / %` > `+ -` > `< > <= >=` > `== !=` > `&&` > `||` > `??` > `?:`（右结合）。

**语句与指令**
- `@if (...) { } else if (...) { } else { }`
- `@foreach (var x in expr) { }`
- `@{ var 名 = 表达式; ... }`（仅 var 声明序列）
- `@model T`：注解指令，Rust 端仅记录不做强类型校验。

**页面级固定模式（迭代 3）**
- 布局：`@{ Layout = "路径"; }`、`@RenderBody()`、`@section X { }`、`@await RenderSectionAsync("X", false)`
- Partials：`@await Html.PartialAsync("Name", model)`

**不支持（Rust 端显式报错）**
- 完整 C# 语句/LINQ/lambda/方法链调用；`@using`/`@inject`/`@functions`/`@helper`；`@switch`/`@while`；`await`（除上述固定模式）。

**语义对齐表**

| 语义 | C# Razor 行为 | Rust 引擎行为 |
|------|--------------|--------------|
| null 输出 | 空串 | 空串 |
| null/缺失链访问 | 编译错/运行时异常 | `RenderError`（快速失败） |
| Bool 文本 | `True/False` | `True/False` |
| 整数除法 | 截断 | 截断 |
| `+` 拼接 | 任一侧字符串即拼接 | 同 |
| 默认转义字符集 | `& < > " '`（v0 对齐目标） | 同；以互操作用例逐字节为准补全 |

### 3.3 C# 互操作接口（验证工具）

- `tools/csharp/RazorInterop`（.NET 10 控制台，RazorEngineCore 或最小 ASP.NET 承载）：
  `render <template.cshtml> <data.json>` → 输出 HTML。
- `scripts/razor_interop.ps1`：遍历 `tests/razor_cases` → 双端渲染 → 逐字节比对 → 汇总（沿用 `config_interop` 脚本风格，成功输出 `RAZOR INTEROP PASSED`）。

## 4. 技术选型

| 领域 | 选型 | 理由 |
|------|------|------|
| 解析器 | 手写递归下降 + 混合扫描器 | 精确对齐 Razor 语法与错误定位，避免通用模板库语义漂移 |
| 值模型 | 自研 `Value` + serde_json 适配 | 生态数据互通；避免渲染时反序列化往返 |
| 转义 | 自研最小 HtmlEncoder | 对齐 Razor 输出；字符集由用例反向验证补全 |
| 缓存 | 路径 + mtime/len（内容哈希兜底） | 零新依赖（复用 sha1） |
| 基准参照 | RazorEngineCore（编译缓存同口径）；Fluid 可选第二参照 | 免宿主、可脚本化 |
| 拒绝项 | proc-macro 代码生成（F014 触发前）、unsafe | 控制复杂度和供应链 |
| 新依赖 | 无（feature `razor` 空依赖，复用 serde_json/sha1） | 保持基库轻量 |

## 5. 关键设计决策

| 决策点 | 方案 | 备选 | 选择理由 |
|--------|------|------|---------|
| 一致性策略 | 子集规约 + C# 编译校验 + 逐字节比对用例集 | 「尽力兼容」 | 兼容性必须可证明、可回归 |
| null/缺失语义 | 渲染期错误（快速失败） | 静默空串 | 对齐 C# 异常语义，防静默差异 |
| 求值模型 | 动态 `Value` 查表 + 路径段预编译 | 强类型代码生成 | 先满足可用与性能；F014 视基准再上 |
| 输出方式 | 先 `String` 缓冲；预留 `RenderWriter` | 直接流式 | 简单可靠；HTTP 集成阶段再流式化 |
| 解析缓存 | 路径 + mtime/len | 内容哈希 | 快；内容哈希兜底 |
| 语义基线 | 用例锁定 .NET 10 Razor 行为 | 跟随最新 | 防漂移；升级走回归用例 |

## 6. 任务分解

> 复杂度=复杂（算法），批次大小 3-5 任务；每任务=一次对话工作量（编码+测试+自测）。

### 批次 1：核心可渲染
- **T001** 需求文档（本文件同批）✅
- **T002** 架构文档 ✅
- **T003** 模块骨架：`feature razor` + `src/razor/{mod,error,value}.rs`（Value、错误模型、公共签名占位）——产出：可编译骨架；验收：`cargo check --features razor` 通过
- **T004** 扫描器 `lexer.rs`：HTML/代码混合切分（`@`、`@@`、注释、块边界）——验收：扫描单测
- **T005** 表达式解析 `expr.rs`：字面量/路径/优先级/三元/`??`——验收：表达式单测（含负向）
- **[检查点 1]**：编译通过 + 扫描/表达式单测全绿

### 批次 2：语句与渲染
- **T006** 节点解析 `parser.rs`：`@if/@foreach/@{ }` 与嵌套——验收：AST 用例
- **T007** 渲染器 `render.rs`：求值+转义+输出+错误定位——验收：端到端渲染单测
- **T008** 用例集骨架 `tests/razor_cases`（≥5 个）+ Rust 侧比对测试——验收：`cargo test` 全绿
- **[检查点 2]**：基础渲染可用（文档示例页可渲染）

### 批次 3：双端验证与基准
- **T009** C# `RazorInterop` 工具 + `scripts/razor_interop.ps1`——验收：`RAZOR INTEROP PASSED`
- **T010** `tools/bench-view` + RazorEngineCore 对照——验收：实测报告（吞吐/分位）
- **T011** 差异修复（按 T009/T010 结果驱动）
- **[检查点 3]**：双端一致 + 性能达标（或触发 F014 评估）

### 批次 4：页面级能力（按需，3-5 任务拆分）
- **T012+** `@{var}` 扩展、F008 布局分区、F009 partials、F010 缓存、F013 HTTP 集成
- **[检查点 4]**

## 7. 风险与缓解

| 风险 | 影响 | 缓解措施 |
|------|------|---------|
| 子集语义差异（null/数值/转义/文化） | 双端输出不一致 | 用例集先行；差异即缺陷；规约冻结 .NET 10 基线 |
| 性能不达标 | 违背核心目标 | 基准提前到批次 3；兜底 F014；优化路径（切片/路径缓存/预分配） |
| 子集被业务突破 | Rust 端报错影响使用 | 报错附「建议写法」；CI 双检查（C# 编译 + Rust 解析） |
| 自研维护成本 | 长期负担 | 模块独立、接口窄；失败可整体回退方案 A（Liquid/Handlebars） |
| 会话上下文漂移 | 批处理中断 | 文档三件套 + 检查点报告 + 新会话续接模板 |

## 8. 变更记录

- 2026-09-28：初始版本（方案 C 立项；批次 1 启动）
