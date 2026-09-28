//! Razor 子集模板引擎：F014 原生编译——模板 → Rust 源码生成器。
//!
//! 生成物为一份独立 crate 的 `lib.rs`（cdylib），把模板「完全展开」为本机代码：
//! 无 AST 遍历、无字符串键查找缓存外的间接层，逐操作调用 [`crate::razor::rt`]
//! 的共享语义内核（保证与解释器输出字节、错误消息完全一致）。
//!
//! 生成代码结构：
//! - `razor_render`（`extern "C"`）：加载器调用入口，C ABI；
//! - `__render`：模板体，遍历被编译为字面量 `push_str`、直接量绑定、
//!   `rt::*` 调用与原生 `for`/`if` 控制流；
//! - 局部变量（`@foreach` 循环变量、`@{ var }`）编译为 Rust 局部绑定（`__u_*`），
//!   块作用域与遮蔽语义由 Rust 词法作用域天然对应；
//! - 诊断路径（错误消息中的 `Model.A.B[0]`）在生成期静态拼装，与解释器的
//!   `format_path` 规则一致。

use std::fmt::Write as _;

use crate::razor::expr::{BinOp, Expr, Literal, Seg, UnOp};
use crate::razor::parser::{Node, Stmt, Template};

/// 生成代码预期的 ABI 版本（加载器校验；不兼容时拒绝加载）。
pub const RAZOR_CODEGEN_ABI: u32 = 1;

impl Template {
    /// 生成可直接作为 cdylib 编译的 `lib.rs` 源码（UTF-8，含完整渲染入口）。
    pub fn to_rust_lib_source(&self) -> String {
        Gen::new().generate(self)
    }
}

/// 生成器内部状态。
struct Gen {
    /// 模板体缓冲（先写体、后装配，字面量池在体生成后才完备）
    body: String,
    /// 最终装配输出
    src: String,
    indent: usize,
    tmp: u32,
    /// 内联缓存槽计数（每个属性访问站点一个槽）
    ic_count: u32,
    /// 字符串字面量池（同值去重；生成代码开头预建 `Rc<str>`，热路径零分配）
    lit_pool: Vec<String>,
    lit_map: std::collections::HashMap<String, usize>,
    /// 用户变量作用域栈（存变量名；Rust 标识为 `__u_<name>`）
    scopes: Vec<Vec<String>>,
}

impl Gen {
    fn new() -> Self {
        Self {
            body: String::new(),
            src: String::new(),
            indent: 0,
            tmp: 0,
            ic_count: 0,
            lit_pool: Vec::new(),
            lit_map: std::collections::HashMap::new(),
            scopes: Vec::new(),
        }
    }

    fn generate(mut self, template: &Template) -> String {
        // 第一段：只生成模板体（同时完成字面量池注册）
        self.indent = 1;
        self.scopes.push(Vec::new());
        self.emit_nodes(&template.nodes);
        self.scopes.pop();
        self.line("Ok(())");

        // 第二段：装配最终源码
        self.src.push_str(
            "//! 本文件由 dhrust Razor 代码生成器（F014）自动生成，请勿手改。\n\
             //!\n\
             //! 语义契约：输出字节与错误消息必须与解释器（dhrust::razor::Template::render）\n\
             //! 完全一致；两者共用 dhrust::razor::rt 的语义内核。\n\
             #![allow(unused_variables, unused_mut, unused_imports, clippy::all)]\n\
             \n\
             use dhrust::razor::expr::{BinOp, UnOp};\n\
             use dhrust::razor::rt;\n\
             use dhrust::razor::{RenderError, Value};\n\
             \n",
        );
        let _ = write!(
            self.src,
            "/// 生成代码 ABI 版本（加载器校验用）。\n\
             #[no_mangle]\n\
             pub extern \"C\" fn razor_abi_version() -> u32 {{\n    {RAZOR_CODEGEN_ABI}\n}}\n\n\
             /// 渲染入口：成功返回 `true`；失败返回 `false` 并通过 `err_out` 移交错误所有权。\n\
             #[no_mangle]\n\
             pub extern \"C\" fn razor_render(\n\
             \x20   model: *const Value,\n\
             \x20   out: *mut String,\n\
             \x20   escape: bool,\n\
             \x20   err_out: *mut *mut RenderError,\n\
             ) -> bool {{\n\
             \x20   if model.is_null() || out.is_null() || err_out.is_null() {{\n\
             \x20       return false;\n\
             \x20   }}\n\
             \x20   let model = unsafe {{ &*model }};\n\
             \x20   let out = unsafe {{ &mut *out }};\n\
             \x20   let mut __ic = [u32::MAX; {n}];\n\
             \x20   match __render(model, out, escape, &mut __ic) {{\n\
             \x20       Ok(()) => true,\n\
             \x20       Err(e) => {{\n\
             \x20           unsafe {{ *err_out = Box::into_raw(e); }}\n\
             \x20           false\n\
             \x20       }}\n\
             \x20   }}\n\
             }}\n\n",
            n = self.ic_count
        );
        self.src.push_str(
            "fn __render(model: &Value, out: &mut String, escape: bool, __ic: &mut [u32]) -> Result<(), Box<RenderError>> {\n",
        );
        // 字面量池（预建 Rc<str>：热路径仅引用计数克隆，零分配）
        for (k, s) in self.lit_pool.iter().enumerate() {
            let _ = writeln!(
                self.src,
                "    let __lit{k}: std::rc::Rc<str> = std::rc::Rc::from({});",
                rust_str(s)
            );
        }
        self.src.push_str(&self.body);
        self.src.push_str("}\n");
        self.src
    }

    // ———— 基础输出 ————

    fn line(&mut self, text: &str) {
        for _ in 0..self.indent {
            self.body.push_str("    ");
        }
        self.body.push_str(text);
        self.body.push('\n');
    }

    /// 开启分支块（`header {`）并加深缩进。
    fn branch_open(&mut self, header: &str) {
        self.line(&format!("{header} {{"));
        self.indent += 1;
    }

    /// 切换 `} else {`。
    fn branch_else(&mut self) {
        self.indent -= 1;
        self.line("} else {");
        self.indent += 1;
    }

    /// 结束分支块（`semi=true` 时输出 `};`，用于块作为表达式值的 let 语句）。
    fn branch_close(&mut self, semi: bool) {
        self.indent -= 1;
        self.line(if semi { "};" } else { "}" });
    }

    fn next_tmp(&mut self) -> String {
        self.tmp += 1;
        format!("__t{}", self.tmp)
    }

    /// 字符串字面量入池（去重），返回池下标。
    fn intern_str(&mut self, s: &str) -> usize {
        if let Some(&k) = self.lit_map.get(s) {
            return k;
        }
        let k = self.lit_pool.len();
        self.lit_pool.push(s.to_string());
        self.lit_map.insert(s.to_string(), k);
        k
    }

    /// 字面量 → Rust 表达式（字符串走字面量池：每渲染预建一次 `Rc<str>`，热路径零分配）。
    fn literal_expr(&mut self, l: &Literal) -> String {
        match l {
            Literal::Str(s) => {
                let k = self.intern_str(s);
                format!("Value::Str(__lit{k}.clone())")
            }
            Literal::Int(i) => {
                if *i == i64::MIN {
                    "Value::Int(i64::MIN)".to_string()
                } else {
                    format!("Value::Int({i}i64)")
                }
            }
            Literal::Float(f) => {
                if f.is_nan() {
                    "Value::Float(f64::NAN)".to_string()
                } else if *f == f64::INFINITY {
                    "Value::Float(f64::INFINITY)".to_string()
                } else if *f == f64::NEG_INFINITY {
                    "Value::Float(f64::NEG_INFINITY)".to_string()
                } else {
                    // `{:?}` 输出最短往返且含小数点/指数的合法 Rust 字面量
                    format!("Value::Float({f:?})")
                }
            }
            Literal::Bool(b) => format!("Value::Bool({b})"),
            Literal::Null => "Value::Null".to_string(),
        }
    }

    fn var_ident(name: &str) -> String {
        format!("__u_{name}")
    }

    /// 查找用户变量（作用域由内向外），返回 Rust 标识。
    fn lookup_var(&self, name: &str) -> Option<String> {
        for level in self.scopes.iter().rev() {
            if level.iter().any(|n| n == name) {
                return Some(Self::var_ident(name));
            }
        }
        None
    }

    fn register_var(&mut self, name: &str) {
        if let Some(level) = self.scopes.last_mut() {
            level.push(name.to_string());
        }
    }

    // ———— 节点 ————

    fn emit_nodes(&mut self, nodes: &[Node]) {
        for node in nodes {
            self.emit_node(node);
        }
    }

    fn emit_node(&mut self, node: &Node) {
        match node {
            Node::Text(s) => {
                self.line(&format!("out.push_str({});", rust_str(s)));
            }
            Node::Write(e) => {
                let v = self.emit_value(e);
                self.branch_open("if escape");
                self.line(&format!("rt::write_escaped_value(&{v}, out);"));
                self.branch_else();
                self.line(&format!("{v}.write_text_into(out);"));
                self.branch_close(false);
            }
            Node::Raw(e) => {
                let v = self.emit_value(e);
                self.line(&format!("{v}.write_text_into(out);"));
            }
            Node::If { branches, else_ } => {
                self.emit_if(branches, else_.as_deref());
            }
            Node::ForEach { var, iter, body } => {
                let it = self.emit_value(iter);
                let items_var = self.next_tmp();
                self.line(&format!(
                    "let {items_var} = rt::foreach_items({it}, || {}.to_string())?;",
                    rust_str(&iter.to_string())
                ));
                let item_ident = self.next_tmp();
                self.branch_open(&format!("for {item_ident} in {items_var}.iter()"));
                self.scopes.push(Vec::new());
                self.line(&format!(
                    "let {}: Value = {item_ident}.clone();",
                    Self::var_ident(var)
                ));
                self.register_var(var);
                self.emit_nodes(body);
                self.scopes.pop();
                self.branch_close(false);
            }
            Node::Code(stmts) => {
                for stmt in stmts {
                    match stmt {
                        Stmt::VarDecl { name, value } => {
                            let v = self.emit_value(value);
                            self.line(&format!("let {}: Value = {v};", Self::var_ident(name)));
                            self.register_var(name);
                        }
                    }
                }
            }
        }
    }

    /// `@if` / `else if` / `else` 链：条件按序求值，与解释器一致。
    fn emit_if(&mut self, branches: &[(Expr, Vec<Node>)], else_body: Option<&[Node]>) {
        let Some(((cond, body), rest)) = branches.split_first() else {
            // 防御：parser 不会产生无分支的 If；仅渲染 else
            if let Some(eb) = else_body {
                self.branch_open("if true");
                self.scopes.push(Vec::new());
                self.emit_nodes(eb);
                self.scopes.pop();
                self.branch_close(false);
            }
            return;
        };
        let cv = self.emit_value(cond);
        self.branch_open(&format!(
            "if rt::cond_bool(&{cv}, || {}.to_string())?",
            rust_str(&cond.to_string())
        ));
        self.scopes.push(Vec::new());
        self.emit_nodes(body);
        self.scopes.pop();
        if rest.is_empty() {
            if let Some(eb) = else_body {
                self.branch_else();
                self.scopes.push(Vec::new());
                self.emit_nodes(eb);
                self.scopes.pop();
            }
            self.branch_close(false);
        } else {
            self.branch_else();
            self.emit_if(rest, else_body);
            self.branch_close(false);
        }
    }

    // ———— 表达式（语句化求值，返回持有结果的临时变量名） ————

    fn emit_value(&mut self, e: &Expr) -> String {
        match e {
            Expr::Lit(l) => {
                let t = self.next_tmp();
                let expr = self.literal_expr(l);
                self.line(&format!("let {t}: Value = {expr};"));
                t
            }
            Expr::Path(segs) => self.emit_path(segs),
            Expr::Unary(op, x) => {
                let v = self.emit_value(x);
                let t = self.next_tmp();
                let op_name = match op {
                    UnOp::Not => "Not",
                    UnOp::Neg => "Neg",
                };
                self.line(&format!(
                    "let {t}: Value = rt::apply_unary(UnOp::{op_name}, {v}, || {}.to_string())?;",
                    rust_str(&e.to_string())
                ));
                t
            }
            Expr::Bin(BinOp::And, l, r) => self.emit_short_circuit(true, l, r, e),
            Expr::Bin(BinOp::Or, l, r) => self.emit_short_circuit(false, l, r, e),
            Expr::Bin(op, l, r) => {
                let lv = self.emit_value(l);
                let rv = self.emit_value(r);
                let t = self.next_tmp();
                self.line(&format!(
                    "let {t}: Value = rt::apply_bin(BinOp::{}, {lv}, {rv}, || {}.to_string())?;",
                    binop_name(*op),
                    rust_str(&e.to_string())
                ));
                t
            }
            Expr::Ternary(c, yes, no) => {
                let cv = self.emit_value(c);
                let t = self.next_tmp();
                self.branch_open(&format!(
                    "let {t}: Value = if rt::ternary_cond(&{cv}, || {}.to_string())?",
                    rust_str(&e.to_string())
                ));
                let y = self.emit_value(yes);
                self.line(&y);
                self.branch_else();
                let n = self.emit_value(no);
                self.line(&n);
                self.branch_close(true);
                t
            }
            Expr::Coalesce(l, r) => {
                let lv = self.emit_value(l);
                let t = self.next_tmp();
                self.branch_open(&format!("let {t}: Value = if {lv}.is_null()"));
                let rv = self.emit_value(r);
                self.line(&rv);
                self.branch_else();
                self.line(&lv);
                self.branch_close(true);
                t
            }
        }
    }

    /// `&&` / `||` 短路（`and=true` 为 `&&`）。
    fn emit_short_circuit(&mut self, and: bool, l: &Expr, r: &Expr, whole: &Expr) -> String {
        let lv = self.emit_value(l);
        let t = self.next_tmp();
        let (op_text, short_lit) = if and {
            ("&&", "Value::Bool(false)")
        } else {
            ("||", "Value::Bool(true)")
        };
        let neg = if and { "!" } else { "" };
        self.branch_open(&format!(
            "let {t}: Value = if {neg}rt::as_bool(&{lv}, {op_text_lit}, || {whole_lit}.to_string())?",
            op_text_lit = rust_str(op_text),
            whole_lit = rust_str(&whole.to_string())
        ));
        self.line(short_lit);
        self.branch_else();
        let rv = self.emit_value(r);
        self.line(&format!(
            "Value::Bool(rt::as_bool(&{rv}, {op_text_lit}, || {whole_lit}.to_string())?)",
            op_text_lit = rust_str(op_text),
            whole_lit = rust_str(&whole.to_string())
        ));
        self.branch_close(true);
        t
    }

    /// 路径求值：单语句内组合（无中间临时/克隆）。
    ///
    /// - 首段绑定：局部变量借用 `&__u_x`、`Model` 借用 `&rt::resolve_model_alias(model)`、
    ///   根属性 `&rt::get_root_prop(model, "A")?`；
    /// - 后续逐段以 `rt::prop_get(前缀, ..)?` 嵌套组合（Rust 临时借用在同一语句内有效），
    ///   仅索引表达式单独成句（允许复杂表达式）。
    fn emit_path(&mut self, segs: &[Seg]) -> String {
        // parser 保证首段为属性名（防御：不可能路径，保持生成代码可编译）
        let Seg::Prop(first) = &segs[0] else {
            let t = self.next_tmp();
            self.line(&format!(
                "let {t}: Value = Value::Null; // 防御：parser 保证首段为属性名"
            ));
            return t;
        };
        // 单段路径：直接产出拥有值
        if segs.len() == 1 {
            let t = self.next_tmp();
            if let Some(ident) = self.lookup_var(first) {
                self.line(&format!("let {t}: Value = {ident}.clone();"));
            } else if first == "Model" {
                self.line(&format!("let {t}: Value = rt::resolve_model_alias(model);"));
            } else {
                self.line(&format!(
                    "let {t}: Value = rt::get_root_prop(model, {})?;",
                    rust_str(first)
                ));
            }
            return t;
        }
        // 多段：首段为借用表达式，逐段嵌套组合
        let mut base = if let Some(ident) = self.lookup_var(first) {
            format!("&{ident}")
        } else if first == "Model" {
            "&rt::resolve_model_alias(model)".to_string()
        } else {
            format!("&rt::get_root_prop(model, {})?", rust_str(first))
        };
        for (i, seg) in segs.iter().enumerate().skip(1) {
            match seg {
                Seg::Prop(name) => {
                    let site = self.ic_count;
                    self.ic_count += 1;
                    let expr = format!(
                        "rt::prop_get_ic({base}, &mut __ic[{site}], {}, {})?",
                        rust_str(name),
                        rust_str(&path_text(segs, i, Some(name)))
                    );
                    base = if i + 1 < segs.len() {
                        format!("&{expr}")
                    } else {
                        expr
                    };
                }
                Seg::Index(ix) => {
                    let iv = self.emit_value(ix);
                    let expr = format!(
                        "rt::index_get({base}, &{iv}, {})?",
                        rust_str(&path_text(segs, i, None))
                    );
                    base = if i + 1 < segs.len() {
                        format!("&{expr}")
                    } else {
                        expr
                    };
                }
            }
        }
        let t = self.next_tmp();
        self.line(&format!("let {t}: Value = {base};"));
        t
    }
}

// ———— 文本工具 ————

/// 生成期与解释器 `format_path` 完全一致的诊断路径（属性失败：前缀 + `.name`；
/// 索引失败：前缀 + `[索引表达式 Display]`）。
fn path_text(segs: &[Seg], upto: usize, tail_prop: Option<&str>) -> String {
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

/// `BinOp` → Rust 变体名（`dhrust::razor::expr::BinOp`）。
fn binop_name(op: BinOp) -> &'static str {
    match op {
        BinOp::Mul => "Mul",
        BinOp::Div => "Div",
        BinOp::Mod => "Mod",
        BinOp::Add => "Add",
        BinOp::Sub => "Sub",
        BinOp::Lt => "Lt",
        BinOp::Gt => "Gt",
        BinOp::Le => "Le",
        BinOp::Ge => "Ge",
        BinOp::Eq => "Eq",
        BinOp::Ne => "Ne",
        BinOp::And => "And",
        BinOp::Or => "Or",
    }
}

/// 字符串 → Rust 字符串字面量（含转义；非 ASCII 原样保留，源文件 UTF-8）。
fn rust_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7F => {
                let _ = write!(out, "\\u{{{:X}}}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gen(src: &str) -> String {
        Template::parse(src).expect("解析失败").to_rust_lib_source()
    }

    #[test]
    fn emits_entry_and_header() {
        let s = gen("<p>hi</p>");
        assert!(s.contains("razor_abi_version"), "缺少 ABI 版本导出");
        assert!(
            s.contains("pub extern \"C\" fn razor_render"),
            "缺少渲染入口"
        );
        assert!(s.contains(
            "fn __render(model: &Value, out: &mut String, escape: bool, __ic: &mut [u32])"
        ));
        assert!(s.contains("let mut __ic = [u32::MAX;"));
        assert!(s.contains("out.push_str(\"<p>hi</p>\");"));
    }

    #[test]
    fn literal_and_error_text_escaped() {
        let s = gen("@Model.V");
        assert!(s.contains(
            r#"rt::prop_get_ic(&rt::resolve_model_alias(model), &mut __ic[0], "V", "Model.V")?"#
        ));
        assert!(s.contains("rt::write_escaped_value("), "转义写出缺失");

        let s_root = gen("@Title");
        assert!(s_root.contains(r#"rt::get_root_prop(model, "Title")?"#));

        let s2 = gen("@Model.A.B");
        assert!(s2.contains(r#""A", "Model.A")?"#));
        assert!(s2.contains(r#""B", "Model.A.B")?"#));
    }

    #[test]
    fn string_literals_are_pooled() {
        let s = gen("@(Model.A + \"s\")");
        assert!(
            s.contains(r#"let __lit0: std::rc::Rc<str> = std::rc::Rc::from("s");"#),
            "字面量池装配缺失：{s}"
        );
        assert!(s.contains("Value::Str(__lit0.clone())"));
    }

    #[test]
    fn control_flow_and_vars() {
        let s = gen("@foreach (var row in Model.Rows) {<td>@row.Id</td>}");
        assert!(s.contains("rt::foreach_items(__t1, || \"Model.Rows\".to_string())?"));
        assert!(s.contains("let __u_row: Value ="));
        assert!(s.contains(r#"&__u_row, &mut __ic[1], "Id", "row.Id")?"#));

        let s2 = gen("@if (Model.Ok) {<b>y</b>} else {<i>n</i>}");
        assert!(s2.contains("rt::cond_bool(&__t1, || \"Model.Ok\".to_string())?"));

        let s3 = gen("@{ var x = 1; }@x");
        assert!(s3.contains("let __u_x: Value = __t1;"));
        assert!(s3.contains("let __t2: Value = __u_x.clone();"));
    }

    #[test]
    fn index_path_text_matches_interpreter_rule() {
        let s = gen("@Model.Sites[0].Name");
        assert!(
            s.contains(r#"rt::index_get(&rt::prop_get_ic(&rt::resolve_model_alias(model), &mut __ic[0], "Sites", "Model.Sites")?, &__t1, "Model.Sites[0]")?"#),
            "索引诊断路径应与解释器 format_path 一致：{s}"
        );
        assert!(s.contains(r#""Name", "Model.Sites[0].Name")?"#));
    }

    #[test]
    fn rust_string_escape() {
        assert_eq!(rust_str("a\"b\\c\nd\te"), r#""a\"b\\c\nd\te""#);
        assert_eq!(rust_str("站点"), "\"站点\"");
        assert_eq!(rust_str("\u{1}"), r#""\u{1}""#);
    }
}
