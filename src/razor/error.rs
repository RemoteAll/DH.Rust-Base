//! Razor 子集模板引擎：错误模型（见架构文档 2.3）。

use std::fmt;

/// 模板解析错误（含行列定位）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    /// 行号（1 基）
    pub line: usize,
    /// 列号（1 基，按字符计）
    pub col: usize,
    /// 错误原因
    pub message: String,
    /// 修复建议（例如超集语法给出替代写法）
    pub hint: Option<String>,
}

impl ParseError {
    /// 创建解析错误。
    pub fn new(line: usize, col: usize, message: impl Into<String>) -> Self {
        Self {
            line,
            col,
            message: message.into(),
            hint: None,
        }
    }

    /// 附加修复建议。
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "模板解析错误（第 {} 行，第 {} 列）：{}",
            self.line, self.col, self.message
        )?;
        if let Some(hint) = &self.hint {
            write!(f, "；建议：{hint}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ParseError {}

/// 渲染错误（求值期）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderError {
    /// 出错位置（如 `Model.Sites[0].Name`）
    pub path: String,
    /// 错误原因
    pub message: String,
}

impl RenderError {
    /// 创建渲染错误。
    pub fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "模板渲染错误（{}）：{}", self.path, self.message)
    }
}

impl std::error::Error for RenderError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_error_display_includes_position_and_hint() {
        let e = ParseError::new(3, 7, "不支持的指令 @using").with_hint("请改用 @model 注解");
        let text = e.to_string();
        assert!(text.contains("第 3 行"));
        assert!(text.contains("第 7 列"));
        assert!(text.contains("建议"));
    }

    #[test]
    fn render_error_display_includes_path() {
        let e = RenderError::new("Model.Sites[0].Name", "属性不存在");
        assert!(e.to_string().contains("Model.Sites[0].Name"));
    }
}
