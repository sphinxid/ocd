//! The tree-walking interpreter: runtime context, scopes, and the shared helpers the evaluator and the standard
//! library build on. Port of `packages/codemode/src/interpreter`.

pub mod callback;
pub mod coerce;
pub mod errors;
pub mod eval;
pub mod exec;
pub mod extensions;
pub mod function;
pub mod generators;
pub mod globals;
pub mod iteration;
pub mod limits;
pub mod promises;
pub mod references;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use crate::ast::Loc;
use crate::object::{define, Obj, ObjKind, HIDDEN};
use crate::tool_runtime::ToolRuntime;
use crate::value::{JsStr, Value};
use errors::{ErrorType, JsResult, Thrown};

/// The program call a built-in is running under: where to locate failures born inside it, and how deep the stack
/// is there.
#[derive(Clone, Copy, Debug, Default)]
pub struct Site {
    pub node: Option<Loc>,
    pub depth: usize,
}

impl Site {
    pub fn at(node: Option<Loc>) -> Site {
        Site { node, depth: 0 }
    }
}

/// The built-in prototype objects of one runtime.
pub struct Builtins {
    pub object: Obj,
    pub function: Obj,
    pub array: Obj,
    pub string: Obj,
    pub number: Obj,
    pub boolean: Obj,
    pub date: Obj,
    pub regexp: Obj,
    pub map: Obj,
    pub set: Obj,
    pub weak_map: Obj,
    pub weak_set: Obj,
    pub url: Obj,
    pub url_search_params: Obj,
    pub headers: Obj,
    pub uint8_array: Obj,
    pub text_encoder: Obj,
    pub text_decoder: Obj,
    pub promise: Obj,
    pub iterator: Obj,
    pub iterator_helper: Obj,
    pub async_iterator: Obj,
    pub generator: Obj,
    pub async_generator: Obj,
    pub errors: Vec<(ErrorType, Obj)>,
}

impl Builtins {
    pub fn new() -> Builtins {
        let object = Obj::new(None, ObjKind::Ordinary);
        // Function.prototype is itself callable and returns undefined.
        let function = Obj::new(
            Some(object.clone()),
            ObjKind::Function(function::FuncKind::native(function::NativeFn {
                call: Rc::new(|_, _, _, _| function::NResult::Ready(Ok(Value::Undefined))),
                construct: None,
                callback: true,
            }, 0)),
        );
        define(&function, "length", Value::Number(0.0), crate::object::READONLY);
        define(&function, "name", Value::str(""), crate::object::READONLY);
        let plain = || Obj::ordinary(&object);
        let error = plain();
        define(&error, "name", Value::str("Error"), HIDDEN);
        define(&error, "message", Value::str(""), HIDDEN);
        let mut errors = vec![(ErrorType::Error, error.clone())];
        for t in &errors::ERROR_TYPES[1..] {
            let proto = Obj::ordinary(&error);
            define(&proto, "name", Value::str(t.name()), HIDDEN);
            define(&proto, "message", Value::str(""), HIDDEN);
            errors.push((*t, proto));
        }
        let iterator = plain();
        let async_iterator = plain();
        Builtins {
            array: Obj::array(&object, Vec::new()),
            string: plain(),
            number: plain(),
            boolean: plain(),
            date: plain(),
            regexp: plain(),
            map: plain(),
            set: plain(),
            weak_map: plain(),
            weak_set: plain(),
            url: plain(),
            url_search_params: plain(),
            headers: plain(),
            uint8_array: plain(),
            text_encoder: plain(),
            text_decoder: plain(),
            promise: plain(),
            iterator_helper: Obj::ordinary(&iterator),
            generator: Obj::ordinary(&iterator),
            async_generator: Obj::ordinary(&async_iterator),
            iterator,
            async_iterator,
            errors,
            function,
            object,
        }
    }

    pub fn error(&self, t: ErrorType) -> &Obj {
        &self.errors.iter().find(|(e, _)| *e == t).unwrap().1
    }

    /// The prototype a primitive reads its methods from without being boxed.
    pub fn primitive_prototype(&self, value: &Value) -> Option<&Obj> {
        match value {
            Value::String(_) => Some(&self.string),
            Value::Number(_) => Some(&self.number),
            Value::Bool(_) => Some(&self.boolean),
            _ => None,
        }
    }
}

impl Default for Builtins {
    fn default() -> Self {
        Builtins::new()
    }
}

/// FNV-1a hashing for identifier-keyed scope maps (small keys, no DoS concern inside one program run).
#[derive(Default, Clone, Copy)]
pub struct FnvHasher(u64);

impl std::hash::Hasher for FnvHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        let mut h = if self.0 == 0 { 0xcbf29ce484222325 } else { self.0 };
        for b in bytes {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        self.0 = h;
    }
}

pub type FnvBuild = std::hash::BuildHasherDefault<FnvHasher>;

/// A variable binding.
#[derive(Clone)]
pub struct Binding {
    pub mutable: bool,
    pub value: Value,
    pub initialized: bool,
}

/// One lexical scope and its parent chain.
pub struct Scope {
    pub vars: RefCell<HashMap<Rc<str>, Binding, FnvBuild>>,
    pub parent: Option<Env>,
}

pub type Env = Rc<Scope>;

thread_local! {
    static ENVS: RefCell<Vec<std::rc::Weak<Scope>>> = const { RefCell::new(Vec::new()) };
    static CURRENT: RefCell<Option<Rt>> = const { RefCell::new(None) };
}

pub fn new_env(parent: Option<&Env>) -> Env {
    let env = Rc::new(Scope { vars: RefCell::new(HashMap::default()), parent: parent.cloned() });
    ENVS.with(|e| {
        let mut e = e.borrow_mut();
        e.push(Rc::downgrade(&env));
        if e.len() % 4096 == 0 {
            e.retain(|w| w.strong_count() > 0);
        }
    });
    env
}

/// Clears every scope created on this thread so closure cycles are released.
pub fn teardown_envs() {
    let envs = ENVS.with(|e| std::mem::take(&mut *e.borrow_mut()));
    for weak in envs {
        if let Some(env) = weak.upgrade() {
            let vars = std::mem::take(&mut *env.vars.borrow_mut());
            drop(vars);
        }
    }
}

impl Scope {
    pub fn lookup(self: &Rc<Self>, name: &str) -> Option<Env> {
        let mut current = Some(self.clone());
        while let Some(scope) = current {
            if scope.vars.borrow().contains_key(name) {
                return Some(scope);
            }
            current = scope.parent.clone();
        }
        None
    }
    pub fn has_own(&self, name: &str) -> bool {
        self.vars.borrow().contains_key(name)
    }
    pub fn set_binding(&self, name: Rc<str>, binding: Binding) {
        self.vars.borrow_mut().insert(name, binding);
    }
}

/// Interrupt state shared with the host thread: a deadline and a cancellation flag.
pub struct Interrupt {
    pub deadline: Option<Instant>,
    pub cancelled: Arc<AtomicBool>,
    pub counter: Cell<u32>,
    pub fired: Cell<bool>,
}

impl Interrupt {
    pub fn check(&self) -> JsResult<()> {
        if self.fired.get() {
            return Err(Thrown::Interrupt);
        }
        let n = self.counter.get().wrapping_add(1);
        self.counter.set(n);
        if n % 256 == 0 {
            let expired = self.deadline.map(|d| Instant::now() >= d).unwrap_or(false);
            if expired || self.cancelled.load(Ordering::Relaxed) {
                self.fired.set(true);
                return Err(Thrown::Interrupt);
            }
        }
        Ok(())
    }
}

/// One program execution: the tool bridge, promise registry, captured logs, and the global scope built once.
pub struct Interp {
    pub builtins: Builtins,
    pub pending: promises::Pending,
    pub tools: ToolRuntime,
    pub logs: RefCell<Vec<String>>,
    /// Template objects by site: a tag sees the same `strings` array every time its literal is evaluated.
    pub templates: RefCell<HashMap<usize, Obj>>,
    pub global: RefCell<Option<Env>>,
    pub interrupt: Interrupt,
    pub extensions: Vec<Arc<dyn crate::host::Extension>>,
    /// `var` names per function body (memoized by body address).
    pub var_names: RefCell<HashMap<usize, Rc<Vec<Rc<str>>>>>,
    /// Whether a function reads `arguments` (memoized by function address).
    pub arguments_use: RefCell<HashMap<usize, bool>>,
}

pub type Rt = Rc<Interp>;

/// The runtime of the current interpreter thread.
pub fn rt() -> Rt {
    CURRENT.with(|c| c.borrow().clone().expect("no interpreter runtime on this thread"))
}

pub fn set_rt(rt: Option<Rt>) {
    CURRENT.with(|c| *c.borrow_mut() = rt);
}

/// `typeof` with tool references (`function` when callable).
pub fn typeof_value(value: &Value) -> &'static str {
    value.js_typeof()
}

/// How diagnostics refer to a value.
pub fn describe_value(value: &Value) -> String {
    match value {
        Value::Undefined => "undefined".into(),
        Value::Null => "null".into(),
        Value::Object(o) => o.describe(),
        Value::Tool(_) => "a tool reference".into(),
        other => format!("a {}", other.js_typeof()),
    }
}

/// ToPropertyKey on a primitive (or an opaque value, which keeps its built-in string form).
pub fn js_str(s: &str) -> JsStr {
    JsStr::from(s)
}
