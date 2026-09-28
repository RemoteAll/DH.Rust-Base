//! Razor 子集模板引擎：混合扫描器。
//!
//! 将 `.cshtml` 源码切分为词法片段流（[`Token`]），供 `parser` 构建 AST。
//! 覆盖子集规约 v0.1（见 `Doc/Razor子集模板引擎架构.md`）中的全部词法形态：
//!
//! - 文本与代码切换：`@`、`@@`（字面 `@`）、`@* ... *@`（注释，丢弃）；
//! - 表达式：隐式 `@Model.X`（仅吸收 标识符 / `.` / `[...]` / `(...)`，遇运算符即停，
//!   对齐 Razor 隐式表达式行为）与显式 `@(表达式)`；
//! - 语句结构：`@if (cond) { } else if (cond) { } else { }`、
//!   `@foreach (var x in expr) { }`、`@{ ... }`；块边界输出 [`TokenKind::LeftBrace`] /
//!   [`TokenKind::RightBrace`]；
//! - 指令：`@model T`（记录原文，不做强类型校验）；
//! - 子集外语法的显式报错（`@using`、`@switch`、`@while`、`@for`、`@await`、`@section` 等，
//!   错误含行列号与「建议写法」，对齐「杜绝静默行为不一致」原则）。
//!
//! # 块边界约定
//! `@if` / `@foreach` 的块体由嵌套模板扫描：body 中**平衡**的花括号视为字面文本
//! （`<style>a { color: red }</style>` 等 CSS/JS 场景），未配对的 `}` 结束当前块，
//! 与 ASP.NET Core Razor 对块内孤立 `}` 的行为一致。
//! 块内代码（`@{ }`、`@(...)`、条件括号）的字符串 / 字符 / 注释中的括号不参与平衡。
//!
//! # 位置约定
//! 行列均 1 基、按字符计；`@` 引入的片段以 `@` 的位置为准。

use crate::razor::error::ParseError;

// ————— 词法片段 —————

/// 词法片段（含位置，供解析错误定位）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    /// 片段类型
    pub kind: TokenKind,
    /// 行号（1 基）
    pub line: usize,
    /// 列号（1 基，按字符计）
    pub col: usize,
}

impl Token {
    fn new(kind: TokenKind, line: usize, col: usize) -> Self {
        Self { kind, line, col }
    }
}

/// 词法片段类型。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TokenKind {
    /// 纯 HTML 文本（`@@` 已还原为字面 `@`）
    Text(String),
    /// 隐式表达式 `@expr` 的原文（如 `Model.Name`、`Raw(Model.Html)`、`Html.Raw(x)`）
    ImplicitExpr(String),
    /// 显式表达式 `@(expr)` 的括号内原文
    ExplicitExpr(String),
    /// `@if (cond)` 的条件原文
    If(String),
    /// `else if (cond)` 的条件原文
    ElseIf(String),
    /// `else`
    Else,
    /// `@foreach (...)` 的括号内原文（形如 `var x in Model.Sites`，拆分与校验交由 parser）
    ForEach(String),
    /// `@{ ... }` 的块内原文（仅允许 var 声明序列，语句拆分交由 parser）
    Code(String),
    /// `@model T` 的类型原文
    Model(String),
    /// 块开始 `{`
    LeftBrace,
    /// 块结束 `}`
    RightBrace,
}

// ————— 扫描入口 —————

/// 扫描模板源码，产出词法片段流。
///
/// 子集外语法在扫描期显式报错（[`ParseError`] 含行列与建议）。
pub fn tokenize(source: &str) -> Result<Vec<Token>, ParseError> {
    let mut lexer = Lexer {
        chars: source.chars().collect(),
        pos: 0,
        line: 1,
        col: 1,
    };
    let mut tokens = Vec::new();
    lexer.scan_template(&mut tokens, false)?;
    Ok(tokens)
}

// ————— 扫描器 —————

/// 扫描位置（用于 `else` 前瞻失败后的回退）。
type SavePoint = (usize, usize, usize);

/// 混合扫描器：以字符流方式在 HTML 文本与 Razor 结构之间切换。
struct Lexer {
    chars: Vec<char>,
    pos: usize,
    line: usize,
    col: usize,
}

impl Lexer {
    // ———— 基础游标 ————

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

    fn save(&self) -> SavePoint {
        (self.pos, self.line, self.col)
    }

    fn restore(&mut self, save: SavePoint) {
        self.pos = save.0;
        self.line = save.1;
        self.col = save.2;
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(c) if c.is_whitespace()) {
            self.bump();
        }
    }

    /// 把字面字符并入当前文本缓冲（首次并入时记录缓冲起点，保证 Text 片段定位准确）。
    fn push_text(&mut self, text: &mut String, text_pos: &mut (usize, usize), c: char) {
        if text.is_empty() {
            *text_pos = (self.line, self.col);
        }
        text.push(c);
        self.bump();
    }

    // ———— 模板与文本 ————

    /// 扫描模板内容（顶层或块体）。
    ///
    /// `stop_at_brace` 为 `true` 时表示当前处于 `@if` / `@foreach` 块体：
    /// 遇到平衡计数归零的 `}` 即返回（不消耗该 `}`，由调用者处理并产出 `RightBrace`）；
    /// body 中平衡的花括号按字面文本处理。
    fn scan_template(
        &mut self,
        tokens: &mut Vec<Token>,
        stop_at_brace: bool,
    ) -> Result<(), ParseError> {
        let mut text = String::new();
        let mut text_pos = (self.line, self.col);
        let mut brace_depth = 0usize;
        loop {
            let Some(c) = self.peek() else {
                if stop_at_brace {
                    flush_text(&mut text, text_pos.0, text_pos.1, tokens);
                    return Err(
                        ParseError::new(self.line, self.col, "块未闭合：缺少匹配的 }")
                            .with_hint("检查 @if / @foreach / @{ } 的花括号是否成对"),
                    );
                }
                break;
            };
            match c {
                '{' if stop_at_brace => {
                    brace_depth += 1;
                    self.push_text(&mut text, &mut text_pos, c);
                }
                '}' if stop_at_brace => {
                    if brace_depth > 0 {
                        brace_depth -= 1;
                        self.push_text(&mut text, &mut text_pos, c);
                    } else {
                        break;
                    }
                }
                '@' if self.peek_at(1) == Some('@') => {
                    // 字面 @：并入文本
                    self.push_text(&mut text, &mut text_pos, '@');
                    self.bump();
                }
                '@' if self.peek_at(1) == Some('*') => {
                    // 注释：透明丢弃（不打断文本缓冲，保证 "a@* *@b" 合并为 "ab"）
                    self.scan_comment(self.line, self.col)?;
                }
                '@' => {
                    flush_text(&mut text, text_pos.0, text_pos.1, tokens);
                    self.scan_at(tokens)?;
                }
                _ => {
                    self.push_text(&mut text, &mut text_pos, c);
                }
            }
        }
        flush_text(&mut text, text_pos.0, text_pos.1, tokens);
        Ok(())
    }

    // ———— `@` 结构 ————

    /// 处理 `@` 后的内容（此时位于 `@` 处）。
    fn scan_at(&mut self, tokens: &mut Vec<Token>) -> Result<(), ParseError> {
        let line = self.line;
        let col = self.col;
        self.bump(); // 消耗 '@'
        let Some(c) = self.peek() else {
            return Err(
                ParseError::new(line, col, "@ 后缺少内容").with_hint("若要输出字面 @ 请使用 @@")
            );
        };
        match c {
            '(' => {
                let inner = self.scan_balanced('(', ')', "表达式", line, col)?;
                tokens.push(Token::new(TokenKind::ExplicitExpr(inner), line, col));
                Ok(())
            }
            '{' => {
                let inner = self.scan_balanced('{', '}', "代码块", line, col)?;
                tokens.push(Token::new(TokenKind::Code(inner), line, col));
                Ok(())
            }
            c if is_ident_start(c) => {
                let word = self.scan_identifier();
                self.scan_directive(tokens, &word, line, col)
            }
            _ => Err(ParseError::new(line, col, format!("不支持的 @ 用法：@{c}"))
                .with_hint("若为字面 @ 请使用 @@；若为表达式请使用 @(表达式)")),
        }
    }

    /// 处理 `@word`：指令 / 语句关键字 / 隐式表达式。
    fn scan_directive(
        &mut self,
        tokens: &mut Vec<Token>,
        word: &str,
        line: usize,
        col: usize,
    ) -> Result<(), ParseError> {
        match word {
            "if" => {
                let cond = self.expect_parenthesized("if", line, col)?;
                tokens.push(Token::new(TokenKind::If(cond), line, col));
                self.scan_block_body("if", line, col, tokens)?;
                self.try_scan_else(tokens)
            }
            "foreach" => {
                let head = self.expect_parenthesized("foreach", line, col)?;
                tokens.push(Token::new(TokenKind::ForEach(head), line, col));
                self.scan_block_body("foreach", line, col, tokens)
            }
            "model" => {
                let mut buf = String::new();
                while !matches!(self.peek(), None | Some('\n') | Some('\r')) {
                    buf.push(self.bump().unwrap());
                }
                let ty = buf.trim();
                if ty.is_empty() {
                    return Err(ParseError::new(line, col, "@model 后缺少类型名")
                        .with_hint("例如：@model MyApp.SiteModel"));
                }
                tokens.push(Token::new(TokenKind::Model(ty.to_string()), line, col));
                Ok(())
            }
            "else" => Err(ParseError::new(line, col, "@else 必须紧跟在 @if 的 } 之后")
                .with_hint("子集规约：@if (...) { } else if (...) { } else { }")),
            "using" | "inject" | "functions" | "helper" | "page" | "namespace" | "implements"
            | "inherits" | "attribute" | "typeparam" => {
                Err(ParseError::new(line, col, format!("不支持 @{word} 指令"))
                    .with_hint("子集规约 v0.1 不含该指令；数据与逻辑请通过页面模型传入"))
            }
            "switch" => Err(ParseError::new(line, col, "不支持 @switch 语句")
                .with_hint("改用 @if / else if 链")),
            "while" => Err(ParseError::new(line, col, "不支持 @while 语句")
                .with_hint("子集仅支持 @foreach；请把数据整理为集合")),
            "for" => Err(ParseError::new(line, col, "不支持 @for 语句")
                .with_hint("子集仅支持 @foreach；固定次数循环请把数据整理为集合")),
            "do" | "try" | "lock" => Err(ParseError::new(
                line,
                col,
                format!("不支持 @{word} 语句"),
            )
            .with_hint("子集规约 v0.1 不含该语句；请改用 @if / @foreach 或把逻辑移入页面模型")),
            "await" => Err(ParseError::new(line, col, "暂不支持 @await")
                .with_hint("@await Html.PartialAsync / RenderSectionAsync 固定模式规划于迭代 3")),
            "section" => Err(ParseError::new(line, col, "暂不支持 @section")
                .with_hint("布局与分区（F008）规划于迭代 3")),
            _ => {
                let expr = self.scan_implicit_expr(word.to_string())?;
                tokens.push(Token::new(TokenKind::ImplicitExpr(expr), line, col));
                Ok(())
            }
        }
    }

    /// 读取 `@if` / `@foreach` / `else if` 的关键字后括号内容（跳过空白）。
    fn expect_parenthesized(
        &mut self,
        what: &str,
        line: usize,
        col: usize,
    ) -> Result<String, ParseError> {
        self.skip_whitespace();
        if self.peek() == Some('(') {
            self.scan_balanced('(', ')', &format!("@{what} 的条件括号"), line, col)
        } else {
            Err(ParseError::new(line, col, format!("@{what} 后缺少 ( )"))
                .with_hint(format!("应写作 @{what} (...) {{ ... }}")))
        }
    }

    /// 期待并扫描 `{ ... }` 块体，产出 `LeftBrace` + body 片段 + `RightBrace`。
    fn scan_block_body(
        &mut self,
        what: &str,
        line: usize,
        col: usize,
        tokens: &mut Vec<Token>,
    ) -> Result<(), ParseError> {
        self.skip_whitespace();
        if self.peek() != Some('{') {
            return Err(
                ParseError::new(line, col, format!("@{what} 后缺少 {{ }} 代码块")).with_hint(
                    "子集规约 v0.1：@if (条件) { ... } / @foreach (var x in 集合) { ... }",
                ),
            );
        }
        let (bl, bc) = (self.line, self.col);
        self.bump(); // '{'
        tokens.push(Token::new(TokenKind::LeftBrace, bl, bc));
        self.scan_template(tokens, true)?;
        // scan_template 在块体未闭合时已报错，此处 peek 必为 '}'
        let (rl, rc) = (self.line, self.col);
        self.bump(); // '}'
        tokens.push(Token::new(TokenKind::RightBrace, rl, rc));
        Ok(())
    }

    /// 块体结束后尝试连接 `else` / `else if` 链。
    ///
    /// 仅当 `}` 之后（跳过空白）紧跟完整单词 `else` 时视为关键字，否则回退为普通文本。
    fn try_scan_else(&mut self, tokens: &mut Vec<Token>) -> Result<(), ParseError> {
        let save = self.save();
        self.skip_whitespace();
        if !self.starts_with_word("else") {
            self.restore(save);
            return Ok(());
        }
        let (el, ec) = (self.line, self.col);
        self.consume_word("else");
        let save2 = self.save();
        self.skip_whitespace();
        if self.starts_with_word("if") {
            self.consume_word("if");
            let cond = self.expect_parenthesized("else if", el, ec)?;
            tokens.push(Token::new(TokenKind::ElseIf(cond), el, ec));
            self.scan_block_body("else if", el, ec, tokens)?;
            // 允许链式 else if ... else
            self.try_scan_else(tokens)
        } else {
            self.restore(save2);
            tokens.push(Token::new(TokenKind::Else, el, ec));
            self.scan_block_body("else", el, ec, tokens)?;
            // 结构错误（多个 else 等）交由 parser 报错，这里继续产出以便诊断
            self.try_scan_else(tokens)
        }
    }

    /// 扫描隐式表达式：从已读出的首标识符继续吸收 `.名称` / `[...]` / `(...)`。
    ///
    /// 与 Razor 一致：遇运算符、空白、HTML 字符即停；`?` 紧跟 `.` 时显式报错
    /// （null 条件运算符为迭代 3 评估项，避免静默差异）。
    fn scan_implicit_expr(&mut self, mut expr: String) -> Result<String, ParseError> {
        loop {
            match self.peek() {
                Some('?') if self.peek_at(1) == Some('.') => {
                    return Err(
                        ParseError::new(self.line, self.col, "不支持 null 条件运算符 ?. ")
                            .with_hint(
                                "规划于迭代 3 评估；可用 @if 判空或 @(表达式 ?? 默认值) 替代",
                            ),
                    );
                }
                Some('.') => {
                    if matches!(self.peek_at(1), Some(c) if is_ident_start(c)) {
                        expr.push(self.bump().unwrap()); // '.'
                        expr.push_str(&self.scan_identifier());
                    } else {
                        // 孤立 '.'：留给文本（如 "@a. 结尾" 的标点场景）
                        break;
                    }
                }
                Some('[') => {
                    let inner = self.scan_balanced('[', ']', "索引器", self.line, self.col)?;
                    expr.push('[');
                    expr.push_str(&inner);
                    expr.push(']');
                }
                Some('(') => {
                    // 方法调用形态（Raw / Html.Raw 由 parser 识别为固定模式；其余由 parser 报错）
                    let inner = self.scan_balanced('(', ')', "方法调用", self.line, self.col)?;
                    expr.push('(');
                    expr.push_str(&inner);
                    expr.push(')');
                }
                _ => break,
            }
        }
        Ok(expr)
    }

    /// 扫描 `@* ... *@` 注释（丢弃，不产出片段）。
    fn scan_comment(&mut self, line: usize, col: usize) -> Result<(), ParseError> {
        self.bump(); // '*'
        loop {
            match self.bump() {
                None => {
                    return Err(ParseError::new(line, col, "@* ... *@ 注释未闭合")
                        .with_hint("补上 *@ 结束注释"));
                }
                Some('*') if self.peek() == Some('@') => {
                    self.bump(); // '@'
                    return Ok(());
                }
                Some(_) => {}
            }
        }
    }

    // ———— 代码区（保护字符串 / 字符 / 注释） ————

    /// 从 `open` 处扫描到与之匹配的 `close`，返回括号内的原文（不含两个边界）。
    ///
    /// 代码区规则：字符串 `"..."`（含 `\"` 转义）、字符 `'...'`（含 `\'` 转义）、
    /// 行注释 `//`、块注释 `/* */` 内部的 `open` / `close` 不参与平衡。
    fn scan_balanced(
        &mut self,
        open: char,
        close: char,
        ctx: &str,
        start_line: usize,
        start_col: usize,
    ) -> Result<String, ParseError> {
        debug_assert_eq!(self.peek(), Some(open));
        self.bump(); // 消耗开括号
        let mut depth = 1usize;
        let mut buf = String::new();
        while let Some(c) = self.peek() {
            if c == '"' {
                buf.push_str(&self.scan_csharp_string(start_line, start_col)?);
            } else if c == '\'' {
                buf.push_str(&self.scan_csharp_char(start_line, start_col)?);
            } else if c == '/' && self.peek_at(1) == Some('/') {
                // 行注释：原样收集到行尾（换行留给主循环，保持行列追踪）
                while let Some(ch) = self.peek() {
                    if ch == '\n' {
                        break;
                    }
                    buf.push(ch);
                    self.bump();
                }
            } else if c == '/' && self.peek_at(1) == Some('*') {
                // 块注释：原样收集到 */
                buf.push(self.bump().unwrap()); // '/'
                buf.push(self.bump().unwrap()); // '*'
                loop {
                    match self.bump() {
                        None => {
                            return Err(ParseError::new(
                                start_line,
                                start_col,
                                format!("{ctx}中的块注释未闭合"),
                            )
                            .with_hint("补上 */"));
                        }
                        Some('*') if self.peek() == Some('/') => {
                            buf.push('*');
                            buf.push(self.bump().unwrap()); // '/'
                            break;
                        }
                        Some(ch) => buf.push(ch),
                    }
                }
            } else if c == open {
                depth += 1;
                buf.push(c);
                self.bump();
            } else if c == close {
                depth -= 1;
                if depth == 0 {
                    self.bump();
                    return Ok(buf);
                }
                buf.push(c);
                self.bump();
            } else {
                buf.push(c);
                self.bump();
            }
        }
        Err(ParseError::new(
            start_line,
            start_col,
            format!("{ctx}未闭合：缺少匹配的 {close}"),
        )
        .with_hint("检查括号是否成对"))
    }

    /// 扫描 C# 字符串字面量（含引号返回；越行报错，对齐 C# 普通字符串语义）。
    fn scan_csharp_string(
        &mut self,
        start_line: usize,
        start_col: usize,
    ) -> Result<String, ParseError> {
        let mut buf = String::new();
        buf.push(self.bump().unwrap()); // 开引号
        loop {
            match self.bump() {
                None => {
                    return Err(ParseError::new(start_line, start_col, "字符串字面量未闭合")
                        .with_hint("补上 \""));
                }
                Some('\\') => {
                    buf.push('\\');
                    match self.bump() {
                        None => {
                            return Err(ParseError::new(
                                start_line,
                                start_col,
                                "字符串字面量未闭合",
                            )
                            .with_hint("补上 \""));
                        }
                        Some(e) => buf.push(e),
                    }
                }
                Some('\n') => {
                    return Err(
                        ParseError::new(start_line, start_col, "字符串字面量不能跨行")
                            .with_hint("子集规约 v0.1 不支持逐字字符串；请改用 \\n 或整理数据"),
                    );
                }
                Some(c) => {
                    buf.push(c);
                    if c == '"' {
                        return Ok(buf);
                    }
                }
            }
        }
    }

    /// 扫描 C# 字符字面量（含引号返回）。
    fn scan_csharp_char(
        &mut self,
        start_line: usize,
        start_col: usize,
    ) -> Result<String, ParseError> {
        let mut buf = String::new();
        buf.push(self.bump().unwrap()); // 开引号
        loop {
            match self.bump() {
                None => {
                    return Err(ParseError::new(start_line, start_col, "字符字面量未闭合")
                        .with_hint("补上 '"));
                }
                Some('\\') => {
                    buf.push('\\');
                    match self.bump() {
                        None => {
                            return Err(ParseError::new(start_line, start_col, "字符字面量未闭合")
                                .with_hint("补上 '"));
                        }
                        Some(e) => buf.push(e),
                    }
                }
                Some('\n') => {
                    return Err(ParseError::new(start_line, start_col, "字符字面量不能跨行"));
                }
                Some(c) => {
                    buf.push(c);
                    if c == '\'' {
                        return Ok(buf);
                    }
                }
            }
        }
    }

    // ———— 单词 ————

    /// 读取一个标识符（当前位置应为标识符起始字符）。
    fn scan_identifier(&mut self) -> String {
        let mut s = String::new();
        while matches!(self.peek(), Some(c) if is_ident_continue(c)) {
            s.push(self.bump().unwrap());
        }
        s
    }

    /// 剩余输入是否以完整单词 `word` 开头（后随非标识符字符或串尾）。
    fn starts_with_word(&self, word: &str) -> bool {
        let mut i = 0usize;
        for wc in word.chars() {
            match self.peek_at(i) {
                Some(c) if c == wc => i += 1,
                _ => return false,
            }
        }
        !matches!(self.peek_at(i), Some(c) if is_ident_continue(c))
    }

    /// 消耗完整单词（调用前须经 [`Self::starts_with_word`] 判定）。
    fn consume_word(&mut self, word: &str) {
        for _ in word.chars() {
            self.bump();
        }
    }
}

// ————— 辅助 —————

/// 是否标识符起始字符（与 C# 一致的宽口径：字母含 Unicode，另加下划线）。
fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

/// 是否标识符延续字符。
fn is_ident_continue(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// 输出文本缓冲（空缓冲不产出片段）。
fn flush_text(text: &mut String, line: usize, col: usize, tokens: &mut Vec<Token>) {
    if !text.is_empty() {
        tokens.push(Token::new(TokenKind::Text(std::mem::take(text)), line, col));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 扫描并只保留片段类型（忽略位置）。
    fn kinds(src: &str) -> Vec<TokenKind> {
        tokenize(src).unwrap().into_iter().map(|t| t.kind).collect()
    }

    /// 期望扫描失败。
    fn err_of(src: &str) -> ParseError {
        tokenize(src).expect_err("应报错")
    }

    // ———— 文本与转义 ————

    #[test]
    fn plain_html_yields_single_text() {
        assert_eq!(
            kinds("<p>你好</p>"),
            vec![TokenKind::Text("<p>你好</p>".into())]
        );
        assert_eq!(kinds(""), Vec::<TokenKind>::new());
    }

    #[test]
    fn double_at_renders_literal_at() {
        assert_eq!(kinds("a@@b"), vec![TokenKind::Text("a@b".into())]);
        // 块体中的 @@
        assert_eq!(
            kinds("@if (x) { a@@b }"),
            vec![
                TokenKind::If("x".into()),
                TokenKind::LeftBrace,
                TokenKind::Text(" a@b ".into()),
                TokenKind::RightBrace,
            ]
        );
    }

    #[test]
    fn comment_is_dropped_and_may_span_lines() {
        assert_eq!(
            kinds("a@* 注释\n  跨行 *@b"),
            vec![TokenKind::Text("ab".into())]
        );
        // 空注释
        assert_eq!(kinds("@**@"), Vec::<TokenKind>::new());
    }

    // ———— 表达式 ————

    #[test]
    fn implicit_expression_takes_path_and_index() {
        assert_eq!(
            kinds("@Model.Sites[0].Name"),
            vec![TokenKind::ImplicitExpr("Model.Sites[0].Name".into())]
        );
        assert_eq!(kinds("@name"), vec![TokenKind::ImplicitExpr("name".into())]);
    }

    #[test]
    fn implicit_expression_stops_at_operators() {
        // 与 Razor 一致：隐式表达式遇运算符即停（完整表达式请用 @( )）
        assert_eq!(
            kinds("@price*1.05"),
            vec![
                TokenKind::ImplicitExpr("price".into()),
                TokenKind::Text("*1.05".into()),
            ]
        );
    }

    #[test]
    fn implicit_expression_keeps_call_and_html_raw_patterns() {
        assert_eq!(
            kinds("@Raw(Model.Html)"),
            vec![TokenKind::ImplicitExpr("Raw(Model.Html)".into())]
        );
        assert_eq!(
            kinds("@Html.Raw(Model.Html)"),
            vec![TokenKind::ImplicitExpr("Html.Raw(Model.Html)".into())]
        );
    }

    #[test]
    fn implicit_keyword_prefix_is_not_keyword() {
        assert_eq!(kinds("@iffy"), vec![TokenKind::ImplicitExpr("iffy".into())]);
        assert_eq!(
            kinds("@switching"),
            vec![TokenKind::ImplicitExpr("switching".into())]
        );
    }

    #[test]
    fn explicit_expression_protects_string_parens() {
        assert_eq!(
            kinds("x@(\"a)b\")y"),
            vec![
                TokenKind::Text("x".into()),
                TokenKind::ExplicitExpr("\"a)b\"".into()),
                TokenKind::Text("y".into()),
            ]
        );
    }

    #[test]
    fn explicit_expression_supports_nested_parens() {
        assert_eq!(
            kinds("@((a + b) * c)"),
            vec![TokenKind::ExplicitExpr("(a + b) * c".into())]
        );
    }

    // ———— 语句结构 ————

    #[test]
    fn if_else_chain_tokens() {
        let t = kinds(
            "@if (Model.Enable) { <b>on</b> } else if (Model.Half) { <i>half</i> } else { off }",
        );
        assert_eq!(
            t,
            vec![
                TokenKind::If("Model.Enable".into()),
                TokenKind::LeftBrace,
                TokenKind::Text(" <b>on</b> ".into()),
                TokenKind::RightBrace,
                TokenKind::ElseIf("Model.Half".into()),
                TokenKind::LeftBrace,
                TokenKind::Text(" <i>half</i> ".into()),
                TokenKind::RightBrace,
                TokenKind::Else,
                TokenKind::LeftBrace,
                TokenKind::Text(" off ".into()),
                TokenKind::RightBrace,
            ]
        );
    }

    #[test]
    fn if_empty_body_and_nested_if() {
        assert_eq!(
            kinds("@if (x) {}"),
            vec![
                TokenKind::If("x".into()),
                TokenKind::LeftBrace,
                TokenKind::RightBrace,
            ]
        );
        // 只有一个空格的块体：空格属于文本
        assert_eq!(
            kinds("@if (x) { }"),
            vec![
                TokenKind::If("x".into()),
                TokenKind::LeftBrace,
                TokenKind::Text(" ".into()),
                TokenKind::RightBrace,
            ]
        );
        let t = kinds("@if (a) { @if (b) { x } else { y } }");
        assert_eq!(
            t,
            vec![
                TokenKind::If("a".into()),
                TokenKind::LeftBrace,
                TokenKind::Text(" ".into()),
                TokenKind::If("b".into()),
                TokenKind::LeftBrace,
                TokenKind::Text(" x ".into()),
                TokenKind::RightBrace,
                TokenKind::Else,
                TokenKind::LeftBrace,
                TokenKind::Text(" y ".into()),
                TokenKind::RightBrace,
                TokenKind::Text(" ".into()),
                TokenKind::RightBrace,
            ]
        );
    }

    #[test]
    fn foreach_head_is_captured_raw() {
        assert_eq!(
            kinds("@foreach (var s in Model.Sites) { @s.Name }"),
            vec![
                TokenKind::ForEach("var s in Model.Sites".into()),
                TokenKind::LeftBrace,
                TokenKind::Text(" ".into()),
                TokenKind::ImplicitExpr("s.Name".into()),
                TokenKind::Text(" ".into()),
                TokenKind::RightBrace,
            ]
        );
    }

    #[test]
    fn code_block_protects_string_brace() {
        assert_eq!(
            kinds("@{ var x = \"}\"; }"),
            vec![TokenKind::Code(" var x = \"}\"; ".into())]
        );
        assert_eq!(kinds("@@{}"), vec![TokenKind::Text("@{}".into())]);
        assert_eq!(kinds("@@@@"), vec![TokenKind::Text("@@".into())]);
    }

    #[test]
    fn balanced_braces_in_body_markup_are_text() {
        // CSS 花括号平衡场景：内层 {} 视为文本，不结束块
        let t = kinds("@if (true) { <style>a { color: red }</style> }");
        assert_eq!(t.len(), 4);
        match &t[2] {
            TokenKind::Text(s) => assert_eq!(s, " <style>a { color: red }</style> "),
            other => panic!("期望 Text，实际 {other:?}"),
        }
    }

    #[test]
    fn else_word_boundary_respected() {
        // "elsewhere" 不是 else 关键字，回退为文本
        let t = kinds("@if (x) { a } elsewhere!");
        assert_eq!(
            t,
            vec![
                TokenKind::If("x".into()),
                TokenKind::LeftBrace,
                TokenKind::Text(" a ".into()),
                TokenKind::RightBrace,
                TokenKind::Text(" elsewhere!".into()),
            ]
        );
    }

    #[test]
    fn model_directive_recorded() {
        assert_eq!(
            kinds("@model MyApp.SiteModel\n<p>hi</p>"),
            vec![
                TokenKind::Model("MyApp.SiteModel".into()),
                TokenKind::Text("\n<p>hi</p>".into()),
            ]
        );
    }

    // ———— 位置 ————

    #[test]
    fn token_positions_track_lines_and_columns() {
        let toks = tokenize("<p>a</p>\n@if (x) {\n  @Model.Name\n}").unwrap();
        assert_eq!((toks[1].line, toks[1].col), (2, 1)); // @if
        assert_eq!(
            (toks[2].kind.clone(), toks[2].line, toks[2].col),
            (TokenKind::LeftBrace, 2, 9)
        );
        let expr = toks
            .iter()
            .find(|t| matches!(t.kind, TokenKind::ImplicitExpr(_)))
            .unwrap();
        assert_eq!((expr.line, expr.col), (3, 3));
    }

    // ———— 负向（子集外显式报错） ————

    #[test]
    fn unsupported_directives_report_position_and_hint() {
        let e = err_of("line1\n@using System;\n");
        assert_eq!((e.line, e.col), (2, 1));
        assert!(e.message.contains("不支持") && e.message.contains("using"));
        assert!(e.hint.is_some());

        for (src, needle) in [
            ("@switch (x) { }", "switch"),
            ("@while (true) { }", "while"),
            ("@for (var i = 0; i < 3; i++) { }", "for"),
            ("@await Html.PartialAsync(\"x\")", "await"),
            ("@section Head { }", "section"),
            ("@inject IFoo Foo", "inject"),
            ("@functions { }", "functions"),
        ] {
            let e = err_of(src);
            assert!(e.message.contains(needle), "message={}", e.message);
            assert!(e.hint.is_some(), "src={src}");
        }
    }

    #[test]
    fn unterminated_constructs_error() {
        assert!(err_of("@* 未闭合").message.contains("注释未闭合"));
        assert!(err_of("@(a + b").message.contains("表达式未闭合"));
        assert!(err_of("@{ var x = 1;").message.contains("代码块未闭合"));
        assert!(err_of("@if (x) { <p>").message.contains("块未闭合"));
        assert!(err_of("@( \"a ").message.contains("字符串字面量"));
    }

    #[test]
    fn dangling_at_and_bad_at_usage_error() {
        assert!(err_of("abc@").message.contains("缺少内容"));
        assert!(err_of("abc@ x").message.contains("不支持的 @ 用法"));
        assert!(err_of("@123").message.contains("不支持的 @ 用法"));
        assert!(err_of("@:line").message.contains("不支持的 @ 用法"));
    }

    #[test]
    fn if_and_else_require_braces() {
        assert!(err_of("@if (x) <p>hi</p>").message.contains("代码块"));
        assert!(err_of("@if x { }").message.contains("缺少 ( )"));
        assert!(err_of("@if (x) { a } else <p>b</p>")
            .message
            .contains("else"));
        assert!(err_of("@else { }").message.contains("else"));
    }

    #[test]
    fn null_conditional_operator_is_explicit_error() {
        let e = err_of("@Model?.Name");
        assert!(e.message.contains("?."));
        assert!(e.hint.is_some());
    }

    #[test]
    fn string_literal_cannot_span_lines() {
        assert!(err_of("@{ var s = \"a\nb\"; }")
            .message
            .contains("不能跨行"));
    }
}
