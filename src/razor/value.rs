//! Razor 子集模板引擎：模型值系统。
//!
//! 页面模型以 [`Value`] 树传入（通常经 [`serde_json::Value`] 转换而来），
//! 与 C# 端模型字段按属性名对应；文本化规则对齐 C# 语义（见架构文档 2.1）。

use std::fmt;
use std::rc::Rc;

/// 对象节点。键值对按插入序保存（与 serde_json `preserve_order` 行为一致），
/// 查找为线性扫描——页面模型字段量小，v0 不引入额外索引依赖。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Object {
    entries: Vec<(String, Value)>,
}

impl Object {
    /// 创建空对象。
    pub fn new() -> Self {
        Self::default()
    }

    /// 设置键值（已存在则覆盖），返回自身以支持链式构造。
    pub fn set(&mut self, key: impl Into<String>, value: impl Into<Value>) -> &mut Self {
        let key = key.into();
        let value = value.into();
        if let Some(slot) = self.entries.iter_mut().find(|(k, _)| *k == key) {
            slot.1 = value;
        } else {
            self.entries.push((key, value));
        }
        self
    }

    /// 按键取值。
    pub fn get(&self, key: &str) -> Option<&Value> {
        // 手写循环（热路径，避免迭代器/闭包层开销；字段量小，线性扫描足够）
        for (k, v) in &self.entries {
            if k == key {
                return Some(v);
            }
        }
        None
    }

    /// 是否包含键。
    pub fn contains_key(&self, key: &str) -> bool {
        self.entries.iter().any(|(k, _)| k == key)
    }

    /// 键值对数量。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 是否为空对象。
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 按键值对插入序迭代。
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v))
    }
}

/// 模型值。
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// 空值，文本化为空串（对齐 C# Razor 对 null 的输出）
    Null,
    /// 布尔，文本化为 `True`/`False`（对齐 C# `Boolean.ToString()`）
    Bool(bool),
    /// 64 位整数
    Int(i64),
    /// 双精度浮点，文本化为最短往返形式
    Float(f64),
    /// 字符串（`Rc<str>` 共享，克隆为引用计数递增）
    Str(Rc<str>),
    /// 列表（`@foreach` 的迭代对象；`Rc` 共享，克隆为引用计数递增）
    List(Rc<Vec<Value>>),
    /// 对象（按属性名访问；`Rc` 共享，克隆为引用计数递增）
    Object(Rc<Object>),
}

impl Value {
    /// 创建对象构造器（链式 `set`）。
    pub fn object() -> Object {
        Object::new()
    }

    /// 创建列表值。
    pub fn list<T: Into<Value>>(items: impl IntoIterator<Item = T>) -> Self {
        Value::List(Rc::new(items.into_iter().map(Into::into).collect()))
    }

    /// 是否为空值。
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// 对象属性访问（非对象返回 `None`）。
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(o) => o.get(key),
            _ => None,
        }
    }

    /// 列表下标访问（越界或非列表返回 `None`）。
    pub fn index(&self, i: usize) -> Option<&Value> {
        match self {
            Value::List(items) => items.get(i),
            _ => None,
        }
    }

    /// 按 C# 语义做文本化：`null`→空串、`Bool`→`True/False`、`Int`→十进制、`Float`→最短往返。
    pub fn to_text(&self) -> String {
        let mut s = String::new();
        self.write_text_into(&mut s);
        s
    }

    /// 按 C# 语义把文本追加到 `out`（避免中间 `String` 分配，渲染器热路径使用）。
    #[inline(always)]
    pub fn write_text_into(&self, out: &mut String) {
        use std::fmt::Write as _;
        match self {
            Value::Null => {}
            Value::Bool(b) => out.push_str(if *b { "True" } else { "False" }),
            Value::Int(i) => {
                let _ = write!(out, "{i}");
            }
            Value::Float(f) => {
                if f.is_nan() {
                    out.push_str("NaN");
                } else if *f == f64::INFINITY {
                    out.push('∞');
                } else if *f == f64::NEG_INFINITY {
                    out.push_str("-∞");
                } else {
                    let _ = write!(out, "{f}");
                }
            }
            Value::Str(s) => out.push_str(s),
            Value::List(_) | Value::Object(_) => {
                let _ = write!(out, "{self}");
            }
        }
    }
}

impl fmt::Display for Value {
    /// 调试用显示（列表/对象为近似 JSON 输出；页面输出请用 [`Value::to_text`]）。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "null"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int(i) => write!(f, "{i}"),
            Value::Float(x) => write!(f, "{x}"),
            Value::Str(s) => write!(f, "{s}"),
            Value::List(items) => {
                write!(f, "[")?;
                for (i, v) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ",")?;
                    }
                    write!(f, "{v}")?;
                }
                write!(f, "]")
            }
            Value::Object(o) => {
                write!(f, "{{")?;
                for (i, (k, v)) in o.iter().enumerate() {
                    if i > 0 {
                        write!(f, ",")?;
                    }
                    write!(f, "\"{k}\":{v}")?;
                }
                write!(f, "}}")
            }
        }
    }
}

impl From<serde_json::Value> for Value {
    fn from(v: serde_json::Value) -> Self {
        match v {
            serde_json::Value::Null => Value::Null,
            serde_json::Value::Bool(b) => Value::Bool(b),
            serde_json::Value::Number(n) => match n.as_i64() {
                Some(i) => Value::Int(i),
                None => Value::Float(n.as_f64().unwrap_or(0.0)),
            },
            serde_json::Value::String(s) => Value::Str(s.into()),
            serde_json::Value::Array(a) => {
                Value::List(Rc::new(a.into_iter().map(Value::from).collect()))
            }
            serde_json::Value::Object(m) => {
                let mut o = Object::new();
                for (k, val) in m {
                    o.set(k, Value::from(val));
                }
                Value::Object(Rc::new(o))
            }
        }
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::Str(Rc::from(s))
    }
}

impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::Str(s.into())
    }
}

impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}

impl From<i64> for Value {
    fn from(i: i64) -> Self {
        Value::Int(i)
    }
}

impl From<i32> for Value {
    fn from(i: i32) -> Self {
        Value::Int(i as i64)
    }
}

impl From<u32> for Value {
    fn from(i: u32) -> Self {
        Value::Int(i as i64)
    }
}

impl From<f64> for Value {
    fn from(x: f64) -> Self {
        Value::Float(x)
    }
}

impl From<f32> for Value {
    fn from(x: f32) -> Self {
        Value::Float(x as f64)
    }
}

impl<T: Into<Value>> From<Vec<T>> for Value {
    fn from(items: Vec<T>) -> Self {
        Value::List(Rc::new(items.into_iter().map(Into::into).collect()))
    }
}

impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(v: Option<T>) -> Self {
        match v {
            Some(x) => x.into(),
            None => Value::Null,
        }
    }
}

impl From<Object> for Value {
    fn from(o: Object) -> Self {
        Value::Object(Rc::new(o))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_conversion_preserves_csharp_semantics() {
        let json = serde_json::json!({
            "Name": "站点",
            "Enable": true,
            "Port": 8282,
            "Tags": ["a", "b"],
            "Ratio": 0.5,
            "None": null
        });
        let v = Value::from(json);
        assert_eq!(v.get("Name").unwrap().to_text(), "站点");
        assert_eq!(v.get("Enable").unwrap().to_text(), "True");
        assert_eq!(v.get("Port").unwrap().to_text(), "8282");
        assert_eq!(v.get("None").unwrap().to_text(), "");
        assert_eq!(v.get("Tags").unwrap().index(1).unwrap().to_text(), "b");
        assert_eq!(v.get("Ratio").unwrap().to_text(), "0.5");
        assert!(v.get("Missing").is_none());
    }

    #[test]
    fn object_builder_keeps_insertion_order() {
        let mut o = Value::object();
        o.set("b", 1).set("a", "x");
        let v = Value::from(o);
        let keys: Vec<&str> = match &v {
            Value::Object(o) => o.iter().map(|(k, _)| k).collect(),
            _ => Vec::new(),
        };
        assert_eq!(keys, vec!["b", "a"]);
        assert!(v.get("a").is_some());
        assert!(v.get("missing").is_none());
    }

    #[test]
    fn float_special_values_match_csharp() {
        assert_eq!(Value::Float(f64::INFINITY).to_text(), "∞");
        assert_eq!(Value::Float(f64::NEG_INFINITY).to_text(), "-∞");
        assert_eq!(Value::Float(f64::NAN).to_text(), "NaN");
        assert_eq!(Value::Float(3.0).to_text(), "3");
        assert_eq!(Value::Float(0.5).to_text(), "0.5");
    }
}
