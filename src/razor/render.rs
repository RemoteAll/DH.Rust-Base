//! Razor 子集模板引擎：渲染器（求值 + 转义 + 输出）。
//!
//! 为 [`Template`] 提供 `render` / `render_with`，语义对齐 C#（见
//! `Doc/Razor子集模板引擎架构.md`「3.2 子集规约 v0.1」语义对齐表）：
//!
//! - `null` 输出空串；`Bool` 文本为 `True/False`；整数除法截断；
//!   `+` 任一侧为字符串即拼接；算术语义对齐 C#（含除零 / 溢出检查）；
//! - null / 缺失链访问 = [`RenderError`]（快速失败，不静默输出空串）；
//! - HTML 自动转义：`& < > " '` 五字符集（v0 对齐目标，由 F011 互操作用例逐字节补全）；
//! - 变量作用域：`@{ var x = ...; }` 声明在所在块内自声明点起可见（对齐 Razor 内联发射
//!   语义）；`@foreach` 循环变量仅在循环体内可见，内层可遮蔽外层同名变量；
//! - 渲染嵌套深度由 [`Options::max_depth`] 限制（默认 32，防嵌套炸弹）。
//!
//! 模型解析约定：路径首段优先匹配局部变量，其次匹配根对象属性；`Model` 为根对象的
//! 别名（对齐 C# 中 Page 的 `Model` 属性语义）。

use crate::razor::error::RenderError;
use crate::razor::expr::{BinOp, Expr, Literal, Seg, UnOp};
use crate::razor::parser::{Node, Stmt, Template};
use crate::razor::value::Value;
use crate::razor::Options;

impl Template {
    /// 渲染模板（默认选项：开启 HTML 转义、嵌套上限 32）。
    pub fn render(&self, model: &Value) -> Result<String, RenderError> {
        self.render_with(model, &Options::default())
    }

    /// 按指定选项渲染模板。
    pub fn render_with(&self, model: &Value, options: &Options) -> Result<String, RenderError> {
        let mut renderer = Renderer {
            options,
            root: model,
            out: String::new(),
            scopes: Vec::new(),
            depth: 0,
        };
        renderer.render_nodes(&self.nodes)?;
        Ok(renderer.out)
    }
}

// ————— 渲染器 —————

struct Renderer<'a> {
    options: &'a Options,
    /// 页面模型（路径首段 `Model` 指向它）
    root: &'a Value,
    /// 输出缓冲
    out: String,
    /// 局部变量栈（`@{ }` 声明与循环变量；后进先出）
    scopes: Vec<(String, Value)>,
    /// 块嵌套深度（`render_nodes` 递归层数）
    depth: usize,
}

impl<'a> Renderer<'a> {
    // ———— 节点渲染 ————

    /// 渲染节点序列（块作用域：出口回收本层新增的局部变量）。
    fn render_nodes(&mut self, nodes: &[Node]) -> Result<(), RenderError> {
        self.depth += 1;
        if self.depth > self.options.max_depth {
            return Err(RenderError::new(
                "模板",
                format!(
                    "渲染嵌套过深（上限 {} 层，可调整 Options.max_depth）",
                    self.options.max_depth
                ),
            ));
        }
        let scope_marker = self.scopes.len();
        for node in nodes {
            self.render_node(node)?;
        }
        self.scopes.truncate(scope_marker);
        self.depth -= 1;
        Ok(())
    }

    fn render_node(&mut self, node: &Node) -> Result<(), RenderError> {
        match node {
            Node::Text(s) => {
                self.out.push_str(s);
                Ok(())
            }
            Node::Write(e) => {
                let v = self.eval(e)?;
                if self.options.escape {
                    escape_html_into(&v.to_text(), &mut self.out);
                } else {
                    self.out.push_str(&v.to_text());
                }
                Ok(())
            }
            Node::Raw(e) => {
                let v = self.eval(e)?;
                self.out.push_str(&v.to_text());
                Ok(())
            }
            Node::If { branches, else_ } => {
                for (cond, body) in branches {
                    let v = self.eval(cond)?;
                    match v {
                        Value::Bool(true) => return self.render_nodes(body),
                        Value::Bool(false) => {}
                        other => {
                            return Err(RenderError::new(
                                cond.to_string(),
                                format!("@if 条件需要布尔值，实际为 {}", value_type_name(&other)),
                            ));
                        }
                    }
                }
                if let Some(body) = else_ {
                    return self.render_nodes(body);
                }
                Ok(())
            }
            Node::ForEach { var, iter, body } => {
                let v = self.eval(iter)?;
                let items = match v {
                    Value::List(items) => items,
                    Value::Null => {
                        return Err(RenderError::new(
                            iter.to_string(),
                            "foreach 不能遍历 null（C# 为 NullReferenceException）",
                        ));
                    }
                    other => {
                        return Err(RenderError::new(
                            iter.to_string(),
                            format!("foreach 需要列表，实际为 {}", value_type_name(&other)),
                        ));
                    }
                };
                for item in &items {
                    let marker = self.scopes.len();
                    self.scopes.push((var.clone(), item.clone()));
                    self.render_nodes(body)?;
                    self.scopes.truncate(marker);
                }
                Ok(())
            }
            Node::Code(stmts) => {
                for stmt in stmts {
                    match stmt {
                        Stmt::VarDecl { name, value } => {
                            let v = self.eval(value)?;
                            self.scopes.push((name.clone(), v));
                        }
                    }
                }
                Ok(())
            }
        }
    }

    // ———— 表达式求值 ————

    fn eval(&self, e: &Expr) -> Result<Value, RenderError> {
        match e {
            Expr::Lit(l) => Ok(literal_to_value(l)),
            Expr::Path(segs) => self.eval_path(e, segs),
            Expr::Unary(op, x) => {
                let v = self.eval(x)?;
                match op {
                    UnOp::Not => match v {
                        Value::Bool(b) => Ok(Value::Bool(!b)),
                        other => Err(expr_err(
                            e,
                            format!("! 需要布尔操作数，实际为 {}", value_type_name(&other)),
                        )),
                    },
                    UnOp::Neg => match v {
                        Value::Int(i) => Ok(Value::Int(i.wrapping_neg())),
                        Value::Float(f) => Ok(Value::Float(-f)),
                        other => Err(expr_err(
                            e,
                            format!("- 需要数值操作数，实际为 {}", value_type_name(&other)),
                        )),
                    },
                }
            }
            // 短路运算（C# && / || 语义）
            Expr::Bin(BinOp::And, l, r) => {
                let lv = self.eval(l)?;
                match lv {
                    Value::Bool(false) => Ok(Value::Bool(false)),
                    Value::Bool(true) => {
                        let rv = self.eval(r)?;
                        match rv {
                            Value::Bool(b) => Ok(Value::Bool(b)),
                            other => Err(expr_err(
                                e,
                                format!("&& 需要布尔操作数，实际为 {}", value_type_name(&other)),
                            )),
                        }
                    }
                    other => Err(expr_err(
                        e,
                        format!("&& 需要布尔操作数，实际为 {}", value_type_name(&other)),
                    )),
                }
            }
            Expr::Bin(BinOp::Or, l, r) => {
                let lv = self.eval(l)?;
                match lv {
                    Value::Bool(true) => Ok(Value::Bool(true)),
                    Value::Bool(false) => {
                        let rv = self.eval(r)?;
                        match rv {
                            Value::Bool(b) => Ok(Value::Bool(b)),
                            other => Err(expr_err(
                                e,
                                format!("|| 需要布尔操作数，实际为 {}", value_type_name(&other)),
                            )),
                        }
                    }
                    other => Err(expr_err(
                        e,
                        format!("|| 需要布尔操作数，实际为 {}", value_type_name(&other)),
                    )),
                }
            }
            Expr::Bin(op, l, r) => {
                let lv = self.eval(l)?;
                let rv = self.eval(r)?;
                apply_bin_op(e, *op, lv, rv)
            }
            Expr::Ternary(c, t, f) => {
                let cv = self.eval(c)?;
                match cv {
                    Value::Bool(true) => self.eval(t),
                    Value::Bool(false) => self.eval(f),
                    other => Err(expr_err(
                        e,
                        format!("三目条件需要布尔值，实际为 {}", value_type_name(&other)),
                    )),
                }
            }
            Expr::Coalesce(l, r) => {
                let lv = self.eval(l)?;
                if lv.is_null() {
                    self.eval(r)
                } else {
                    Ok(lv)
                }
            }
        }
    }

    /// 路径求值：首段解析（局部变量 → 根对象属性 → `Model` 别名），随后逐段访问。
    fn eval_path(&self, whole: &Expr, segs: &[Seg]) -> Result<Value, RenderError> {
        let Seg::Prop(first) = &segs[0] else {
            // parser 保证首段为属性名；防御
            return Err(expr_err(whole, "路径缺少起始属性"));
        };
        let mut path = first.clone();
        let mut cur = self
            .resolve_root(first)
            .ok_or_else(|| RenderError::new(path.clone(), "未找到变量或属性"))?;
        for seg in &segs[1..] {
            match seg {
                Seg::Prop(name) => match cur.get(name).cloned() {
                    Some(v) => {
                        cur = v;
                        path.push('.');
                        path.push_str(name);
                    }
                    None => return Err(describe_prop_error(&path, name, &cur)),
                },
                Seg::Index(ix) => {
                    let iv = self.eval(ix)?;
                    let iv_text = iv.to_text();
                    let next = index_into(&cur, &iv)
                        .map_err(|msg| RenderError::new(format!("{path}[{iv_text}]"), msg))?;
                    cur = next;
                    path.push('[');
                    path.push_str(&iv_text);
                    path.push(']');
                }
            }
        }
        Ok(cur)
    }

    /// 首段解析：局部变量栈（后进先出）→ 根对象属性 → `Model` 别名（根对象本身）。
    fn resolve_root(&self, name: &str) -> Option<Value> {
        for (n, v) in self.scopes.iter().rev() {
            if n == name {
                return Some(v.clone());
            }
        }
        match self.root {
            Value::Object(o) => {
                if let Some(v) = o.get(name) {
                    Some(v.clone())
                } else if name == "Model" {
                    Some(self.root.clone())
                } else {
                    None
                }
            }
            _ => {
                if name == "Model" {
                    Some(self.root.clone())
                } else {
                    None
                }
            }
        }
    }
}

// ————— 运算符与语义 ————

/// 数值（对齐 C# 的 int/double 提升）。
enum Number {
    I(i64),
    F(f64),
}

fn as_number(v: &Value) -> Option<Number> {
    match v {
        Value::Int(i) => Some(Number::I(*i)),
        Value::Float(f) => Some(Number::F(*f)),
        _ => None,
    }
}

fn to_f64(n: Number) -> f64 {
    match n {
        Number::I(i) => i as f64,
        Number::F(f) => f,
    }
}

fn apply_bin_op(e: &Expr, op: BinOp, lv: Value, rv: Value) -> Result<Value, RenderError> {
    match op {
        BinOp::Add => {
            // 任一侧为字符串 → 拼接（C# 语义：null 参与拼接按空串）
            if matches!(lv, Value::Str(_)) || matches!(rv, Value::Str(_)) {
                return Ok(Value::Str(format!("{}{}", lv.to_text(), rv.to_text())));
            }
            let (Some(a), Some(b)) = (as_number(&lv), as_number(&rv)) else {
                return Err(bin_type_err(
                    e,
                    &lv,
                    &rv,
                    "+ 需要数值，或任一侧为字符串用于拼接",
                ));
            };
            match (a, b) {
                (Number::I(x), Number::I(y)) => Ok(Value::Int(x.wrapping_add(y))),
                (x, y) => Ok(Value::Float(to_f64(x) + to_f64(y))),
            }
        }
        BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod => {
            let (Some(a), Some(b)) = (as_number(&lv), as_number(&rv)) else {
                return Err(bin_type_err(e, &lv, &rv, "算术运算需要数值"));
            };
            numeric_op(e, op, a, b)
        }
        BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge => {
            let (Some(a), Some(b)) = (as_number(&lv), as_number(&rv)) else {
                return Err(bin_type_err(e, &lv, &rv, "比较运算需要数值"));
            };
            Ok(Value::Bool(compare_numbers(op, a, b)))
        }
        BinOp::Eq | BinOp::Ne => {
            let eq = values_equal(e, &lv, &rv)?;
            Ok(Value::Bool(if op == BinOp::Eq { eq } else { !eq }))
        }
        BinOp::And | BinOp::Or => unreachable!("短路运算在 eval 前处理"),
    }
}

/// 整数/浮点算术（整数除零与溢出显式报错，对齐 C# 异常语义）。
fn numeric_op(e: &Expr, op: BinOp, a: Number, b: Number) -> Result<Value, RenderError> {
    match (a, b) {
        (Number::I(x), Number::I(y)) => match op {
            BinOp::Sub => Ok(Value::Int(x.wrapping_sub(y))),
            BinOp::Mul => Ok(Value::Int(x.wrapping_mul(y))),
            BinOp::Div => {
                if y == 0 {
                    return Err(expr_err(e, "整数除数为零（C# 为 DivideByZeroException）"));
                }
                match x.checked_div(y) {
                    Some(v) => Ok(Value::Int(v)),
                    None => Err(expr_err(e, "整数除法溢出（C# 为 OverflowException）")),
                }
            }
            BinOp::Mod => {
                if y == 0 {
                    return Err(expr_err(
                        e,
                        "整数取模除数为零（C# 为 DivideByZeroException）",
                    ));
                }
                // i64::MIN % -1 除法部分溢出，但其余数按 C# 语义为 0
                Ok(Value::Int(x.checked_rem(y).unwrap_or(0)))
            }
            _ => unreachable!("仅算术运算符"),
        },
        (x, y) => {
            let (x, y) = (to_f64(x), to_f64(y));
            let v = match op {
                BinOp::Sub => x - y,
                BinOp::Mul => x * y,
                BinOp::Div => x / y,
                BinOp::Mod => x % y,
                _ => unreachable!("仅算术运算符"),
            };
            Ok(Value::Float(v))
        }
    }
}

fn compare_numbers(op: BinOp, a: Number, b: Number) -> bool {
    match (a, b) {
        (Number::I(x), Number::I(y)) => match op {
            BinOp::Lt => x < y,
            BinOp::Gt => x > y,
            BinOp::Le => x <= y,
            BinOp::Ge => x >= y,
            _ => unreachable!("仅比较运算符"),
        },
        (x, y) => {
            let (x, y) = (to_f64(x), to_f64(y));
            match op {
                BinOp::Lt => x < y,
                BinOp::Gt => x > y,
                BinOp::Le => x <= y,
                BinOp::Ge => x >= y,
                _ => unreachable!("仅比较运算符"),
            }
        }
    }
}

/// `==` / `!=`：null 先行、字符串值比较、数值提升；列表/对象引用比较不支持（显式报错）。
fn values_equal(e: &Expr, l: &Value, r: &Value) -> Result<bool, RenderError> {
    match (l, r) {
        (Value::Null, Value::Null) => Ok(true),
        (Value::Null, _) | (_, Value::Null) => Ok(false),
        (Value::Str(a), Value::Str(b)) => Ok(a == b),
        (Value::Bool(a), Value::Bool(b)) => Ok(a == b),
        (Value::Int(a), Value::Int(b)) => Ok(a == b),
        (Value::Int(_) | Value::Float(_), Value::Int(_) | Value::Float(_)) => {
            let (a, b) = (as_number(l).unwrap(), as_number(r).unwrap());
            Ok(to_f64(a) == to_f64(b))
        }
        (Value::List(_) | Value::Object(_), _) | (_, Value::List(_) | Value::Object(_)) => Err(
            expr_err(e, "== 不支持列表/对象（C# 为引用比较，语义不保证一致）"),
        ),
        (a, b) => Err(expr_err(
            e,
            format!(
                "== 两侧类型不同（{} 与 {}）",
                value_type_name(a),
                value_type_name(b)
            ),
        )),
    }
}

/// 索引访问（列表 + 整数、对象 + 字符串键）。
fn index_into(target: &Value, index: &Value) -> Result<Value, String> {
    match (target, index) {
        (Value::List(items), Value::Int(i)) => {
            if *i < 0 || *i as usize >= items.len() {
                Err(format!("索引越界（列表长度 {}，下标 {i}）", items.len()))
            } else {
                Ok(items[*i as usize].clone())
            }
        }
        (Value::List(_), other) => Err(format!(
            "列表索引需要整数，实际为 {}",
            value_type_name(other)
        )),
        (Value::Object(o), Value::Str(k)) => {
            o.get(k).cloned().ok_or_else(|| format!("键不存在：{k}"))
        }
        (Value::Object(_), other) => Err(format!(
            "对象索引需要字符串键，实际为 {}",
            value_type_name(other)
        )),
        (Value::Null, _) => Err("在 null 上取索引（C# 为 NullReferenceException）".into()),
        (other, _) => Err(format!("{} 不支持索引访问", value_type_name(other))),
    }
}

fn literal_to_value(l: &Literal) -> Value {
    match l {
        Literal::Str(s) => Value::Str(s.clone()),
        Literal::Int(i) => Value::Int(*i),
        Literal::Float(f) => Value::Float(*f),
        Literal::Bool(b) => Value::Bool(*b),
        Literal::Null => Value::Null,
    }
}

/// 属性访问失败的错误（含完整路径）。
fn describe_prop_error(path: &str, name: &str, cur: &Value) -> RenderError {
    let full = format!("{path}.{name}");
    let msg = match cur {
        Value::Null => "在 null 上访问属性（C# 为 NullReferenceException）".to_string(),
        Value::Object(_) => format!("属性不存在：{name}"),
        other => format!("{} 不支持属性访问", value_type_name(other)),
    };
    RenderError::new(full, msg)
}

fn expr_err(e: &Expr, msg: impl Into<String>) -> RenderError {
    RenderError::new(e.to_string(), msg)
}

fn bin_type_err(e: &Expr, l: &Value, r: &Value, advice: &str) -> RenderError {
    expr_err(
        e,
        format!(
            "操作数类型不支持（{} 与 {}）：{advice}",
            value_type_name(l),
            value_type_name(r)
        ),
    )
}

fn value_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "布尔",
        Value::Int(_) => "整数",
        Value::Float(_) => "浮点",
        Value::Str(_) => "字符串",
        Value::List(_) => "列表",
        Value::Object(_) => "对象",
    }
}

// ————— HTML 转义 —————

/// v0.1 转义：等价于 .NET 10 `HtmlEncoder.Default`（由互操作工具 `probe-encode` 全字符实测对齐）。
///
/// - 原样输出：空格与可打印 ASCII（0x20–0x7E，排除下表中的六个特殊字符）；
/// - `"`→`&quot;`、`&`→`&amp;`、`'`→`&#x27;`、`+`→`&#x2B;`、`<`→`&lt;`、`>`→`&gt;`；
/// - 其余字符（含 `\r` `\n`、Tab、非 ASCII、控制符）→ `&#x` + 大写十六进制 + `;`
///   （非 BMP 按标量编码，如 `😀`→`&#x1F600;`）。
fn escape_html_into(text: &str, out: &mut String) {
    use std::fmt::Write as _;
    for c in text.chars() {
        match c {
            '"' => out.push_str("&quot;"),
            '&' => out.push_str("&amp;"),
            '\'' => out.push_str("&#x27;"),
            '+' => out.push_str("&#x2B;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c if c.is_ascii() && (' '..='~').contains(&c) => out.push(c),
            _ => {
                let _ = write!(out, "&#x{:X};", c as u32);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn m(json: serde_json::Value) -> Value {
        Value::from(json)
    }

    fn render_ok(src: &str, model: &Value) -> String {
        Template::parse(src).unwrap().render(model).unwrap()
    }

    fn render_err(src: &str, model: &Value) -> RenderError {
        Template::parse(src).unwrap().render(model).unwrap_err()
    }

    // ———— 文本 / 转义 ————

    #[test]
    fn text_passthrough() {
        assert_eq!(render_ok("<p>hi</p>", &m(json!({}))), "<p>hi</p>");
    }

    #[test]
    fn write_escapes_five_chars() {
        let model = m(json!({"V": "&<>\"'"}));
        assert_eq!(render_ok("@Model.V", &model), "&amp;&lt;&gt;&quot;&#x27;");
        assert_eq!(render_ok("@(Model.V)", &model), "&amp;&lt;&gt;&quot;&#x27;");
    }

    #[test]
    fn escape_matches_html_encoder_default() {
        // `+`、Tab、非 ASCII、引号均按 HtmlEncoder.Default 实体化
        let model = m(json!({"V": "a+b 中\t\"&'<>"}));
        assert_eq!(
            render_ok("@Model.V", &model),
            "a&#x2B;b &#x4E2D;&#x9;&quot;&amp;&#x27;&lt;&gt;"
        );
        // 非 BMP → 单标量实体
        assert_eq!(render_ok("@Model.E", &m(json!({"E": "😀"}))), "&#x1F600;");
        // 换行 → 实体
        assert_eq!(render_ok("@Model.L", &m(json!({"L": "a\nb"}))), "a&#xA;b");
    }

    #[test]
    fn raw_is_not_escaped_and_escape_can_be_disabled() {
        let model = m(json!({"Html": "<b>x</b>"}));
        assert_eq!(render_ok("@Raw(Model.Html)", &model), "<b>x</b>");
        assert_eq!(render_ok("@Html.Raw(Model.Html)", &model), "<b>x</b>");
        let options = Options {
            escape: false,
            max_depth: 32,
        };
        let t = Template::parse("@Model.Html").unwrap();
        assert_eq!(t.render_with(&model, &options).unwrap(), "<b>x</b>");
    }

    #[test]
    fn null_renders_empty_string() {
        let model = m(json!({"N": null}));
        assert_eq!(render_ok("@Model.N", &model), "");
        assert_eq!(render_ok("[@Model.N]", &model), "[]");
    }

    // ———— 错误路径 ————

    #[test]
    fn missing_property_fails_fast_with_path() {
        let e = render_err("@Model.Missing", &m(json!({"Name": "x"})));
        assert_eq!(e.path, "Model.Missing");
        assert!(e.message.contains("属性不存在"));
    }

    #[test]
    fn null_chain_access_fails_fast() {
        let e = render_err("@Model.A.B", &m(json!({"A": null})));
        assert_eq!(e.path, "Model.A.B");
        assert!(e.message.contains("null"));
    }

    #[test]
    fn index_errors_report_path() {
        let model = m(json!({"Sites": [{"Name": "A"}]}));
        assert_eq!(render_ok("@Model.Sites[0].Name", &model), "A");
        let e = render_err("@Model.Sites[5].Name", &model);
        assert_eq!(e.path, "Model.Sites[5]");
        assert!(e.message.contains("越界"));
        let model2 = m(json!({"Map": {"k": "v"}}));
        assert_eq!(render_ok("@Model.Map[\"k\"]", &model2), "v");
    }

    // ———— 数值 / 布尔文本 ————

    #[test]
    fn csharp_number_text() {
        assert_eq!(render_ok("@Model.Count", &m(json!({"Count": 42}))), "42");
        assert_eq!(render_ok("@Model.Flag", &m(json!({"Flag": true}))), "True");
        assert_eq!(
            render_ok("@Model.Flag", &m(json!({"Flag": false}))),
            "False"
        );
        assert_eq!(render_ok("@Model.Ratio", &m(json!({"Ratio": 0.5}))), "0.5");
    }

    #[test]
    fn arithmetic_semantics() {
        assert_eq!(render_ok("@(7 / 2)", &m(json!({}))), "3"); // 整数除法截断
        assert_eq!(render_ok("@(7 / -2)", &m(json!({}))), "-3"); // 向零截断
        assert_eq!(render_ok("@(7 % -3)", &m(json!({}))), "1");
        assert_eq!(render_ok("@(2 + 3 * 4)", &m(json!({}))), "14");
        assert_eq!(render_ok("@(10 - 4.5)", &m(json!({}))), "5.5");
        assert_eq!(render_ok("@(1.5 * 2)", &m(json!({}))), "3");
        // 非 ASCII（∞）按 HtmlEncoder.Default 实体化
        assert_eq!(render_ok("@(1.0 / 0.0)", &m(json!({}))), "&#x221E;");
        assert_eq!(render_ok("@(0.0 / 0.0)", &m(json!({}))), "NaN");
    }

    #[test]
    fn integer_division_errors() {
        assert!(render_err("@(1 / 0)", &m(json!({})))
            .message
            .contains("除数为零"));
        assert!(render_err("@(1 % 0)", &m(json!({})))
            .message
            .contains("除数为零"));
    }

    #[test]
    fn string_concat_semantics() {
        assert_eq!(render_ok("@(\"a\" + 1 + true)", &m(json!({}))), "a1True");
        assert_eq!(render_ok("@(1 + 2 + \"x\")", &m(json!({}))), "3x");
        let model = m(json!({"N": null}));
        assert_eq!(render_ok("@(\"a\" + Model.N)", &model), "a");
    }

    #[test]
    fn comparison_logic_and_short_circuit() {
        let model = m(json!({"Count": 3, "Flag": true}));
        assert_eq!(
            render_ok("@(Model.Count > 2 && Model.Flag)", &model),
            "True"
        );
        assert_eq!(
            render_ok("@(Model.Count < 2 || Model.Flag)", &model),
            "True"
        );
        // 短路：右侧除零不执行
        assert_eq!(
            render_ok("@(false && Model.Count / 0 > 1)", &model),
            "False"
        );
        assert_eq!(render_ok("@(true || Model.Count / 0 > 1)", &model), "True");
    }

    #[test]
    fn equality_semantics() {
        let model = m(json!({"Count": 3, "N": null, "Name": "ok"}));
        assert_eq!(render_ok("@(Model.Count == 3)", &model), "True");
        assert_eq!(render_ok("@(Model.Count != 3.0)", &model), "False");
        assert_eq!(render_ok("@(Model.N == null)", &model), "True");
        assert_eq!(render_ok("@(Model.Name == \"ok\")", &model), "True");
        let e = render_err("@(Model.Count == \"3\")", &model);
        assert!(e.message.contains("类型不同"));
    }

    #[test]
    fn coalesce_and_ternary() {
        let model = m(json!({"N": null, "Count": 3}));
        assert_eq!(render_ok("@(Model.N ?? \"fallback\")", &model), "fallback");
        assert_eq!(
            render_ok("@(Model.Count > 2 ? \"many\" : \"few\")", &model),
            "many"
        );
    }

    #[test]
    fn operator_type_error_reports_expression() {
        let e = render_err("@(Model.Flag + 1)", &m(json!({"Flag": true})));
        assert_eq!(e.path, "Model.Flag + 1");
        assert!(e.message.contains("布尔"));
    }

    // ———— 语句结构 ————

    #[test]
    fn if_and_foreach_end_to_end() {
        let model = m(json!({
            "Enable": true,
            "Name": "首页",
            "Sites": [{"Name": "A"}, {"Name": "B"}]
        }));
        // 首行 if 体保留空缩；`}` 后的换行被吞掉（对齐 Razor）；中文按实体转义
        let src = "@if (Model.Enable) { <b>@Model.Name</b> } else { <u>off</u> }\n@foreach (var s in Model.Sites) { <li>@s.Name</li> }";
        assert_eq!(
            render_ok(src, &model),
            " <b>&#x9996;&#x9875;</b>  <li>A</li>  <li>B</li> "
        );
    }

    #[test]
    fn if_false_renders_else_branch() {
        let model = m(json!({"Enable": false}));
        assert_eq!(
            render_ok(
                "@if (Model.Enable) { <i>on</i> } else { <i>off</i> }",
                &model
            ),
            " <i>off</i> "
        );
    }

    #[test]
    fn foreach_empty_and_nested() {
        let model = m(json!({"Groups": [
            {"Name": "G1", "Items": ["a", "b"]},
            {"Name": "G2", "Items": []}
        ]}));
        // 内层循环与外层共用标记区；空集合渲染为空
        assert_eq!(
            render_ok(
                "@foreach (var g in Model.Groups) {<p>@g.Name:@foreach (var i in g.Items) {<i>@i</i>}</p>}",
                &model
            ),
            "<p>G1:<i>a</i><i>b</i></p><p>G2:</p>"
        );
    }

    #[test]
    fn foreach_type_errors() {
        assert!(
            render_err("@foreach (var x in Model.N) { }", &m(json!({"N": null})))
                .message
                .contains("不能遍历 null")
        );
        assert!(
            render_err("@foreach (var x in Model.F) { }", &m(json!({"F": true})))
                .message
                .contains("需要列表")
        );
    }

    // ———— 变量作用域 ————

    #[test]
    fn code_declarations_visible_afterward() {
        let model = m(json!({"Count": 3}));
        assert_eq!(
            render_ok("@{ var x = Model.Count + 1; }<i>@x</i>", &model),
            "<i>4</i>"
        );
        // 顶层声明可在后续 @if 条件中使用
        assert_eq!(
            render_ok("@{ var y = 2; }@if (y == 2) { <i>yes</i> }", &model),
            " <i>yes</i> "
        );
    }

    #[test]
    fn block_scoping_semantics() {
        // if 体内声明在体内可见（表达式周边空白为 C# 空白，不输出）
        assert_eq!(
            render_ok("@if (true) { @{ var z = 1; }@z }", &m(json!({}))),
            "1"
        );
        // if 体内声明在体外不可见
        let e = render_err("@if (true) { @{ var z = 1; } }@z", &m(json!({})));
        assert_eq!(e.path, "z");
        assert!(e.message.contains("未找到"));
    }

    #[test]
    fn foreach_var_shadows_and_restores() {
        let model = m(json!({"Sites": ["a", "b"]}));
        assert_eq!(
            render_ok(
                "@{ var s = \"outer\"; }@foreach (var s in Model.Sites) {<i>@s</i>}@s",
                &model
            ),
            "<i>a</i><i>b</i>outer"
        );
    }

    // ———— 深度守卫 ————

    #[test]
    fn render_depth_is_guarded_by_options() {
        let src = format!(
            "{}{}{}",
            "@if (true) {".repeat(40),
            " <i>x</i> ",
            "}".repeat(40)
        );
        let model = m(json!({}));
        let t = Template::parse(&src).unwrap();
        let e = t.render(&model).unwrap_err();
        assert!(e.message.contains("嵌套过深"));
        let options = Options {
            escape: true,
            max_depth: 100,
        };
        assert_eq!(t.render_with(&model, &options).unwrap(), " <i>x</i> ");
    }
}
