//! Program values: JS primitives (UTF-16 strings, f64 numbers), the two confined symbols, objects, and tool
//! references.

use std::cell::RefCell;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use crate::object::Obj;

/// A JavaScript string: an immutable sequence of UTF-16 code units (lone surrogates allowed).
#[derive(Clone, Eq)]
pub struct JsStr(Rc<[u16]>);

thread_local! {
    static EMPTY: JsStr = JsStr(Rc::from(&[][..]));
}

impl JsStr {
    pub fn empty() -> JsStr {
        EMPTY.with(|e| e.clone())
    }
    pub fn from_units(units: Vec<u16>) -> JsStr {
        JsStr(Rc::from(units))
    }
    pub fn from_slice(units: &[u16]) -> JsStr {
        JsStr(Rc::from(units))
    }
    pub fn from_rc(units: Rc<[u16]>) -> JsStr {
        JsStr(units)
    }
    pub fn units(&self) -> &[u16] {
        &self.0
    }
    pub fn rc(&self) -> &Rc<[u16]> {
        &self.0
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    /// Lossy conversion (lone surrogates become U+FFFD).
    pub fn to_rust(&self) -> String {
        String::from_utf16_lossy(&self.0)
    }
    pub fn ptr_eq(a: &JsStr, b: &JsStr) -> bool {
        Rc::ptr_eq(&a.0, &b.0)
    }
    pub fn concat(&self, other: &JsStr) -> JsStr {
        if other.is_empty() {
            return self.clone();
        }
        if self.is_empty() {
            return other.clone();
        }
        let mut v = Vec::with_capacity(self.len() + other.len());
        v.extend_from_slice(&self.0);
        v.extend_from_slice(&other.0);
        JsStr::from_units(v)
    }
    pub fn slice(&self, start: usize, end: usize) -> JsStr {
        if start == 0 && end == self.len() {
            return self.clone();
        }
        JsStr::from_slice(&self.0[start..end.max(start)])
    }
    /// True when the string equals the ASCII/Unicode text `s`.
    pub fn eq_str(&self, s: &str) -> bool {
        let mut it = s.encode_utf16();
        for &u in self.0.iter() {
            match it.next() {
                Some(c) if c == u => {}
                _ => return false,
            }
        }
        it.next().is_none()
    }
}

impl PartialEq for JsStr {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0) || self.0[..] == other.0[..]
    }
}

impl Hash for JsStr {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0[..].hash(state)
    }
}

impl PartialOrd for JsStr {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for JsStr {
    /// Code-unit order, like JS `<` on strings.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0[..].cmp(&other.0[..])
    }
}

impl From<&str> for JsStr {
    fn from(s: &str) -> Self {
        JsStr::from_units(s.encode_utf16().collect())
    }
}

impl From<String> for JsStr {
    fn from(s: String) -> Self {
        JsStr::from(s.as_str())
    }
}

impl From<&String> for JsStr {
    fn from(s: &String) -> Self {
        JsStr::from(s.as_str())
    }
}

impl fmt::Display for JsStr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_rust())
    }
}

impl fmt::Debug for JsStr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.to_rust())
    }
}

/// The only symbols a program can hold.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Sym {
    Iterator,
    AsyncIterator,
}

impl Sym {
    pub fn description(self) -> &'static str {
        match self {
            Sym::Iterator => "Symbol(Symbol.iterator)",
            Sym::AsyncIterator => "Symbol(Symbol.asyncIterator)",
        }
    }
}

/// A path into the host tool tree (`tools.a.b`). Each property read creates a fresh reference, as upstream does.
#[derive(Debug)]
pub struct ToolRefData {
    pub path: Vec<Rc<str>>,
}

#[derive(Clone, Debug)]
pub struct ToolRef(pub Rc<ToolRefData>);

impl ToolRef {
    pub fn new(path: Vec<Rc<str>>) -> ToolRef {
        ToolRef(Rc::new(ToolRefData { path }))
    }
    pub fn path(&self) -> &[Rc<str>] {
        &self.0.path
    }
}

/// Every value a program can hold.
#[derive(Clone, Default)]
pub enum Value {
    #[default]
    Undefined,
    Null,
    Bool(bool),
    Number(f64),
    String(JsStr),
    Symbol(Sym),
    Object(Obj),
    Tool(ToolRef),
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Undefined => write!(f, "undefined"),
            Value::Null => write!(f, "null"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Number(n) => write!(f, "{}", crate::number::to_string(*n)),
            Value::String(s) => write!(f, "{s:?}"),
            Value::Symbol(s) => write!(f, "{}", s.description()),
            Value::Object(o) => write!(f, "[object {}]", o.tag()),
            Value::Tool(t) => write!(f, "[tool {}]", t.path().join(".")),
        }
    }
}

impl Value {
    pub fn str(s: &str) -> Value {
        Value::String(JsStr::from(s))
    }
    pub fn num(n: f64) -> Value {
        Value::Number(n)
    }
    pub fn is_nullish(&self) -> bool {
        matches!(self, Value::Undefined | Value::Null)
    }
    pub fn is_undefined(&self) -> bool {
        matches!(self, Value::Undefined)
    }
    pub fn as_object(&self) -> Option<&Obj> {
        match self {
            Value::Object(o) => Some(o),
            _ => None,
        }
    }
    pub fn as_number(&self) -> Option<f64> {
        match self {
            Value::Number(n) => Some(*n),
            _ => None,
        }
    }
    pub fn as_string(&self) -> Option<&JsStr> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }
    /// JS truthiness (`Boolean(value)`); objects and tool references are truthy.
    pub fn truthy(&self) -> bool {
        match self {
            Value::Undefined | Value::Null => false,
            Value::Bool(b) => *b,
            Value::Number(n) => !(*n == 0.0 || n.is_nan()),
            Value::String(s) => !s.is_empty(),
            Value::Symbol(_) | Value::Object(_) | Value::Tool(_) => true,
        }
    }
    /// `===`
    pub fn strict_equals(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Undefined, Value::Undefined) | (Value::Null, Value::Null) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Number(a), Value::Number(b)) => a == b,
            (Value::String(a), Value::String(b)) => a == b,
            (Value::Symbol(a), Value::Symbol(b)) => a == b,
            (Value::Object(a), Value::Object(b)) => Obj::ptr_eq(a, b),
            (Value::Tool(a), Value::Tool(b)) => Rc::ptr_eq(&a.0, &b.0),
            _ => false,
        }
    }
    /// `Object.is`
    pub fn same_value(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Number(a), Value::Number(b)) => {
                if a.is_nan() && b.is_nan() {
                    return true;
                }
                a == b && a.is_sign_negative() == b.is_sign_negative()
            }
            _ => self.strict_equals(other),
        }
    }
    /// SameValueZero (`includes`, Map/Set keys).
    pub fn same_value_zero(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Number(a), Value::Number(b)) => (a.is_nan() && b.is_nan()) || a == b,
            _ => self.strict_equals(other),
        }
    }
    /// The JS `typeof` of a primitive (objects answer through [`crate::interp::typeof_value`]).
    pub fn js_typeof(&self) -> &'static str {
        match self {
            Value::Undefined => "undefined",
            Value::Null => "object",
            Value::Bool(_) => "boolean",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Symbol(_) => "symbol",
            Value::Object(o) => {
                if o.is_callable() {
                    "function"
                } else {
                    "object"
                }
            }
            Value::Tool(t) => {
                if t.path().is_empty() {
                    "object"
                } else {
                    "function"
                }
            }
        }
    }
}

impl From<f64> for Value {
    fn from(n: f64) -> Self {
        Value::Number(n)
    }
}
impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}
impl From<JsStr> for Value {
    fn from(s: JsStr) -> Self {
        Value::String(s)
    }
}
impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::String(JsStr::from(s))
    }
}
impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::String(JsStr::from(s))
    }
}
impl From<Obj> for Value {
    fn from(o: Obj) -> Self {
        Value::Object(o)
    }
}

/// A property key after ToPropertyKey.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum PropKey {
    Str(JsStr),
    Sym(Sym),
}

impl PropKey {
    pub fn str(s: &str) -> PropKey {
        PropKey::Str(JsStr::from(s))
    }
    pub fn as_str(&self) -> Option<&JsStr> {
        match self {
            PropKey::Str(s) => Some(s),
            PropKey::Sym(_) => None,
        }
    }
    /// `String(key)` as diagnostics show it.
    pub fn display(&self) -> String {
        match self {
            PropKey::Str(s) => s.to_rust(),
            PropKey::Sym(s) => s.description().to_string(),
        }
    }
    pub fn to_value(&self) -> Value {
        match self {
            PropKey::Str(s) => Value::String(s.clone()),
            PropKey::Sym(s) => Value::Symbol(*s),
        }
    }
}

impl From<&str> for PropKey {
    fn from(s: &str) -> Self {
        PropKey::str(s)
    }
}
impl From<JsStr> for PropKey {
    fn from(s: JsStr) -> Self {
        PropKey::Str(s)
    }
}
impl From<Sym> for PropKey {
    fn from(s: Sym) -> Self {
        PropKey::Sym(s)
    }
}

/// A key in a Map/Set using SameValueZero identity.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum MapKey {
    Undefined,
    Null,
    Bool(bool),
    Number(u64),
    String(JsStr),
    Symbol(Sym),
    Ptr(usize),
}

impl MapKey {
    pub fn of(value: &Value) -> MapKey {
        match value {
            Value::Undefined => MapKey::Undefined,
            Value::Null => MapKey::Null,
            Value::Bool(b) => MapKey::Bool(*b),
            Value::Number(n) => {
                let n = if *n == 0.0 { 0.0 } else { *n };
                MapKey::Number(if n.is_nan() { f64::NAN.to_bits() } else { n.to_bits() })
            }
            Value::String(s) => MapKey::String(s.clone()),
            Value::Symbol(s) => MapKey::Symbol(*s),
            Value::Object(o) => MapKey::Ptr(o.addr()),
            Value::Tool(t) => MapKey::Ptr(Rc::as_ptr(&t.0) as *const u8 as usize),
        }
    }
}

/// Shared mutable cell helper used across the runtime.
pub type Shared<T> = Rc<RefCell<T>>;
