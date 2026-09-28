//! Razor 子集模板引擎（方案 C）。
//!
//! 目标：同一份 `.cshtml`，C# 端由原生 Razor 渲染、Rust 端由本引擎渲染，
//! 子集内输出逐字节一致（规约见 `Doc/Razor子集模板引擎架构.md`「3.2 子集规约 v0.1」）。
//!
//! 设计要点：
//! - 纯 Rust、零新依赖（复用 serde_json / sha1）；
//! - 动态 [`Value`] 模型 + 解释执行（F014 视基准再评估代码生成）；
//! - 子集外语法显式报错（[`ParseError`] 含行列号与建议）。
//!
//! 当前进度（批次 1）：
//! - T003 ✅ 骨架：`value`（模型值）、`error`（错误模型）、[`Options`]
//! - T004 ✅ 扫描器 `lexer`：HTML/代码混合切分、注释/转义、块边界（单测覆盖）
//! - T005 ✅ 表达式子集 `expr`：字面量/路径/优先级/三目/`??`（单测覆盖）

pub mod error;
pub mod expr;
pub mod lexer;
pub mod value;

pub use error::{ParseError, RenderError};
pub use value::{Object, Value};

/// 渲染选项。
#[derive(Clone, Debug)]
pub struct Options {
    /// 隐式表达式是否进行 HTML 转义（默认 `true`，对齐 Razor 默认行为）
    pub escape: bool,
    /// 模板嵌套深度上限（防嵌套炸弹，默认 32）
    pub max_depth: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            escape: true,
            max_depth: 32,
        }
    }
}
