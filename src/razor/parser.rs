//! Razor 子集模板引擎：模板 AST 与节点解析。
//!
//! 输入 [`crate::razor::lexer`] 的词法片段流，产出 [`Template`]：
//!
//! - [`Node::Text`]：原样 HTML；
//! - [`Node::Write`]：隐式 / 显式表达式（渲染时 HTML 转义）；
//! - [`Node::Raw`]：`@Raw(...)` / `@Html.Raw(...)` 固定模式（渲染时不转义）；
//! - [`Node::If`]：`@if { } else if { } else { }`（含嵌套）；
//! - [`Node::ForEach`]：`@foreach (var x in expr) { }`；
//! - [`Node::Code`]：`@{ var 名 = 表达式; ... }` 声明序列（[`Stmt::VarDecl`]）；
//! - `@model T`：记录到 [`Template::model`]，不做强类型校验。
//!
//! 子集外的用法（自由方法调用、非 var 语句、`Raw(...)` 链式访问等）在此显式报错，
//! 错误位置由 [`Token`] 的正文坐标精确推导。模板结构嵌套深度上限 [`MAX_NODE_DEPTH`]。

use crate::razor::error::ParseError;
use crate::razor::expr::{self, Expr};
use crate::razor::lexer::{self, Token, TokenKind};

/// 模板结构嵌套深度上限（@if / @foreach 嵌套层数）。
const MAX_NODE_DEPTH: usize = 64;

// ————— 模板 AST（对齐架构文档 2.2） —————

/// 解析后的模板。
#[derive(Clone, Debug, PartialEq)]
pub struct Template {
    /// 节点序列
    pub nodes: Vec<Node>,
    /// `@model T` 的类型原文（仅记录，不做强类型校验）
    pub model: Option<String>,
}

/// 模板节点。
#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    /// 原样文本（HTML）
    Text(String),
    /// 转义输出（隐式 / 显式表达式）
    Write(Expr),
    /// 不转义输出（`@Raw(...)` / `@Html.Raw(...)`）
    Raw(Expr),
    /// `@if` 链
    If {
        /// `(条件, 块体)` 序列（首项为 if，其后为 else if）
        branches: Vec<(Expr, Vec<Node>)>,
        /// `else` 块体
        else_: Option<Vec<Node>>,
    },
    /// `@foreach (var 变量 in 集合) { }`
    ForEach {
        /// 循环变量名
        var: String,
        /// 迭代集合表达式
        iter: Expr,
        /// 循环体
        body: Vec<Node>,
    },
    /// `@section 名称 { }`（F008；分区体在定义处立即渲染并收集）
    Section {
        /// 分区名
        name: String,
        /// 分区体
        body: Vec<Node>,
    },
    /// `@RenderBody()`（F008；布局中输出页面体）
    RenderBody,
    /// `@await RenderSectionAsync("名称", required)`（F008；布局中输出分区）
    RenderSection {
        /// 分区名
        name: String,
        /// 缺失时是否报错
        required: bool,
    },
    /// `@await Html.PartialAsync("名称", 模型表达式)`（F009）
    Partial {
        /// 部分视图名（受控目录内）
        name: String,
        /// 子模型表达式
        model: Expr,
    },
    /// `@{ ... }` 代码块
    Code(Vec<Stmt>),
}

/// 代码块语句（var 声明 / Layout 赋值）。
#[derive(Clone, Debug, PartialEq)]
pub enum Stmt {
    /// `var 名称 = 表达式;`
    VarDecl {
        /// 变量名
        name: String,
        /// 初始值表达式
        value: Expr,
    },
    /// `Layout = 表达式;`（F008；仅限 Layout）
    Assign {
        /// 目标名（仅 Layout）
        name: String,
        /// 表达式
        value: Expr,
    },
}

impl Template {
    /// 解析模板源码（扫描 + 节点构建，一次性）。
    pub fn parse(source: &str) -> Result<Template, ParseError> {
        let tokens = lexer::tokenize(source)?;
        let mut parser = NodeParser {
            tokens: &tokens,
            pos: 0,
            depth: 0,
            model: None,
        };
        let nodes = parser.parse_nodes(false)?;
        Ok(Template {
            nodes,
            model: parser.model,
        })
    }
}

// ————— 节点解析 —————

struct NodeParser<'t> {
    tokens: &'t [Token],
    pos: usize,
    /// 块嵌套深度（@if / @foreach）
    depth: usize,
    /// @model 记录
    model: Option<String>,
}

impl<'t> NodeParser<'t> {
    fn peek(&self) -> Option<&'t Token> {
        self.tokens.get(self.pos)
    }

    fn next(&mut self) -> Option<&'t Token> {
        let tok = self.tokens.get(self.pos);
        if tok.is_some() {
            self.pos += 1;
        }
        tok
    }

    /// 解析节点序列；`in_block` 时遇到 `}` 返回（不消耗，由 [`Self::parse_block_body`] 收尾）。
    fn parse_nodes(&mut self, in_block: bool) -> Result<Vec<Node>, ParseError> {
        let mut nodes: Vec<Node> = Vec::new();
        while let Some(tok) = self.peek() {
            match &tok.kind {
                TokenKind::Text(s) => {
                    push_text(&mut nodes, s);
                    self.pos += 1;
                }
                TokenKind::Model(s) => {
                    self.model = Some(s.clone());
                    self.pos += 1;
                }
                TokenKind::ImplicitExpr(s) => {
                    let node = self.parse_implicit(tok, s)?;
                    nodes.push(node);
                    self.pos += 1;
                }
                TokenKind::ExplicitExpr(s) => {
                    let e = expr::parse(s, tok.content_line, tok.content_col)?;
                    nodes.push(Node::Write(e));
                    self.pos += 1;
                }
                TokenKind::If(cond) => {
                    let node = self.parse_if(tok, cond)?;
                    nodes.push(node);
                }
                TokenKind::ForEach(head) => {
                    let node = self.parse_foreach(tok, head)?;
                    nodes.push(node);
                }
                TokenKind::Section(name) => {
                    let node = self.parse_section(name);
                    nodes.push(node?);
                }
                TokenKind::RenderBody => {
                    nodes.push(Node::RenderBody);
                    self.pos += 1;
                }
                TokenKind::AwaitSection(inner) => {
                    nodes.push(parse_await_section(inner, tok)?);
                    self.pos += 1;
                }
                TokenKind::AwaitPartial(inner) => {
                    nodes.push(parse_await_partial(inner, tok)?);
                    self.pos += 1;
                }
                TokenKind::Code(s) => {
                    if let Some(node) = parse_code_block(s, tok)? {
                        nodes.push(node);
                    }
                    self.pos += 1;
                }
                TokenKind::ElseIf(_) | TokenKind::Else => {
                    return Err(ParseError::new(
                        tok.line,
                        tok.col,
                        "多余的 else：缺少配对的 @if 块",
                    )
                    .with_hint("子集规约：@if (...) { } else if (...) { } else { }"));
                }
                TokenKind::RightBrace if in_block => return Ok(nodes),
                TokenKind::LeftBrace | TokenKind::RightBrace => {
                    return Err(ParseError::new(tok.line, tok.col, "意外的块边界")
                        .with_hint("内部不一致：请提交模板样本以便排查"));
                }
            }
        }
        Ok(nodes)
    }

    /// 解析隐式表达式：`Raw(...)` / `Html.Raw(...)` 固定模式 → [`Node::Raw`]，其余 → [`Node::Write`]。
    fn parse_implicit(&self, tok: &'t Token, raw: &str) -> Result<Node, ParseError> {
        if let Some(found) = scan_fixed_call(raw, "Raw(") {
            return finish_raw(tok, "Raw", found, 4);
        }
        if let Some(found) = scan_fixed_call(raw, "Html.Raw(") {
            return finish_raw(tok, "Html.Raw", found, 9);
        }
        let e = expr::parse(raw, tok.content_line, tok.content_col)?;
        Ok(Node::Write(e))
    }

    /// 解析 `@if` 链（含 `else if` / `else`；结构错误在此报错）。
    fn parse_if(&mut self, tok: &'t Token, cond: &str) -> Result<Node, ParseError> {
        let cond_expr = expr::parse(cond, tok.content_line, tok.content_col)?;
        self.pos += 1; // 消耗 If
        let body = self.parse_block_body()?;
        let mut branches = vec![(cond_expr, body)];
        let mut else_: Option<Vec<Node>> = None;
        while let Some(next) = self.peek() {
            match &next.kind {
                TokenKind::ElseIf(cond2) => {
                    if else_.is_some() {
                        return Err(ParseError::new(
                            next.line,
                            next.col,
                            "else 之后不能再出现 else if",
                        )
                        .with_hint("把该分支并入前面的条件（&& / ||），或改用嵌套 @if"));
                    }
                    let e2 = expr::parse(cond2, next.content_line, next.content_col)?;
                    self.pos += 1;
                    let body2 = self.parse_block_body()?;
                    branches.push((e2, body2));
                }
                TokenKind::Else => {
                    if else_.is_some() {
                        return Err(ParseError::new(
                            next.line,
                            next.col,
                            "多余的 else（每个 @if 最多一个 else）",
                        )
                        .with_hint("删除多余的 else 块"));
                    }
                    self.pos += 1;
                    let body3 = self.parse_block_body()?;
                    else_ = Some(body3);
                }
                _ => break,
            }
        }
        Ok(Node::If { branches, else_ })
    }

    /// 解析 `@foreach (var x in expr) { }`。
    fn parse_foreach(&mut self, tok: &'t Token, head: &str) -> Result<Node, ParseError> {
        let (var, iter) = split_foreach_head(head, tok.content_line, tok.content_col)?;
        self.pos += 1; // 消耗 ForEach
        let body = self.parse_block_body()?;
        Ok(Node::ForEach { var, iter, body })
    }

    /// 解析 `@section 名称 { }`（分区体在渲染期定义处立即求值并收集）。
    fn parse_section(&mut self, name: &'t str) -> Result<Node, ParseError> {
        self.pos += 1; // 消耗 Section
        let body = self.parse_block_body()?;
        Ok(Node::Section {
            name: name.to_string(),
            body,
        })
    }

    /// 消耗 `{`、解析块体节点、消耗 `}`。
    fn parse_block_body(&mut self) -> Result<Vec<Node>, ParseError> {
        let Some(tok) = self.next() else {
            return Err(
                ParseError::new(1, 1, "内部不一致：块体缺失").with_hint("请提交模板样本以便排查")
            );
        };
        if !matches!(tok.kind, TokenKind::LeftBrace) {
            return Err(ParseError::new(tok.line, tok.col, "内部不一致：块起始缺失")
                .with_hint("请提交模板样本以便排查"));
        }
        self.depth += 1;
        if self.depth > MAX_NODE_DEPTH {
            return Err(ParseError::new(tok.line, tok.col, "模板嵌套过深")
                .with_hint(format!("上限 {MAX_NODE_DEPTH} 层（@if / @foreach 嵌套）")));
        }
        let nodes = self.parse_nodes(true)?;
        self.depth -= 1;
        // 消耗 RightBrace（lexer 保证与 LeftBrace 配对）
        if let Some(rb) = self.next() {
            if !matches!(rb.kind, TokenKind::RightBrace) {
                return Err(ParseError::new(rb.line, rb.col, "内部不一致：块结束缺失")
                    .with_hint("请提交模板样本以便排查"));
            }
        }
        Ok(nodes)
    }
}

// ————— 固定模式辅助 —————

/// `Raw(...)` / `Html.Raw(...)` 固定调用的扫描结果。
enum FixedCall {
    /// 前缀匹配且首个调用括号一直延伸到文本末尾：括号内原文
    Inner(String),
    /// 前缀匹配但调用之后还有链式内容（如 `@Raw(x).Foo`）
    Chained,
}

/// 匹配 `前缀(...)` 且首个右括号恰好落在文本末尾。
fn scan_fixed_call(s: &str, prefix: &str) -> Option<FixedCall> {
    let rest = s.strip_prefix(prefix)?;
    let chars: Vec<char> = rest.chars().collect();
    let mut depth = 1usize; // prefix 内含 '('
    let mut i = 0usize;
    while i < chars.len() {
        match chars[i] {
            '"' => {
                // 跳过字符串（转义 \\ 与 \")
                i += 1;
                while i < chars.len() {
                    if chars[i] == '\\' {
                        i += 2;
                    } else if chars[i] == '"' {
                        i += 1;
                        break;
                    } else {
                        i += 1;
                    }
                }
            }
            '(' => {
                depth += 1;
                i += 1;
            }
            ')' => {
                depth -= 1;
                if depth == 0 {
                    if i == chars.len() - 1 {
                        return Some(FixedCall::Inner(chars[..i].iter().collect()));
                    }
                    return Some(FixedCall::Chained);
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    // lexer 已保证括号平衡，此处仅作防御
    Some(FixedCall::Chained)
}

/// 由固定调用扫描结果构建 [`Node::Raw`]。
fn finish_raw(
    tok: &Token,
    name: &str,
    found: FixedCall,
    prefix_len: usize,
) -> Result<Node, ParseError> {
    match found {
        FixedCall::Inner(inner) => {
            let e = expr::parse(&inner, tok.content_line, tok.content_col + prefix_len)?;
            Ok(Node::Raw(e))
        }
        FixedCall::Chained => Err(ParseError::new(
            tok.content_line,
            tok.content_col,
            format!("不支持对 {name}(...) 的链式访问（仅支持直接输出）"),
        )
        .with_hint("请把处理逻辑移入页面模型，模板内保持 @Raw(表达式) 原样输出")),
    }
}

// ————— 代码块语句 —————

/// 解析 `@{ ... }` 块内原文；全部为空白 / 注释时返回 `None`（不产出节点）。
fn parse_code_block(source: &str, tok: &Token) -> Result<Option<Node>, ParseError> {
    let segments = split_statements(source, tok.content_line, tok.content_col)?;
    let mut stmts = Vec::new();
    for (seg, (l, c)) in segments {
        if let Some(stmt) = parse_statement(&seg, l, c)? {
            stmts.push(stmt);
        }
    }
    if stmts.is_empty() {
        return Ok(None);
    }
    Ok(Some(Node::Code(stmts)))
}

/// 语句文本与（行、列）起始位置。
type StatementChunk = (String, (usize, usize));

/// 按顶层 `;` 拆分语句（字符串 / 字符 / 注释内的分号不拆分）。
///
/// 返回 `(语句文本, 起始位置)` 列表；尾部非空且无 `;` 时报「缺少 ;」。
fn split_statements(
    source: &str,
    line: usize,
    col: usize,
) -> Result<Vec<StatementChunk>, ParseError> {
    let mut sc = Scanner::new(source, line, col);
    let mut segments = Vec::new();
    let mut seg_start_idx = 0usize;
    let mut seg_start_pos = (line, col);
    let mut has_code = false;
    while let Some(c) = sc.peek() {
        match c {
            ';' => {
                let seg: String = sc.chars[seg_start_idx..sc.pos].iter().collect();
                segments.push((seg, seg_start_pos));
                sc.bump(); // ';'
                seg_start_idx = sc.pos;
                seg_start_pos = sc.here();
                has_code = false;
            }
            '"' => {
                sc.skip_quoted('"', "字符串字面量")?;
                has_code = true;
            }
            '\'' => {
                sc.skip_quoted('\'', "字符字面量")?;
                has_code = true;
            }
            '/' if sc.peek_at(1) == Some('/') => {
                while matches!(sc.peek(), Some(c) if c != '\n') {
                    sc.bump();
                }
            }
            '/' if sc.peek_at(1) == Some('*') => {
                sc.skip_block_comment()?;
            }
            c if c.is_whitespace() => {
                sc.bump();
            }
            _ => {
                sc.bump();
                has_code = true;
            }
        }
    }
    if has_code {
        return Err(sc
            .error("代码块语句缺少 ;")
            .with_hint("每条声明都应以 ; 结束，如：var x = 1;"));
    }
    Ok(segments)
}

/// 解析单条语句（段文本已剥离 `;`）；全空白 / 注释返回 `None`。
fn parse_statement(source: &str, line: usize, col: usize) -> Result<Option<Stmt>, ParseError> {
    let mut sc = Scanner::new(source, line, col);
    sc.skip_trivia()?;
    if sc.peek().is_none() {
        return Ok(None);
    }
    if !sc.try_consume_word("var") {
        // 赋值语句：`Layout = 表达式;`（F008；仅限 Layout）
        if matches!(sc.peek(), Some(c) if is_ident_start(c)) {
            let name = sc.scan_identifier();
            sc.skip_trivia()?;
            if sc.peek() == Some('=') && sc.peek_at(1) != Some('=') {
                sc.bump(); // '='
                let rest = sc.rest();
                let base = sc.here();
                if rest.trim().is_empty() {
                    return Err(sc.error("= 后缺少表达式"));
                }
                if name != "Layout" {
                    return Err(sc
                        .error(format!("不支持赋值：{name}（仅 Layout）"))
                        .with_hint("子集规约：代码块支持 var 声明与 Layout = \"布局名\"; 赋值"));
                }
                let value = expr::parse(&rest, base.0, base.1)?;
                return Ok(Some(Stmt::Assign { name, value }));
            }
        }
        return Err(sc
            .error("代码块仅支持 var 名称 = 表达式; 声明与 Layout = 表达式; 赋值（子集规约 v0.1）")
            .with_hint("复杂逻辑请移入页面模型"));
    }
    sc.skip_trivia()?;
    if !matches!(sc.peek(), Some(c) if is_ident_start(c)) {
        return Err(sc.error("var 后缺少变量名"));
    }
    let name = sc.scan_identifier();
    sc.skip_trivia()?;
    if sc.peek() != Some('=') {
        return Err(sc
            .error("变量声明缺少 =")
            .with_hint("形如：var name = 表达式;"));
    }
    sc.bump(); // '='
    let rest = sc.rest();
    let base = sc.here();
    if rest.trim().is_empty() {
        return Err(sc.error("= 后缺少表达式"));
    }
    let value = expr::parse(&rest, base.0, base.1)?;
    Ok(Some(Stmt::VarDecl { name, value }))
}

/// 解析 `@await RenderSectionAsync("名称", required)` 固定模式（F008）。
fn parse_await_section(inner: &str, tok: &Token) -> Result<Node, ParseError> {
    let hint = "固定模式：@await RenderSectionAsync(\"Side\", false)";
    let Some((name_part, required_part)) = split_top_level_args(inner) else {
        return Err(
            ParseError::new(tok.line, tok.col, "RenderSectionAsync 需要两个参数").with_hint(hint),
        );
    };
    let name = parse_string_arg(name_part.trim(), tok, "分区名")?;
    let required = match required_part.trim() {
        "true" => true,
        "false" => false,
        _ => {
            return Err(ParseError::new(
                tok.line,
                tok.col,
                "RenderSectionAsync 第二个参数需为 true/false",
            )
            .with_hint(hint));
        }
    };
    Ok(Node::RenderSection { name, required })
}

/// 解析 `@await Html.PartialAsync("名称", 模型表达式)` 固定模式（F009）。
fn parse_await_partial(inner: &str, tok: &Token) -> Result<Node, ParseError> {
    let hint = "固定模式：@await Html.PartialAsync(\"Name\", model)";
    let Some((name_part, model_part)) = split_top_level_args(inner) else {
        return Err(
            ParseError::new(tok.line, tok.col, "PartialAsync 需要两个参数").with_hint(hint),
        );
    };
    let name = parse_string_arg(name_part.trim(), tok, "Partial 名称")?;
    let model_src = model_part.trim();
    if model_src.is_empty() {
        return Err(
            ParseError::new(tok.line, tok.col, "PartialAsync 缺少模型参数").with_hint(hint),
        );
    }
    let model = expr::parse(model_src, tok.content_line, tok.content_col)?;
    Ok(Node::Partial { name, model })
}

/// 解析字符串字面量参数（如 `"Side"`），返回其值。
fn parse_string_arg(src: &str, tok: &Token, what: &str) -> Result<String, ParseError> {
    match expr::parse(src, tok.content_line, tok.content_col)? {
        Expr::Lit(expr::Literal::Str(s)) => Ok(s.to_string()),
        _ => Err(ParseError::new(
            tok.line,
            tok.col,
            format!("{what}需为字符串字面量（双引号）"),
        )
        .with_hint("例如：\"Side\"")),
    }
}

/// 顶层逗号拆分（字符串/字符/括号内不拆分）；固定模式两个参数专用。
fn split_top_level_args(s: &str) -> Option<(String, String)> {
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut esc = false;
    for (i, c) in s.char_indices() {
        if let Some(q) = quote {
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => quote = Some(c),
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            ',' if depth == 0 => return Some((s[..i].to_string(), s[i + 1..].to_string())),
            _ => {}
        }
    }
    None
}

/// 拆分 `@foreach` 头部：`var 名称 in 表达式`。
fn split_foreach_head(head: &str, line: usize, col: usize) -> Result<(String, Expr), ParseError> {
    let mut sc = Scanner::new(head, line, col);
    sc.skip_trivia()?;
    if !sc.try_consume_word("var") {
        return Err(sc
            .error("foreach 仅支持 var 名称 in 集合（子集规约 v0.1）")
            .with_hint("例如：@foreach (var s in Model.Sites) { ... }"));
    }
    sc.skip_trivia()?;
    if !matches!(sc.peek(), Some(c) if is_ident_start(c)) {
        return Err(sc.error("var 后缺少循环变量名"));
    }
    let var = sc.scan_identifier();
    sc.skip_trivia()?;
    if !sc.try_consume_word("in") {
        return Err(sc
            .error("foreach 缺少 in 关键字")
            .with_hint("例如：@foreach (var s in Model.Sites) { ... }"));
    }
    sc.skip_trivia()?;
    let rest = sc.rest();
    let base = sc.here();
    if rest.trim().is_empty() {
        return Err(sc.error("in 后缺少迭代集合表达式"));
    }
    let iter = expr::parse(&rest, base.0, base.1)?;
    Ok((var, iter))
}

// ————— 通用工具 —————

/// 合并相邻文本节点（lexer 因注释透明化可能产出相邻 Text 片段）。
fn push_text(nodes: &mut Vec<Node>, s: &str) {
    if s.is_empty() {
        return;
    }
    if let Some(Node::Text(prev)) = nodes.last_mut() {
        prev.push_str(s);
        return;
    }
    nodes.push(Node::Text(s.to_string()));
}

/// 带位置追踪的字符游标（解析模板内嵌代码 / 头部时使用）。
struct Scanner {
    chars: Vec<char>,
    pos: usize,
    line: usize,
    col: usize,
}

impl Scanner {
    fn new(source: &str, line: usize, col: usize) -> Self {
        Self {
            chars: source.chars().collect(),
            pos: 0,
            line,
            col,
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<char> {
        self.chars.get(self.pos + offset).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.chars.get(self.pos).copied()?;
        self.pos += 1;
        if c == '\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        Some(c)
    }

    fn here(&self) -> (usize, usize) {
        (self.line, self.col)
    }

    fn error(&self, message: impl Into<String>) -> ParseError {
        ParseError::new(self.line, self.col, message)
    }

    /// 剩余文本。
    fn rest(&self) -> String {
        self.chars[self.pos..].iter().collect()
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(c) if c.is_whitespace()) {
            self.bump();
        }
    }

    /// 跳过空白与 C# 注释（`//`、`/* */`；块注释未闭合时报错）。
    fn skip_trivia(&mut self) -> Result<(), ParseError> {
        loop {
            self.skip_whitespace();
            if self.peek() == Some('/') && self.peek_at(1) == Some('/') {
                while matches!(self.peek(), Some(c) if c != '\n') {
                    self.bump();
                }
            } else if self.peek() == Some('/') && self.peek_at(1) == Some('*') {
                self.skip_block_comment()?;
            } else {
                break;
            }
        }
        Ok(())
    }

    /// 跳过块注释（当前位置为 `/*`）。
    fn skip_block_comment(&mut self) -> Result<(), ParseError> {
        let (l, c) = self.here();
        self.bump(); // '/'
        self.bump(); // '*'
        loop {
            match self.bump() {
                None => {
                    return Err(ParseError::new(l, c, "块注释未闭合").with_hint("补上 */"));
                }
                Some('*') if self.peek() == Some('/') => {
                    self.bump();
                    break;
                }
                Some(_) => {}
            }
        }
        Ok(())
    }

    /// 跳过引号包裹的字面量（当前位置为开引号；含 `\` 转义保护）。
    fn skip_quoted(&mut self, quote: char, what: &str) -> Result<(), ParseError> {
        let (l, c) = self.here();
        self.bump(); // 开引号
        loop {
            match self.bump() {
                None => {
                    return Err(ParseError::new(l, c, format!("{what}未闭合")));
                }
                Some('\\') => {
                    if self.bump().is_none() {
                        return Err(ParseError::new(l, c, format!("{what}未闭合")));
                    }
                }
                Some(ch) if ch == quote => return Ok(()),
                Some(_) => {}
            }
        }
    }

    /// 尝试消耗完整单词（后随非标识符字符或串尾）；失败时不移动游标。
    fn try_consume_word(&mut self, word: &str) -> bool {
        let mut i = 0usize;
        for wc in word.chars() {
            if self.peek_at(i) != Some(wc) {
                return false;
            }
            i += 1;
        }
        if matches!(self.peek_at(i), Some(c) if is_ident_continue(c)) {
            return false;
        }
        for _ in word.chars() {
            self.bump();
        }
        true
    }

    fn scan_identifier(&mut self) -> String {
        let mut s = String::new();
        while matches!(self.peek(), Some(c) if is_ident_continue(c)) {
            s.push(self.bump().unwrap());
        }
        s
    }
}

/// 是否标识符起始字符（与 `lexer` / `expr` 保持一致的宽口径）。
fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

/// 是否标识符延续字符。
fn is_ident_continue(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::razor::expr::Seg;

    fn parse_t(src: &str) -> Template {
        Template::parse(src).unwrap()
    }

    fn nodes(src: &str) -> Vec<Node> {
        parse_t(src).nodes
    }

    fn err(src: &str) -> ParseError {
        Template::parse(src).expect_err("应报错")
    }

    /// 便捷构造期望表达式（表达式解析本身已在 expr 模块单测覆盖）。
    fn ex(src: &str) -> Expr {
        crate::razor::expr::parse(src, 1, 1).unwrap()
    }

    fn p(name: &str) -> Expr {
        Expr::Path(vec![Seg::Prop(name.into())])
    }

    fn text(s: &str) -> Node {
        Node::Text(s.into())
    }

    // ———— 基础 ————

    #[test]
    fn text_and_write_nodes() {
        assert_eq!(nodes("<p>hi</p>"), vec![text("<p>hi</p>")]);
        assert_eq!(nodes("@Model.Title"), vec![Node::Write(ex("Model.Title"))]);
        assert_eq!(nodes("@(1 + 2)"), vec![Node::Write(ex("1 + 2"))]);
        assert!(nodes("").is_empty());
    }

    #[test]
    fn text_merges_across_comment() {
        assert_eq!(nodes("a@* 注 *@b"), vec![text("ab")]);
    }

    // ———— Raw 固定模式 ————

    #[test]
    fn raw_fixed_patterns() {
        assert_eq!(nodes("@Raw(Model.Html)"), vec![Node::Raw(ex("Model.Html"))]);
        assert_eq!(
            nodes("@Raw((a + b) * c)"),
            vec![Node::Raw(ex("(a + b) * c"))]
        );
        assert_eq!(nodes("@Html.Raw(x)"), vec![Node::Raw(p("x"))]);
    }

    #[test]
    fn raw_chained_access_is_explicit_error() {
        let e = err("@Raw(x).Foo");
        assert!(e.message.contains("链式"));
        assert!(e.hint.is_some());
        let e2 = err("@Html.Raw(x)[0]");
        assert!(e2.message.contains("链式"));
    }

    #[test]
    fn other_method_call_is_explicit_error() {
        let e = err("@Foo(1)");
        assert!(e.message.contains("方法调用"));
    }

    // ———— 语句结构 ————

    #[test]
    fn if_else_chain_ast() {
        assert_eq!(
            nodes("@if (a) { <i>1</i> } else if (b) { <i>2</i> } else { <i>3</i> }"),
            vec![Node::If {
                branches: vec![
                    (p("a"), vec![text(" <i>1</i> ")]),
                    (p("b"), vec![text(" <i>2</i> ")]),
                ],
                else_: Some(vec![text(" <i>3</i> ")]),
            }]
        );
    }

    #[test]
    fn nested_if_ast() {
        assert_eq!(
            nodes("@if (a) { @if (b) { <i>x</i> } }"),
            vec![Node::If {
                branches: vec![(
                    p("a"),
                    vec![Node::If {
                        branches: vec![(p("b"), vec![text(" <i>x</i> ")])],
                        else_: None,
                    }],
                )],
                else_: None,
            }]
        );
    }

    #[test]
    fn foreach_ast() {
        assert_eq!(
            nodes("@foreach (var s in Model.Sites) { @s.Name }"),
            vec![Node::ForEach {
                var: "s".into(),
                iter: ex("Model.Sites"),
                body: vec![Node::Write(ex("s.Name"))],
            }]
        );
    }

    #[test]
    fn foreach_head_errors() {
        assert!(err("@foreach () { }").message.contains("仅支持 var"));
        assert!(err("@foreach (s in Model) { }")
            .message
            .contains("仅支持 var"));
        assert!(err("@foreach (var s Model) { }")
            .message
            .contains("缺少 in"));
        assert!(err("@foreach (var s in) { }")
            .message
            .contains("集合表达式"));
        // "index" 以 "in" 开头：验证 in 单词边界正确
        assert_eq!(
            nodes("@foreach (var index in Model) { }"),
            vec![Node::ForEach {
                var: "index".into(),
                iter: p("Model"),
                body: vec![],
            }]
        );
    }

    #[test]
    fn stray_else_is_error() {
        assert!(err("@if (a) { } else { } else { }")
            .message
            .contains("多余"));
    }

    #[test]
    fn condition_error_positions_are_exact() {
        // "@if (a +) { }"：条件正文 "a +"，'+' 之后（第 9 列）缺少操作数
        let e = err("@if (a +) { }");
        assert_eq!((e.line, e.col), (1, 9));
        assert!(e.message.contains("意外结束"));
        // "@if (a ! b) { }"：条件正文中出现非法的 '!'（第 8 列）
        let e2 = err("@if (a ! b) { }");
        assert_eq!((e2.line, e2.col), (1, 8));
        assert!(e2.message.contains("意外"));
    }

    #[test]
    fn nesting_depth_is_guarded() {
        let src = format!(
            "{}{}",
            "@if (x) {".repeat(MAX_NODE_DEPTH + 2),
            "}".repeat(MAX_NODE_DEPTH + 2)
        );
        let e = Template::parse(&src).unwrap_err();
        assert!(e.message.contains("嵌套过深"));
    }

    // ———— 代码块 ————

    #[test]
    fn code_block_var_decls() {
        assert_eq!(
            nodes("@{ var x = 1; var y = \"a\"; }"),
            vec![Node::Code(vec![
                Stmt::VarDecl {
                    name: "x".into(),
                    value: ex("1"),
                },
                Stmt::VarDecl {
                    name: "y".into(),
                    value: ex("\"a\""),
                },
            ])]
        );
    }

    #[test]
    fn code_block_empty_or_comment_only_is_skipped() {
        assert!(nodes("@{}").is_empty());
        assert!(nodes("@{ }").is_empty());
        assert!(nodes("@{ /* 注释 */ }").is_empty());
        assert!(nodes("@{// 行注释\n}").is_empty());
    }

    #[test]
    fn code_block_statement_errors() {
        assert!(err("@{ x = 1; }").message.contains("不支持赋值"));
        assert!(err("@{ var = 1; }").message.contains("缺少变量名"));
        assert!(err("@{ var x 1; }").message.contains("缺少 ="));
        assert!(err("@{ var x = 1 }").message.contains("缺少 ;"));
        assert!(err("@{ var x = ; }").message.contains("缺少表达式"));
    }

    #[test]
    fn code_block_protects_string_and_comments() {
        assert_eq!(
            nodes("@{ var x = \"a;b\"; }"),
            vec![Node::Code(vec![Stmt::VarDecl {
                name: "x".into(),
                value: ex("\"a;b\""),
            }])]
        );
        let ns = nodes("@{ var x = 1; // 注释\n var y = /* c */ 2; }");
        match &ns[..] {
            [Node::Code(stmts)] => assert_eq!(stmts.len(), 2),
            other => panic!("期望单个 Code 节点，实际 {other:?}"),
        }
    }

    #[test]
    fn code_block_inside_if_body() {
        assert_eq!(
            nodes("@if (x) { @{ var y = 1; } @y }"),
            vec![Node::If {
                branches: vec![(
                    p("x"),
                    vec![
                        Node::Code(vec![Stmt::VarDecl {
                            name: "y".into(),
                            value: ex("1"),
                        }]),
                        Node::Write(p("y")),
                    ],
                )],
                else_: None,
            }]
        );
    }

    #[test]
    fn unicode_identifiers_work() {
        assert_eq!(
            nodes("@{ var 站点 = 1; } @站点"),
            vec![
                Node::Code(vec![Stmt::VarDecl {
                    name: "站点".into(),
                    value: ex("1"),
                }]),
                text(" "),
                Node::Write(p("站点")),
            ]
        );
    }

    // ———— @model ————

    #[test]
    fn model_is_recorded() {
        let t = parse_t("@model MyApp.SiteModel\n<p>hi</p>");
        assert_eq!(t.model.as_deref(), Some("MyApp.SiteModel"));
        assert_eq!(t.nodes, vec![text("\n<p>hi</p>")]);
    }
}
