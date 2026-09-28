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
            // 预分配输出缓冲（小页面一次分配到位，避免热路径反复扩容）
            out: String::with_capacity(4096),
            scopes: Vec::with_capacity(8),
            depth: 0,
        };
        renderer.render_nodes(&self.nodes).map_err(|e| *e)?;
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
    fn render_nodes(&mut self, nodes: &[Node]) -> Result<(), Box<RenderError>> {
        self.depth += 1;
        if self.depth > self.options.max_depth {
            return Err(fail(
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

    fn render_node(&mut self, node: &Node) -> Result<(), Box<RenderError>> {
        match node {
            Node::Text(s) => {
                self.out.push_str(s);
                Ok(())
            }
            Node::Write(e) => {
                let v = self.eval(e)?;
                if self.options.escape {
                    write_escaped_value(&v, &mut self.out);
                } else {
                    v.write_text_into(&mut self.out);
                }
                Ok(())
            }
            Node::Raw(e) => {
                let v = self.eval(e)?;
                v.write_text_into(&mut self.out);
                Ok(())
            }
            Node::If { branches, else_ } => {
                for (cond, body) in branches {
                    let v = self.eval(cond)?;
                    match v {
                        Value::Bool(true) => return self.render_nodes(body),
                        Value::Bool(false) => {}
                        other => {
                            return Err(fail(
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
                        return Err(fail(
                            iter.to_string(),
                            "foreach 不能遍历 null（C# 为 NullReferenceException）",
                        ));
                    }
                    other => {
                        return Err(fail(
                            iter.to_string(),
                            format!("foreach 需要列表，实际为 {}", value_type_name(&other)),
                        ));
                    }
                };
                // 循环变量一次性入栈，逐轮改写值（Rc 克隆廉价，避免每行重建变量名）
                let marker = self.scopes.len();
                self.scopes.push((var.clone(), Value::Null));
                for item in items.iter() {
                    self.scopes[marker].1 = item.clone();
                    self.render_nodes(body)?;
                }
                self.scopes.truncate(marker);
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

    fn eval(&self, e: &Expr) -> Result<Value, Box<RenderError>> {
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
    #[inline(always)]
    fn eval_path(&self, whole: &Expr, segs: &[Seg]) -> Result<Value, Box<RenderError>> {
        let Seg::Prop(first) = &segs[0] else {
            // parser 保证首段为属性名；防御
            return Err(expr_err(whole, "路径缺少起始属性"));
        };
        // 快路径：`变量.属性`（页面模板最常见形态，如 `row.Id`、`Model.Title`）。
        // 局部变量优先且不克隆对象本身，直接在原值上取属性。
        if let [Seg::Prop(name)] = &segs[1..] {
            for (n, v) in self.scopes.iter().rev() {
                if n == first {
                    return match v.get(name).cloned() {
                        Some(val) => Ok(val),
                        None => Err(describe_prop_error(
                            &format_path(segs, 1, Some(name)),
                            name,
                            v,
                        )),
                    };
                }
            }
            let base = match self.root {
                Value::Object(o) => match o.get(first) {
                    Some(v) => v,
                    None if first == "Model" => self.root,
                    None => return Err(fail(first.clone(), "未找到变量或属性")),
                },
                _ if first == "Model" => self.root,
                _ => return Err(fail(first.clone(), "未找到变量或属性")),
            };
            return match base.get(name).cloned() {
                Some(val) => Ok(val),
                None => Err(describe_prop_error(
                    &format_path(segs, 1, Some(name)),
                    name,
                    base,
                )),
            };
        }
        let mut cur = self
            .resolve_root(first)
            .ok_or_else(|| fail(first.clone(), "未找到变量或属性"))?;
        // 成功路径零分配；失败时才拼装诊断路径（format_path）
        for (i, seg) in segs.iter().enumerate().skip(1) {
            match seg {
                Seg::Prop(name) => match cur.get(name).cloned() {
                    Some(v) => cur = v,
                    None => {
                        let path = format_path(segs, i, Some(name));
                        return Err(describe_prop_error(&path, name, &cur));
                    }
                },
                Seg::Index(ix) => {
                    let iv = self.eval(ix)?;
                    cur = index_into(&cur, &iv)
                        .map_err(|msg| fail(format_path(segs, i, None), msg))?;
                }
            }
        }
        Ok(cur)
    }

    /// 首段解析：局部变量栈（后进先出）→ 根对象属性 → `Model` 别名（根对象本身）。
    #[inline]
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

fn apply_bin_op(e: &Expr, op: BinOp, lv: Value, rv: Value) -> Result<Value, Box<RenderError>> {
    match op {
        BinOp::Add => {
            // 任一侧为字符串 → 拼接（C# 语义：null 参与拼接按空串）
            if matches!(lv, Value::Str(_)) || matches!(rv, Value::Str(_)) {
                // C# 语义：null 参与拼接按空串；直接追加避免中间字符串
                let mut s = String::new();
                lv.write_text_into(&mut s);
                rv.write_text_into(&mut s);
                return Ok(Value::Str(s.into()));
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
fn numeric_op(e: &Expr, op: BinOp, a: Number, b: Number) -> Result<Value, Box<RenderError>> {
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
fn values_equal(e: &Expr, l: &Value, r: &Value) -> Result<bool, Box<RenderError>> {
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

#[inline(always)]
fn literal_to_value(l: &Literal) -> Value {
    match l {
        Literal::Str(s) => Value::Str(s.clone()),
        Literal::Int(i) => Value::Int(*i),
        Literal::Float(f) => Value::Float(*f),
        Literal::Bool(b) => Value::Bool(*b),
        Literal::Null => Value::Null,
    }
}

/// 属性访问失败的错误（`path` 为含失败段的完整路径）。
fn describe_prop_error(path: &str, name: &str, cur: &Value) -> Box<RenderError> {
    let msg = match cur {
        Value::Null => "在 null 上访问属性（C# 为 NullReferenceException）".to_string(),
        Value::Object(_) => format!("属性不存在：{name}"),
        other => format!("{} 不支持属性访问", value_type_name(other)),
    };
    fail(path, msg)
}

/// 拼装诊断路径（仅错误路径调用）：`segs[..upto]` 为已成功解析的前缀，
/// `tail_prop` 为失败属性名（索引段失败传 `None`，取 `segs[upto]` 的源码形式）。
fn format_path(segs: &[Seg], upto: usize, tail_prop: Option<&str>) -> String {
    use std::fmt::Write as _;
    let mut path = String::new();
    for (i, seg) in segs[..upto].iter().enumerate() {
        match seg {
            Seg::Prop(name) => {
                if i > 0 {
                    path.push('.');
                }
                path.push_str(name);
            }
            Seg::Index(ix) => {
                let _ = write!(path, "[{ix}]");
            }
        }
    }
    match tail_prop {
        Some(name) => {
            path.push('.');
            path.push_str(name);
        }
        None => {
            if let Some(Seg::Index(ix)) = segs.get(upto) {
                let _ = write!(path, "[{ix}]");
            }
        }
    }
    path
}

/// 构造错误（热路径内部统一装箱：`Result` 保持小体积，避免每次求值搬运 48 字节错误结构）。
fn fail(path: impl Into<String>, msg: impl Into<String>) -> Box<RenderError> {
    Box::new(RenderError::new(path, msg))
}

fn expr_err(e: &Expr, msg: impl Into<String>) -> Box<RenderError> {
    fail(e.to_string(), msg)
}

fn bin_type_err(e: &Expr, l: &Value, r: &Value, advice: &str) -> Box<RenderError> {
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
///
/// 实现按字节扫描：可打印 ASCII 连续段整段复制（`push_str`），仅在特殊/非 ASCII
/// 字节处打断；块级快路径用 8 字节 SWAR 探测（无特殊字节整块跳过），避免长文本
/// 逐字符处理的迭代开销。
#[inline]
fn escape_html_into(text: &str, out: &mut String) {
    use std::fmt::Write as _;
    out.reserve(text.len());
    let bytes = text.as_bytes();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        // 8 字节快路径：整块均为「无特殊字节的可打印 ASCII」时直接前进
        while i + 8 <= bytes.len() && boring8(&bytes[i..i + 8]) {
            i += 8;
        }
        if i >= bytes.len() {
            break;
        }
        let b = bytes[i];
        // 单字节快路径：可打印 ASCII 且非特殊字符
        if matches!(b, b' '..=b'~') && !matches!(b, b'"' | b'&' | b'\'' | b'+' | b'<' | b'>') {
            i += 1;
            continue;
        }
        // 先复制未处理的原样段
        if start < i {
            out.push_str(&text[start..i]);
        }
        match b {
            b'"' => out.push_str("&quot;"),
            b'&' => out.push_str("&amp;"),
            b'\'' => out.push_str("&#x27;"),
            b'+' => out.push_str("&#x2B;"),
            b'<' => out.push_str("&lt;"),
            b'>' => out.push_str("&gt;"),
            _ => {
                // 控制符或非 ASCII：解码首字符（i 恒为字符边界）后整体实体化
                let c = text[i..].chars().next().expect("i 恒为字符边界");
                let _ = write!(out, "&#x{:X};", c as u32);
                i += c.len_utf8();
                start = i;
                continue;
            }
        }
        i += 1;
        start = i;
    }
    if start < bytes.len() {
        out.push_str(&text[start..]);
    }
}

/// 8 字节块是否「无特殊字节」（可做整段复制的充分条件）。
///
/// 特殊字节 = 需实体化的字符集：控制符（<0x20）、DEL 与非 ASCII（≥0x7F）、
/// 以及 `"` `&` `'` `+` `<` `>` 六字符。SWAR 探测允许误报（判非平凡而实际平凡），
/// 但不得漏报——漏报会破坏转义语义。
#[inline(always)]
fn boring8(chunk: &[u8]) -> bool {
    const ONES: u64 = 0x0101_0101_0101_0101;
    #[inline]
    fn has_zero(x: u64) -> bool {
        x.wrapping_sub(ONES) & !x & 0x8080_8080_8080_8080 != 0
    }
    #[inline]
    fn has_byte(x: u64, b: u8) -> bool {
        has_zero(x ^ (ONES * b as u64))
    }
    #[inline]
    fn has_less(x: u64, n: u8) -> bool {
        // 经典 hasless：n ∈ [1,128]
        x.wrapping_sub(ONES * n as u64) & !x & 0x8080_8080_8080_8080 != 0
    }
    let mut buf = [0u8; 8];
    buf.copy_from_slice(chunk);
    let x = u64::from_le_bytes(buf);
    if has_less(x, 0x20) || x & 0x8080_8080_8080_8080 != 0 || has_byte(x, 0x7F) {
        return false;
    }
    !(has_byte(x, b'"')
        || has_byte(x, b'&')
        || has_byte(x, b'\'')
        || has_byte(x, b'+')
        || has_byte(x, b'<')
        || has_byte(x, b'>'))
}

/// 转义写出值（热路径：不经中间 `String`；数值/布尔不含特殊字符，直接追加）。
#[inline(always)]
fn write_escaped_value(v: &Value, out: &mut String) {
    use std::fmt::Write as _;
    match v {
        Value::Null => {}
        Value::Bool(b) => out.push_str(if *b { "True" } else { "False" }),
        Value::Int(i) => push_i64(out, *i),
        Value::Float(f) => {
            if f.is_nan() {
                out.push_str("NaN");
            } else if *f == f64::INFINITY {
                out.push_str("&#x221E;");
            } else if *f == f64::NEG_INFINITY {
                out.push_str("-&#x221E;");
            } else {
                let _ = write!(out, "{f}");
            }
        }
        Value::Str(s) => escape_html_into(s, out),
        Value::List(_) | Value::Object(_) => {
            // 列表/对象先文本化再转义（罕见路径，保持与 to_text 管道一致）
            let mut tmp = String::new();
            v.write_text_into(&mut tmp);
            escape_html_into(&tmp, out);
        }
    }
}

/// 手写十进制整数写出（避开 `write!` 格式化机制开销，热路径使用）。
#[inline(always)]
fn push_i64(out: &mut String, v: i64) {
    if v == 0 {
        out.push('0');
        return;
    }
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    let mut u = v.unsigned_abs();
    while u > 0 {
        i -= 1;
        buf[i] = b'0' + (u % 10) as u8;
        u /= 10;
    }
    if v < 0 {
        out.push('-');
    }
    out.push_str(std::str::from_utf8(&buf[i..]).expect("十进制 ASCII"));
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
