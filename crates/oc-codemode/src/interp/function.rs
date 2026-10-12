//! Function objects: program closures and native built-ins, plus the helpers that install built-ins.
//! Port of `interpreter/native.ts` and the callable half of `objects.ts`.

use std::cell::RefCell;
use std::future::Future;
use std::rc::Rc;

use futures::future::LocalBoxFuture;

use super::errors::{type_error, JsResult};
use super::{describe_value, Env, Rt, Site};
use crate::ast;
use crate::object::{define, Obj, ObjKind, FROZEN, HIDDEN, READONLY};
use crate::value::{JsStr, Value};

/// What a native body produces: a value now, or a future that may run program code.
pub enum NResult {
    Ready(JsResult<Value>),
    Future(LocalBoxFuture<'static, JsResult<Value>>),
}

impl NResult {
    pub fn ok(value: impl Into<Value>) -> NResult {
        NResult::Ready(Ok(value.into()))
    }
    pub fn err(e: super::errors::Thrown) -> NResult {
        NResult::Ready(Err(e))
    }
    pub fn future(f: impl Future<Output = JsResult<Value>> + 'static) -> NResult {
        NResult::Future(Box::pin(f))
    }
    pub async fn resolve(self) -> JsResult<Value> {
        match self {
            NResult::Ready(r) => r,
            NResult::Future(f) => f.await,
        }
    }
}

impl From<JsResult<Value>> for NResult {
    fn from(r: JsResult<Value>) -> Self {
        NResult::Ready(r)
    }
}

pub type NativeCall = Rc<dyn Fn(&Rt, Site, Value, Vec<Value>) -> NResult>;
pub type NativeConstruct = Rc<dyn Fn(&Rt, Site, Vec<Value>, Obj) -> NResult>;

pub struct NativeFn {
    pub call: NativeCall,
    /// `new name(...)`; without it the function is not a constructor.
    pub construct: Option<NativeConstruct>,
    /// Whether callback sites (array methods, replacers, promise reactions) admit this function.
    pub callback: bool,
}

pub struct Closure {
    pub func: Rc<ast::Function>,
    pub env: RefCell<Option<Env>>,
}

pub enum FuncImpl {
    Closure(Rc<Closure>),
    Native(Rc<NativeFn>),
}

pub struct FuncKind {
    /// The `length` a function was created with (used by `bind`).
    pub length: usize,
    pub imp: FuncImpl,
}

impl FuncKind {
    pub fn native(native: NativeFn, length: usize) -> FuncKind {
        FuncKind { length, imp: FuncImpl::Native(Rc::new(native)) }
    }
    pub fn teardown(&self) -> Box<dyn std::any::Any> {
        match &self.imp {
            FuncImpl::Closure(c) => Box::new(c.env.borrow_mut().take()),
            FuncImpl::Native(_) => Box::new(()),
        }
    }
}

impl Obj {
    pub fn func(&self) -> Option<&FuncKind> {
        match self.kind() {
            ObjKind::Function(f) => Some(f),
            _ => None,
        }
    }
    pub fn native_fn(&self) -> Option<&Rc<NativeFn>> {
        match self.func() {
            Some(FuncKind { imp: FuncImpl::Native(n), .. }) => Some(n),
            _ => None,
        }
    }
    pub fn closure(&self) -> Option<&Rc<Closure>> {
        match self.func() {
            Some(FuncKind { imp: FuncImpl::Closure(c), .. }) => Some(c),
            _ => None,
        }
    }
}

/// A callable object with `length` and `name`.
pub fn make_function(proto: &Obj, name: &str, length: usize, imp: FuncImpl) -> Obj {
    let obj = Obj::new(Some(proto.clone()), ObjKind::Function(FuncKind { length, imp }));
    define(&obj, "length", Value::Number(length as f64), READONLY);
    define(&obj, "name", Value::String(JsStr::from(name)), READONLY);
    obj
}

pub fn native(rt_builtins_function: &Obj, name: &str, length: usize, native: NativeFn) -> Obj {
    make_function(rt_builtins_function, name, length, FuncImpl::Native(Rc::new(native)))
}

/// A plain native function.
pub fn fn_(
    function_proto: &Obj,
    name: &str,
    length: usize,
    call: impl Fn(&Rt, Site, Value, Vec<Value>) -> NResult + 'static,
) -> Obj {
    native(function_proto, name, length, NativeFn { call: Rc::new(call), construct: None, callback: true })
}

pub type Method = (&'static str, usize, NativeCall);

pub fn method(
    name: &'static str,
    length: usize,
    call: impl Fn(&Rt, Site, Value, Vec<Value>) -> NResult + 'static,
) -> Method {
    (name, length, Rc::new(call))
}

/// Installs hidden methods on `target`.
pub fn methods(function_proto: &Obj, target: &Obj, table: Vec<Method>) {
    for (name, length, call) in table {
        let f = native(function_proto, name, length, NativeFn { call, construct: None, callback: true });
        define(target, name, Value::Object(f), HIDDEN);
    }
}

/// Installs frozen constants on `target`.
pub fn constants(target: &Obj, table: Vec<(&str, Value)>) {
    for (name, value) in table {
        define(target, name, value, FROZEN);
    }
}

/// A constructor wired to its prototype: `C.prototype === proto` and `proto.constructor === C`.
pub fn constructor(function_proto: &Obj, proto: &Obj, name: &str, length: usize, native_fn: NativeFn) -> Obj {
    let ctor = native(function_proto, name, length, native_fn);
    define(&ctor, "prototype", Value::Object(proto.clone()), FROZEN);
    define(proto, "constructor", Value::Object(ctor.clone()), HIDDEN);
    ctor
}

/// The `call` of a constructor that JS requires to be invoked with `new`.
pub fn requires_new(name: &'static str) -> NativeCall {
    Rc::new(move |_, _, _, _| NResult::err(type_error(format!("Constructor {name} requires 'new'."))))
}

/// The instance prototype for `new` via `newTarget.prototype`, falling back to the built-in's own.
pub fn prototype_from(new_target: &Obj, fallback: &Obj) -> Obj {
    let props = new_target.0.props.borrow();
    match props.get(&crate::value::PropKey::str("prototype")) {
        Some(crate::object::Slot::Data { value: Value::Object(o), .. }) => o.clone(),
        _ => fallback.clone(),
    }
}

/// Narrows a method receiver to the built-in it belongs to, or throws the TypeError JS would.
pub fn receiver(this: &Value, method: &str, check: impl Fn(&Obj) -> bool) -> JsResult<Obj> {
    if let Value::Object(o) = this
        && check(o)
    {
        return Ok(o.clone());
    }
    Err(type_error(format!("{method} called on incompatible receiver {}.", describe_value(this))))
}

/// Argument `i` or `undefined`.
pub fn arg(args: &[Value], i: usize) -> Value {
    args.get(i).cloned().unwrap_or(Value::Undefined)
}
