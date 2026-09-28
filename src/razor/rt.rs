//! Razor 子集模板引擎：双端共享语义内核（runtime）。
//!
//! 本模块承载「求值语义 + 转义 + 错误构造」的最小内核，供两处调用：
//! - 解释器（`render.rs`）——逐节点求值；
//! - **生成代码**（F014 原生编译，`codegen.rs` 输出，`native.rs` 加载）——模板被编译为
//!   本机代码后，逐操作调用此处的函数。
//!
//! 契约（改动须知）：解释器与生成代码必须在输出字节与错误消息上**完全一致**，
//! 因此语义实现只在此处维护一份；两者都不允许各自复制逻辑。
//! 所有函数均标注 `#[inline]`：生成代码所在的 dylib 以 MIR 内联方式复用同一实现。

use std::fmt::Write as _;
use std::rc::Rc;

use crate::razor::error::RenderError;
use crate::razor::expr::{BinOp, UnOp};
use crate::razor::value::Value;

// ————— 错误构造 —————

/// 构造错误（热路径内部统一装箱：`Result` 保持小体积，避免每次求值搬运 48 字节错误结构）。
#[inline]
pub fn fail(path: impl Into<String>, msg: impl Into<String>) -> Box<RenderError> {
    Box::new(RenderError::new(path, msg))
}

/// 值类型的中文名（错误消息用）。
#[inline]
pub fn value_type_name(v: &Value) -> &'static str {
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

/// 属性访问失败的错误（`path` 为含失败段的完整路径）。
#[inline]
pub fn describe_prop_error(path: &str, name: &str, cur: &Value) -> Box<RenderError> {
    let msg = match cur {
        Value::Null => "在 null 上访问属性（C# 为 NullReferenceException）".to_string(),
        Value::Object(_) => format!("属性不存在：{name}"),
        other => format!("{} 不支持属性访问", value_type_name(other)),
    };
    fail(path, msg)
}

/// 一元 `!` / 一元 `-`（表达式源码文本仅在出错时生成）。
#[inline]
pub fn op_not(v: Value, expr_text: impl FnOnce() -> String) -> Result<Value, Box<RenderError>> {
    match v {
        Value::Bool(b) => Ok(Value::Bool(!b)),
        other => Err(fail(
            expr_text(),
            format!("! 需要布尔操作数，实际为 {}", value_type_name(&other)),
        )),
    }
}

/// 一元 `-`。
#[inline]
pub fn op_neg(v: Value, expr_text: impl FnOnce() -> String) -> Result<Value, Box<RenderError>> {
    match v {
        Value::Int(i) => Ok(Value::Int(i.wrapping_neg())),
        Value::Float(f) => Ok(Value::Float(-f)),
        other => Err(fail(
            expr_text(),
            format!("- 需要数值操作数，实际为 {}", value_type_name(&other)),
        )),
    }
}

/// `&&` / `||` 的布尔取用（`op_text` 为 `"&&"` 或 `"||"`）。
#[inline]
pub fn as_bool(
    v: &Value,
    op_text: &str,
    expr_text: impl FnOnce() -> String,
) -> Result<bool, Box<RenderError>> {
    match v {
        Value::Bool(b) => Ok(*b),
        other => Err(fail(
            expr_text(),
            format!(
                "{op_text} 需要布尔操作数，实际为 {}",
                value_type_name(other)
            ),
        )),
    }
}

/// `@if` / `else if` 条件取用。
#[inline]
pub fn cond_bool(v: &Value, expr_text: impl FnOnce() -> String) -> Result<bool, Box<RenderError>> {
    match v {
        Value::Bool(b) => Ok(*b),
        other => Err(fail(
            expr_text(),
            format!("@if 条件需要布尔值，实际为 {}", value_type_name(other)),
        )),
    }
}

/// 三目条件取用。
#[inline]
pub fn ternary_cond(
    v: &Value,
    expr_text: impl FnOnce() -> String,
) -> Result<bool, Box<RenderError>> {
    match v {
        Value::Bool(b) => Ok(*b),
        other => Err(fail(
            expr_text(),
            format!("三目条件需要布尔值，实际为 {}", value_type_name(other)),
        )),
    }
}

/// `@foreach` 迭代对象取用（`iter_text` 为迭代表达式源码文本，仅出错时生成）。
#[inline]
pub fn foreach_items(
    v: Value,
    iter_text: impl FnOnce() -> String,
) -> Result<Rc<Vec<Value>>, Box<RenderError>> {
    match v {
        Value::List(items) => Ok(items),
        Value::Null => Err(fail(
            iter_text(),
            "foreach 不能遍历 null（C# 为 NullReferenceException）",
        )),
        other => Err(fail(
            iter_text(),
            format!("foreach 需要列表，实际为 {}", value_type_name(&other)),
        )),
    }
}

// ————— 路径/属性/索引 —————

/// `Model` 别名：根对象的 `Model` 属性（若存在），否则根对象本身。
#[inline]
pub fn resolve_model_alias(model: &Value) -> Value {
    if let Value::Object(o) = model {
        if let Some(v) = o.get("Model") {
            return v.clone();
        }
    }
    model.clone()
}

/// 根对象属性取用（首段非局部变量、非 `Model` 时；缺失报「未找到变量或属性」）。
#[inline]
pub fn get_root_prop(model: &Value, name: &str) -> Result<Value, Box<RenderError>> {
    if let Value::Object(o) = model {
        if let Some(v) = o.get(name) {
            return Ok(v.clone());
        }
    }
    Err(fail(name, "未找到变量或属性"))
}

/// 属性访问（缺失/类型错误时按 `path` 构造诊断）。
#[inline]
pub fn prop_get(cur: &Value, name: &str, path: &str) -> Result<Value, Box<RenderError>> {
    match cur.get(name).cloned() {
        Some(v) => Ok(v),
        None => Err(describe_prop_error(path, name, cur)),
    }
}

/// 属性访问 + 每站点内联缓存（生成代码热路径）：命中为 1 次键比较。
///
/// `cache` 为该访问站点私有的槽位（初始 `u32::MAX`）：记录上次成功的条目下标，
/// 对象布局不变时直接命中；未命中回退线性扫描并回写槽位。
#[inline]
pub fn prop_get_ic(
    cur: &Value,
    cache: &mut u32,
    name: &str,
    path: &str,
) -> Result<Value, Box<RenderError>> {
    if let Value::Object(o) = cur {
        let idx = *cache as usize;
        if let Some((k, v)) = o.entry_at(idx) {
            if k == name {
                return Ok(v.clone());
            }
        }
        let mut i = 0;
        while let Some((k, v)) = o.entry_at(i) {
            if k == name {
                *cache = i as u32;
                return Ok(v.clone());
            }
            i += 1;
        }
    }
    Err(describe_prop_error(path, name, cur))
}

/// 索引访问（`path` 为含失败索引段的完整诊断路径，仅出错时使用）。
#[inline]
pub fn index_get(target: &Value, index: &Value, path: &str) -> Result<Value, Box<RenderError>> {
    index_into(target, index).map_err(|msg| fail(path, msg))
}

/// 索引访问内核（列表 + 整数、对象 + 字符串键）。
#[inline]
pub(crate) fn index_into(target: &Value, index: &Value) -> Result<Value, String> {
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

// ————— 运算符 —————

/// 数值（对齐 C# 的 int/double 提升）。
enum Number {
    I(i64),
    F(f64),
}

#[inline]
fn as_number(v: &Value) -> Option<Number> {
    match v {
        Value::Int(i) => Some(Number::I(*i)),
        Value::Float(f) => Some(Number::F(*f)),
        _ => None,
    }
}

#[inline]
fn to_f64(n: Number) -> f64 {
    match n {
        Number::I(i) => i as f64,
        Number::F(f) => f,
    }
}

/// 二元运算（`&&`/`||` 短路在调用方处理；`expr_text` 为完整表达式源码文本，仅出错时生成）。
pub fn apply_bin(
    op: BinOp,
    lv: Value,
    rv: Value,
    expr_text: impl FnOnce() -> String,
) -> Result<Value, Box<RenderError>> {
    match op {
        BinOp::Add => {
            // 任一侧为字符串 → 拼接（C# 语义：null 参与拼接按空串）
            if matches!(lv, Value::Str(_)) || matches!(rv, Value::Str(_)) {
                let mut s = String::new();
                lv.write_text_into(&mut s);
                rv.write_text_into(&mut s);
                return Ok(Value::Str(s.into()));
            }
            let (Some(a), Some(b)) = (as_number(&lv), as_number(&rv)) else {
                return Err(bin_type_err(
                    expr_text(),
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
                return Err(bin_type_err(expr_text(), &lv, &rv, "算术运算需要数值"));
            };
            numeric_op(op, a, b, expr_text)
        }
        BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge => {
            let (Some(a), Some(b)) = (as_number(&lv), as_number(&rv)) else {
                return Err(bin_type_err(expr_text(), &lv, &rv, "比较运算需要数值"));
            };
            Ok(Value::Bool(compare_numbers(op, a, b)))
        }
        BinOp::Eq | BinOp::Ne => {
            let eq = values_equal(&lv, &rv, expr_text)?;
            Ok(Value::Bool(if op == BinOp::Eq { eq } else { !eq }))
        }
        BinOp::And | BinOp::Or => unreachable!("短路运算在调用方处理"),
    }
}

/// 整数/浮点算术（整数除零与溢出显式报错，对齐 C# 异常语义）。
fn numeric_op(
    op: BinOp,
    a: Number,
    b: Number,
    expr_text: impl FnOnce() -> String,
) -> Result<Value, Box<RenderError>> {
    match (a, b) {
        (Number::I(x), Number::I(y)) => match op {
            BinOp::Sub => Ok(Value::Int(x.wrapping_sub(y))),
            BinOp::Mul => Ok(Value::Int(x.wrapping_mul(y))),
            BinOp::Div => {
                if y == 0 {
                    return Err(fail(
                        expr_text(),
                        "整数除数为零（C# 为 DivideByZeroException）",
                    ));
                }
                match x.checked_div(y) {
                    Some(v) => Ok(Value::Int(v)),
                    None => Err(fail(expr_text(), "整数除法溢出（C# 为 OverflowException）")),
                }
            }
            BinOp::Mod => {
                if y == 0 {
                    return Err(fail(
                        expr_text(),
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
pub fn values_equal(
    l: &Value,
    r: &Value,
    expr_text: impl FnOnce() -> String,
) -> Result<bool, Box<RenderError>> {
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
        (Value::List(_) | Value::Object(_), _) | (_, Value::List(_) | Value::Object(_)) => {
            Err(fail(
                expr_text(),
                "== 不支持列表/对象（C# 为引用比较，语义不保证一致）",
            ))
        }
        (a, b) => Err(fail(
            expr_text(),
            format!(
                "== 两侧类型不同（{} 与 {}）",
                value_type_name(a),
                value_type_name(b)
            ),
        )),
    }
}

fn bin_type_err(text: String, l: &Value, r: &Value, advice: &str) -> Box<RenderError> {
    fail(
        text,
        format!(
            "操作数类型不支持（{} 与 {}）：{advice}",
            value_type_name(l),
            value_type_name(r)
        ),
    )
}

/// 一元运算调度（供解释器/生成代码共用；`!`、`-`）。
#[inline]
pub fn apply_unary(
    op: UnOp,
    v: Value,
    expr_text: impl FnOnce() -> String,
) -> Result<Value, Box<RenderError>> {
    match op {
        UnOp::Not => op_not(v, expr_text),
        UnOp::Neg => op_neg(v, expr_text),
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
pub fn escape_html_into(text: &str, out: &mut String) {
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
pub fn write_escaped_value(v: &Value, out: &mut String) {
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
pub fn push_i64(out: &mut String, v: i64) {
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
