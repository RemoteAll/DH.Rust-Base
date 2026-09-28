//! Razor 子集模板引擎：表达式子集解析（递归下降）。
//!
//! 覆盖子集规约 v0.1（见 `Doc/Razor子集模板引擎架构.md`「3.2 子集规约 v0.1」）：
//!
//! - 字面量：字符串（双引号 + 转义）、整数、浮点、`true` / `false` / `null`；
//! - 路径：`A.B`、`A[i]`；v0 不支持自由方法调用（`parser` 会先行识别
//!   `Raw(...)` / `Html.Raw(...)` 固定模式，其余调用在此显式报错）；
//! - 运算符与优先级（对齐 C#，从高到低）：
//!   `! -`（单目） > `* / %` > `+ -` > `< > <= >=` > `== !=` > `&&` > `||` > `??` >
//!   `?:`（右结合）。
//! - 表达式内允许 C# 注释（`//`、`/* */`）作为空白跳过（未闭合由「意外结束」兜底）。
//!
//! 解析入口为 [`parse`]；产物 [`Expr`] 树由 `parser` 组装进模板 AST、由 `render` 求值。
//! 错误带行列号；位置基准（首字符的 line/col）由调用方传入，多行表达式按换行推进。

use std::fmt;

use crate::razor::error::ParseError;

/// 表达式嵌套深度上限（防极端输入的栈溢出）。
const MAX_DEPTH: usize = 64;

// ————— AST —————

/// 表达式节点（对齐架构文档 2.2 `Expr`）。
#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    /// 字面量
    Lit(Literal),
    /// 路径：`Model.Name`、`Items[0].Title`（首段必为属性名）
    Path(Vec<Seg>),
    /// 单目运算
    Unary(UnOp, Box<Expr>),
    /// 二元运算
    Bin(BinOp, Box<Expr>, Box<Expr>),
    /// 三目 `cond ? a : b`（右结合）
    Ternary(Box<Expr>, Box<Expr>, Box<Expr>),
    /// null 合并 `a ?? b`（右结合）
    Coalesce(Box<Expr>, Box<Expr>),
}

/// 字面量。
#[derive(Clone, Debug, PartialEq)]
pub enum Literal {
    /// 字符串（转义已解码）
    Str(String),
    /// 64 位整数
    Int(i64),
    /// 双精度浮点
    Float(f64),
    /// 布尔
    Bool(bool),
    /// null
    Null,
}

/// 路径段。
#[derive(Clone, Debug, PartialEq)]
pub enum Seg {
    /// 属性 `A.B` 中的 `B`
    Prop(String),
    /// 索引 `A[i]`
    Index(Box<Expr>),
}

/// 单目运算符。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnOp {
    /// 逻辑非 `!`
    Not,
    /// 取负 `-`
    Neg,
}

/// 二元运算符（优先级从高到低排列）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    /// `*`
    Mul,
    /// `/`
    Div,
    /// `%`
    Mod,
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `<`
    Lt,
    /// `>`
    Gt,
    /// `<=`
    Le,
    /// `>=`
    Ge,
    /// `==`
    Eq,
    /// `!=`
    Ne,
    /// `&&`
    And,
    /// `||`
    Or,
}

impl fmt::Display for Expr {
    /// 诊断用表达式文本：近似源形式（复合子表达式补括号保证无歧义），不保证可回读。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Expr::Lit(Literal::Str(s)) => write!(f, "\"{s}\""),
            Expr::Lit(Literal::Int(i)) => write!(f, "{i}"),
            Expr::Lit(Literal::Float(x)) => write!(f, "{x}"),
            Expr::Lit(Literal::Bool(b)) => write!(f, "{b}"),
            Expr::Lit(Literal::Null) => write!(f, "null"),
            Expr::Path(segs) => {
                for (i, seg) in segs.iter().enumerate() {
                    match seg {
                        Seg::Prop(name) => {
                            if i > 0 {
                                write!(f, ".")?;
                            }
                            write!(f, "{name}")?;
                        }
                        Seg::Index(ix) => write!(f, "[{ix}]")?,
                    }
                }
                Ok(())
            }
            Expr::Unary(op, x) => {
                let sym = match op {
                    UnOp::Not => "!",
                    UnOp::Neg => "-",
                };
                write!(f, "{sym}{}", wrap(x))
            }
            Expr::Bin(op, l, r) => {
                let sym = match op {
                    BinOp::Mul => "*",
                    BinOp::Div => "/",
                    BinOp::Mod => "%",
                    BinOp::Add => "+",
                    BinOp::Sub => "-",
                    BinOp::Lt => "<",
                    BinOp::Gt => ">",
                    BinOp::Le => "<=",
                    BinOp::Ge => ">=",
                    BinOp::Eq => "==",
                    BinOp::Ne => "!=",
                    BinOp::And => "&&",
                    BinOp::Or => "||",
                };
                write!(f, "{} {sym} {}", wrap(l), wrap(r))
            }
            Expr::Ternary(c, t, e) => write!(f, "{} ? {} : {}", wrap(c), wrap(t), wrap(e)),
            Expr::Coalesce(l, r) => write!(f, "{} ?? {}", wrap(l), wrap(r)),
        }
    }
}

/// 诊断文本的括号包装（复合子表达式加括号，保证无歧义）。
fn wrap(e: &Expr) -> String {
    match e {
        Expr::Bin(..) | Expr::Ternary(..) | Expr::Coalesce(..) => format!("({e})"),
        _ => e.to_string(),
    }
}

// ————— 解析入口 —————

/// 解析表达式源码。
///
/// `line` / `col` 为 `source` 首字符的位置（用于错误定位）。
pub fn parse(source: &str, line: usize, col: usize) -> Result<Expr, ParseError> {
    let mut p = ExprParser {
        chars: source.chars().collect(),
        pos: 0,
        line,
        col,
        depth: 0,
    };
    p.skip_whitespace();
    if p.peek().is_none() {
        return Err(p.error("表达式不能为空"));
    }
    let expr = p.parse_ternary()?;
    p.skip_whitespace();
    if let Some(c) = p.peek() {
        return Err(match c {
            '(' => p
                .error("不支持方法调用")
                .with_hint("v0 子集仅识别 @Raw / @Html.Raw 固定模式；其余调用请移入页面模型"),
            '=' => p.error("表达式内不支持赋值"),
            '|' | '&' | '^' => p
                .error(format!("不支持位运算 '{c}'（子集规约 v0.1）"))
                .with_hint("逻辑与/或请使用 && 与 ||"),
            _ => p.error(format!("表达式存在多余内容：意外字符 '{c}'")),
        });
    }
    Ok(expr)
}

// ————— 解析器 —————

struct ExprParser {
    chars: Vec<char>,
    pos: usize,
    line: usize,
    col: usize,
    /// 递归深度（单目链 / 括号嵌套 / 三目右结合链均由此累计）
    depth: usize,
}

impl ExprParser {
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

    /// 跳过空白与 C# 注释（`//`、`/* */`；子集允许表达式内注释）。
    ///
    /// 注释未闭合时不在此报错，交由后续的「意外结束」错误统一兜底。
    fn skip_whitespace(&mut self) {
        loop {
            while matches!(self.peek(), Some(c) if c.is_whitespace()) {
                self.bump();
            }
            if self.peek() == Some('/') && self.peek_at(1) == Some('/') {
                while matches!(self.peek(), Some(c) if c != '\n') {
                    self.bump();
                }
            } else if self.peek() == Some('/') && self.peek_at(1) == Some('*') {
                self.bump();
                self.bump();
                while let Some(c) = self.peek() {
                    self.bump();
                    if c == '*' && self.peek() == Some('/') {
                        self.bump();
                        break;
                    }
                }
            } else {
                break;
            }
        }
    }

    fn error(&self, message: impl Into<String>) -> ParseError {
        ParseError::new(self.line, self.col, message)
    }

    /// 记录当前位置（错误定位到本字符而非消费后）。
    fn here(&self) -> (usize, usize) {
        (self.line, self.col)
    }

    // ———— 语法层级（从低到高） ————

    /// `?:`（右结合）：`coalesce [ '?' ternary ':' ternary ]`
    fn parse_ternary(&mut self) -> Result<Expr, ParseError> {
        let cond = self.parse_coalesce()?;
        self.skip_whitespace();
        if self.peek() == Some('?') {
            if self.peek_at(1) == Some('.') {
                return Err(self
                    .error("不支持 null 条件运算符 ?. ")
                    .with_hint("规划于迭代 3 评估；可用 @if 判空或 @(表达式 ?? 默认值) 替代"));
            }
            self.bump(); // '?'
            let then_expr = self.parse_ternary()?;
            self.skip_whitespace();
            if self.peek() != Some(':') {
                return Err(self.error("三目表达式缺少 ':'"));
            }
            self.bump(); // ':'
            let else_expr = self.parse_ternary()?;
            return Ok(Expr::Ternary(
                Box::new(cond),
                Box::new(then_expr),
                Box::new(else_expr),
            ));
        }
        Ok(cond)
    }

    /// `??`（右结合）
    fn parse_coalesce(&mut self) -> Result<Expr, ParseError> {
        let lhs = self.parse_or()?;
        self.skip_whitespace();
        if self.peek() == Some('?') && self.peek_at(1) == Some('?') {
            self.bump();
            self.bump();
            let rhs = self.parse_coalesce()?;
            return Ok(Expr::Coalesce(Box::new(lhs), Box::new(rhs)));
        }
        Ok(lhs)
    }

    /// `||`（左结合）
    fn parse_or(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_and()?;
        loop {
            self.skip_whitespace();
            if self.peek() == Some('|') && self.peek_at(1) == Some('|') {
                self.bump();
                self.bump();
                let rhs = self.parse_and()?;
                lhs = Expr::Bin(BinOp::Or, Box::new(lhs), Box::new(rhs));
            } else {
                break;
            }
        }
        Ok(lhs)
    }

    /// `&&`（左结合）
    fn parse_and(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_equality()?;
        loop {
            self.skip_whitespace();
            if self.peek() == Some('&') && self.peek_at(1) == Some('&') {
                self.bump();
                self.bump();
                let rhs = self.parse_equality()?;
                lhs = Expr::Bin(BinOp::And, Box::new(lhs), Box::new(rhs));
            } else {
                break;
            }
        }
        Ok(lhs)
    }

    /// `==` `!=`（左结合）
    fn parse_equality(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_relational()?;
        loop {
            self.skip_whitespace();
            let op = match (self.peek(), self.peek_at(1)) {
                (Some('='), Some('=')) => BinOp::Eq,
                (Some('!'), Some('=')) => BinOp::Ne,
                _ => break,
            };
            self.bump();
            self.bump();
            let rhs = self.parse_relational()?;
            lhs = Expr::Bin(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    /// `<` `>` `<=` `>=`（左结合）
    fn parse_relational(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_additive()?;
        loop {
            self.skip_whitespace();
            let op = match (self.peek(), self.peek_at(1)) {
                (Some('<'), Some('=')) => BinOp::Le,
                (Some('>'), Some('=')) => BinOp::Ge,
                (Some('<'), _) => BinOp::Lt,
                (Some('>'), _) => BinOp::Gt,
                _ => break,
            };
            if matches!(op, BinOp::Lt | BinOp::Gt) {
                self.bump();
            } else {
                self.bump();
                self.bump();
            }
            let rhs = self.parse_additive()?;
            lhs = Expr::Bin(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    /// `+` `-`（左结合）
    fn parse_additive(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_multiplicative()?;
        loop {
            self.skip_whitespace();
            let op = match self.peek() {
                Some('+') => BinOp::Add,
                Some('-') => BinOp::Sub,
                _ => break,
            };
            self.bump();
            let rhs = self.parse_multiplicative()?;
            lhs = Expr::Bin(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    /// `*` `/` `%`（左结合）
    fn parse_multiplicative(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_unary()?;
        loop {
            self.skip_whitespace();
            let op = match self.peek() {
                Some('*') => BinOp::Mul,
                Some('/') => BinOp::Div,
                Some('%') => BinOp::Mod,
                _ => break,
            };
            self.bump();
            let rhs = self.parse_unary()?;
            lhs = Expr::Bin(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    /// 单目 `!` `-`（可链式），并承担递归深度守卫。
    fn parse_unary(&mut self) -> Result<Expr, ParseError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self
                .error("表达式嵌套过深")
                .with_hint(format!("上限 {MAX_DEPTH} 层")));
        }
        let result = self.parse_unary_inner();
        self.depth -= 1;
        result
    }

    fn parse_unary_inner(&mut self) -> Result<Expr, ParseError> {
        self.skip_whitespace();
        match self.peek() {
            Some('!') => {
                self.bump();
                let operand = self.parse_unary()?;
                Ok(Expr::Unary(UnOp::Not, Box::new(operand)))
            }
            Some('-') => {
                self.bump();
                let operand = self.parse_unary()?;
                Ok(Expr::Unary(UnOp::Neg, Box::new(operand)))
            }
            _ => self.parse_primary(),
        }
    }

    /// 字面量 / 路径 / 分组括号。
    fn parse_primary(&mut self) -> Result<Expr, ParseError> {
        self.skip_whitespace();
        let Some(c) = self.peek() else {
            return Err(self.error("表达式意外结束（缺少操作数）"));
        };
        match c {
            '(' => {
                self.bump();
                let inner = self.parse_ternary()?;
                self.skip_whitespace();
                if self.peek() != Some(')') {
                    return Err(self.error("表达式缺少 )"));
                }
                self.bump();
                Ok(inner)
            }
            '"' => Ok(Expr::Lit(Literal::Str(self.scan_string()?))),
            '0'..='9' => Ok(Expr::Lit(self.scan_number()?)),
            '\'' => Err(self
                .error("不支持字符字面量")
                .with_hint("请改用字符串，如 \"x\"")),
            c if is_ident_start(c) => self.parse_path_or_keyword(),
            _ => Err(self.error(format!("意外的字符 '{c}'"))),
        }
    }

    /// 关键字（`true`/`false`/`null`）或路径。
    fn parse_path_or_keyword(&mut self) -> Result<Expr, ParseError> {
        let name = self.scan_identifier();
        match name.as_str() {
            "true" => return Ok(Expr::Lit(Literal::Bool(true))),
            "false" => return Ok(Expr::Lit(Literal::Bool(false))),
            "null" => return Ok(Expr::Lit(Literal::Null)),
            _ => {}
        }
        let mut segs = vec![Seg::Prop(name)];
        loop {
            match self.peek() {
                Some('.') => {
                    self.bump();
                    if !matches!(self.peek(), Some(c) if is_ident_start(c)) {
                        return Err(self.error("'.' 后缺少属性名"));
                    }
                    segs.push(Seg::Prop(self.scan_identifier()));
                }
                Some('[') => {
                    self.bump();
                    let index = self.parse_ternary()?;
                    self.skip_whitespace();
                    if self.peek() != Some(']') {
                        return Err(self.error("索引器缺少 ]"));
                    }
                    self.bump();
                    segs.push(Seg::Index(Box::new(index)));
                }
                _ => break,
            }
        }
        if self.peek() == Some('(') {
            return Err(self
                .error("不支持方法调用")
                .with_hint("v0 子集仅识别 @Raw / @Html.Raw 固定模式；其余调用请移入页面模型"));
        }
        Ok(Expr::Path(segs))
    }

    // ———— 词素 ————

    fn scan_identifier(&mut self) -> String {
        let mut s = String::new();
        while matches!(self.peek(), Some(c) if is_ident_continue(c)) {
            s.push(self.bump().unwrap());
        }
        s
    }

    /// 数字：十进制整数 / 浮点（不支持科学计数法与类型后缀）。
    fn scan_number(&mut self) -> Result<Literal, ParseError> {
        let start = self.here();
        let mut s = String::new();
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            s.push(self.bump().unwrap());
        }
        let mut is_float = false;
        if self.peek() == Some('.') && matches!(self.peek_at(1), Some(c) if c.is_ascii_digit()) {
            is_float = true;
            s.push(self.bump().unwrap()); // '.'
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                s.push(self.bump().unwrap());
            }
        }
        if matches!(self.peek(), Some(c) if is_ident_continue(c)) {
            return Err(self
                .error("数字后不支持后缀或科学计数法（子集规约 v0.1）")
                .with_hint("仅支持十进制整数与浮点，如 42、1.5"));
        }
        if self.peek() == Some('.') {
            return Err(self.error("小数点后缺少数字"));
        }
        if is_float {
            match s.parse::<f64>() {
                Ok(v) => Ok(Literal::Float(v)),
                Err(_) => Err(ParseError::new(start.0, start.1, "浮点数超出范围")),
            }
        } else {
            match s.parse::<i64>() {
                Ok(v) => Ok(Literal::Int(v)),
                Err(_) => Err(ParseError::new(start.0, start.1, "整数超出范围")),
            }
        }
    }

    /// 字符串字面量（双引号 + 基础 C# 转义，转义在此解码）。
    fn scan_string(&mut self) -> Result<String, ParseError> {
        let start = self.here();
        self.bump(); // 开引号
        let mut s = String::new();
        loop {
            match self.bump() {
                None => {
                    return Err(ParseError::new(start.0, start.1, "字符串字面量未闭合")
                        .with_hint("补上 \""));
                }
                Some('"') => return Ok(s),
                Some('\\') => match self.bump() {
                    None => {
                        return Err(ParseError::new(start.0, start.1, "字符串字面量未闭合")
                            .with_hint("补上 \""));
                    }
                    Some('"') => s.push('"'),
                    Some('\\') => s.push('\\'),
                    Some('\'') => s.push('\''),
                    Some('n') => s.push('\n'),
                    Some('r') => s.push('\r'),
                    Some('t') => s.push('\t'),
                    Some('0') => s.push('\0'),
                    Some(c) => {
                        return Err(self
                            .error(format!("不支持的转义序列 '\\{c}'"))
                            .with_hint("支持：\\\" \\\\ \\' \\n \\r \\t \\0"));
                    }
                },
                Some('\n' | '\r') => {
                    return Err(ParseError::new(start.0, start.1, "字符串字面量不能跨行")
                        .with_hint("请改用 \\n 或整理数据"));
                }
                Some(c) => s.push(c),
            }
        }
    }
}

/// 是否标识符起始字符（与 `lexer` 保持一致的宽口径）。
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

    fn ok(src: &str) -> Expr {
        parse(src, 1, 1).unwrap()
    }

    fn err(src: &str) -> ParseError {
        parse(src, 1, 1).expect_err("应报错")
    }

    fn i(v: i64) -> Expr {
        Expr::Lit(Literal::Int(v))
    }

    fn f(v: f64) -> Expr {
        Expr::Lit(Literal::Float(v))
    }

    fn p(name: &str) -> Expr {
        Expr::Path(vec![Seg::Prop(name.into())])
    }

    fn bin(op: BinOp, l: Expr, r: Expr) -> Expr {
        Expr::Bin(op, Box::new(l), Box::new(r))
    }

    // ———— 字面量 ————

    #[test]
    fn literals_all_kinds() {
        assert_eq!(ok("42"), i(42));
        assert_eq!(ok("1.5"), f(1.5));
        assert_eq!(ok("true"), Expr::Lit(Literal::Bool(true)));
        assert_eq!(ok("false"), Expr::Lit(Literal::Bool(false)));
        assert_eq!(ok("null"), Expr::Lit(Literal::Null));
        assert_eq!(ok("\"hi\""), Expr::Lit(Literal::Str("hi".into())));
        assert_eq!(ok(" 42 "), i(42));
    }

    #[test]
    fn string_escapes_are_decoded() {
        assert_eq!(
            ok("\"a\\\"b\\\\c\\nd\\te\""),
            Expr::Lit(Literal::Str("a\"b\\c\nd\te".into()))
        );
        assert_eq!(ok("\"站点\""), Expr::Lit(Literal::Str("站点".into())));
    }

    #[test]
    fn negative_number_is_unary_neg() {
        assert_eq!(ok("-7"), Expr::Unary(UnOp::Neg, Box::new(i(7))));
        assert_eq!(ok("- 0.5"), Expr::Unary(UnOp::Neg, Box::new(f(0.5))));
    }

    // ———— 路径 ————

    #[test]
    fn paths_with_props_and_index() {
        assert_eq!(ok("Model"), p("Model"));
        assert_eq!(
            ok("Model.Name"),
            Expr::Path(vec![Seg::Prop("Model".into()), Seg::Prop("Name".into())])
        );
        assert_eq!(
            ok("Model.Sites[0].Name"),
            Expr::Path(vec![
                Seg::Prop("Model".into()),
                Seg::Prop("Sites".into()),
                Seg::Index(Box::new(i(0))),
                Seg::Prop("Name".into()),
            ])
        );
        assert_eq!(
            ok("Map[\"key\"]"),
            Expr::Path(vec![
                Seg::Prop("Map".into()),
                Seg::Index(Box::new(Expr::Lit(Literal::Str("key".into())))),
            ])
        );
        assert_eq!(
            ok("Items[i + 1]"),
            Expr::Path(vec![
                Seg::Prop("Items".into()),
                Seg::Index(Box::new(bin(BinOp::Add, p("i"), i(1)))),
            ])
        );
    }

    // ———— 优先级 ————

    #[test]
    fn arithmetic_precedence() {
        assert_eq!(
            ok("1 + 2 * 3"),
            bin(BinOp::Add, i(1), bin(BinOp::Mul, i(2), i(3)))
        );
        assert_eq!(
            ok("(1 + 2) * 3"),
            bin(BinOp::Mul, bin(BinOp::Add, i(1), i(2)), i(3))
        );
        assert_eq!(
            ok("8 / 2 - 1"),
            bin(BinOp::Sub, bin(BinOp::Div, i(8), i(2)), i(1))
        );
    }

    #[test]
    fn logic_and_equality_precedence() {
        assert_eq!(
            ok("a || b && c"),
            bin(BinOp::Or, p("a"), bin(BinOp::And, p("b"), p("c")))
        );
        assert_eq!(
            ok("a < b == c >= d"),
            bin(
                BinOp::Eq,
                bin(BinOp::Lt, p("a"), p("b")),
                bin(BinOp::Ge, p("c"), p("d"))
            )
        );
        assert_eq!(
            ok("!a && b != c"),
            bin(
                BinOp::And,
                Expr::Unary(UnOp::Not, Box::new(p("a"))),
                bin(BinOp::Ne, p("b"), p("c"))
            )
        );
    }

    #[test]
    fn unary_chains() {
        assert_eq!(
            ok("!!a"),
            Expr::Unary(
                UnOp::Not,
                Box::new(Expr::Unary(UnOp::Not, Box::new(p("a"))))
            )
        );
        assert_eq!(
            ok("-(1 + 2)"),
            Expr::Unary(UnOp::Neg, Box::new(bin(BinOp::Add, i(1), i(2))))
        );
    }

    // ———— ?? 与三目 ————

    #[test]
    fn coalesce_is_right_associative() {
        assert_eq!(
            ok("a ?? b"),
            Expr::Coalesce(Box::new(p("a")), Box::new(p("b")))
        );
        assert_eq!(
            ok("a ?? b ?? c"),
            Expr::Coalesce(
                Box::new(p("a")),
                Box::new(Expr::Coalesce(Box::new(p("b")), Box::new(p("c"))))
            )
        );
        // 优先级：|| 高于 ??
        assert_eq!(
            ok("a ?? b || c"),
            Expr::Coalesce(Box::new(p("a")), Box::new(bin(BinOp::Or, p("b"), p("c"))))
        );
    }

    #[test]
    fn ternary_precedence_and_associativity() {
        assert_eq!(
            ok("a ? b : c"),
            Expr::Ternary(Box::new(p("a")), Box::new(p("b")), Box::new(p("c")))
        );
        // 右结合
        assert_eq!(
            ok("a ? b : c ? d : e"),
            Expr::Ternary(
                Box::new(p("a")),
                Box::new(p("b")),
                Box::new(Expr::Ternary(
                    Box::new(p("c")),
                    Box::new(p("d")),
                    Box::new(p("e"))
                ))
            )
        );
        // ?? 绑定强于 ?:
        assert_eq!(
            ok("x ?? y ? p : q"),
            Expr::Ternary(
                Box::new(Expr::Coalesce(Box::new(p("x")), Box::new(p("y")))),
                Box::new(p("p")),
                Box::new(p("q"))
            )
        );
        // 真分支内嵌套三目
        assert_eq!(
            ok("a ? b ? c : d : e"),
            Expr::Ternary(
                Box::new(p("a")),
                Box::new(Expr::Ternary(
                    Box::new(p("b")),
                    Box::new(p("c")),
                    Box::new(p("d"))
                )),
                Box::new(p("e"))
            )
        );
    }

    // ———— 负向 ————

    #[test]
    fn empty_or_incomplete_expression_errors() {
        assert!(err("").message.contains("不能为空"));
        assert!(err("   ").message.contains("不能为空"));
        assert!(err("1 +").message.contains("意外结束"));
        assert!(err("1 + * 2").message.contains("意外的字符 '*'"));
    }

    #[test]
    fn bracket_errors() {
        assert!(err("(a").message.contains("缺少 )"));
        assert!(err("a[0").message.contains("索引器缺少 ]"));
        assert!(err("a..b").message.contains("'.' 后缺少属性名"));
    }

    #[test]
    fn method_call_is_explicit_error() {
        let e = err("Foo(1)");
        assert!(e.message.contains("方法调用"));
        assert!(e.hint.is_some());
        let e2 = err("Foo (1)");
        assert!(e2.message.contains("方法调用"));
    }

    #[test]
    fn null_conditional_and_char_literal_errors() {
        assert!(err("a?.b").message.contains("?."));
        let e = err("'a'");
        assert!(e.message.contains("字符字面量"));
        assert!(e.hint.is_some());
    }

    #[test]
    fn assignment_and_bitwise_errors() {
        assert!(err("a = 1").message.contains("赋值"));
        let e = err("a & b");
        assert!(e.message.contains("位运算"));
        assert!(e.hint.is_some());
        assert!(err("a | b").message.contains("位运算"));
    }

    #[test]
    fn number_errors() {
        assert!(err("1.5f").message.contains("后缀"));
        assert!(err("1e5").message.contains("后缀"));
        assert!(err("0x1F").message.contains("后缀"));
        assert!(err("9223372036854775808").message.contains("超出范围"));
        assert!(err("1.foo").message.contains("小数点后缺少数字"));
    }

    #[test]
    fn string_errors() {
        assert!(err("\"abc").message.contains("未闭合"));
        assert!(err("\"a\\qb\"").message.contains("不支持的转义序列"));
        assert!(err("\"a\nb\"").message.contains("不能跨行"));
    }

    #[test]
    fn trailing_content_and_ternary_errors() {
        assert!(err("a b").message.contains("多余内容"));
        assert!(err("a ? b").message.contains("缺少 ':'"));
    }

    #[test]
    fn nesting_depth_is_guarded() {
        let src = format!(
            "{}x{}",
            "(".repeat(MAX_DEPTH + 6),
            ")".repeat(MAX_DEPTH + 6)
        );
        let e = parse(&src, 1, 1).unwrap_err();
        assert!(e.message.contains("嵌套过深"));
    }

    // ———— 位置 ————

    #[test]
    fn error_positions_track_lines() {
        // "1 +\n2 +"：最后一行的 '+' 之后结束
        let e = parse("1 +\n2 +", 5, 7).unwrap_err();
        assert_eq!((e.line, e.col), (6, 4));

        // "a + )"：')' 位置（列 14）
        let e = parse("a + )", 3, 10).unwrap_err();
        assert_eq!((e.line, e.col), (3, 14));
        assert!(e.message.contains("意外的字符 ')'"));
    }

    #[test]
    fn comments_are_skipped_as_whitespace() {
        assert_eq!(ok("a + /* c */ b"), bin(BinOp::Add, p("a"), p("b")));
        assert_eq!(ok("1 // c\n + 2"), bin(BinOp::Add, i(1), i(2)));
        // 未闭合注释：兜底为「意外结束」
        let e = err("a + /* 未闭合");
        assert!(e.message.contains("意外结束"));
    }

    #[test]
    fn display_is_diagnostic_friendly() {
        assert_eq!(ok("Model.Sites[0].Name").to_string(), "Model.Sites[0].Name");
        assert_eq!(ok("a + b * c").to_string(), "a + (b * c)");
        assert_eq!(ok("(a + b) * c").to_string(), "(a + b) * c");
        assert_eq!(ok("!a && b").to_string(), "!a && b");
        assert_eq!(ok("a ?? b ? c : d").to_string(), "(a ?? b) ? c : d");
        assert_eq!(ok("-(1 + 2)").to_string(), "-(1 + 2)");
    }
}
