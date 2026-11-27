//! The object model: own properties plus a prototype link, with one [`ObjKind`] per built-in kind of object.
//! Port of `interpreter/objects.ts`.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use indexmap::IndexMap;

use crate::interp::errors::{type_error, JsResult, PendingThrow};
use crate::interp::limits::check_array_length;
use crate::value::{JsStr, PropKey, Sym, Value};

/// Property attributes, as in a JS property descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attributes {
    pub writable: bool,
    pub enumerable: bool,
    pub configurable: bool,
}

/// Ordinary assignment: writable, enumerable, configurable.
pub const DATA: Attributes = Attributes { writable: true, enumerable: true, configurable: true };
/// Built-in methods and `constructor`: writable and configurable but hidden from enumeration.
pub const HIDDEN: Attributes = Attributes { writable: true, enumerable: false, configurable: true };
/// Function `name` and `length`: read-only but deletable.
pub const READONLY: Attributes = Attributes { writable: false, enumerable: false, configurable: true };
/// Constants such as `Math.PI` and a constructor's `prototype`.
pub const FROZEN: Attributes = Attributes { writable: false, enumerable: false, configurable: false };

pub type Getter = Rc<dyn Fn(&Value) -> JsResult<Value>>;
pub type Setter = Rc<dyn Fn(&Value, Value) -> JsResult<()>>;

/// One half of an accessor: a built-in's native closure, or a program function (2.4.1, #64908: getters and setters).
#[derive(Clone)]
pub enum Half<F> {
    Native(F),
    Program(Obj),
}

impl<F> Half<F> {
    pub fn program(&self) -> Option<&Obj> {
        match self {
            Half::Program(f) => Some(f),
            Half::Native(_) => None,
        }
    }
}

pub type GetHalf = Half<Getter>;
pub type SetHalf = Half<Setter>;

/// One own property: a data slot or an accessor pair.
#[derive(Clone)]
pub enum Slot {
    Data { value: Value, attrs: Attributes },
    Accessor { get: Option<GetHalf>, set: Option<SetHalf>, enumerable: bool, configurable: bool },
}

impl Slot {
    /// An accessor with a program half, which only program code may call (display shows `[Getter]` instead).
    pub fn is_program_accessor(&self) -> bool {
        matches!(self, Slot::Accessor { get, set, .. }
            if matches!(get, Some(Half::Program(_))) || matches!(set, Some(Half::Program(_))))
    }
    /// How display shows a program accessor in place of its value, in Node's words.
    pub fn accessor_label(&self) -> &'static str {
        match self {
            Slot::Accessor { get: Some(_), set: Some(_), .. } => "[Getter/Setter]",
            Slot::Accessor { get: Some(_), .. } => "[Getter]",
            _ => "[Setter]",
        }
    }
    /// An accessor with no getter: it reads as undefined and never crosses to the host.
    pub fn is_setter_only(&self) -> bool {
        matches!(self, Slot::Accessor { get: None, .. })
    }
    pub fn enumerable(&self) -> bool {
        match self {
            Slot::Data { attrs, .. } => attrs.enumerable,
            Slot::Accessor { enumerable, .. } => *enumerable,
        }
    }
    pub fn configurable(&self) -> bool {
        match self {
            Slot::Data { attrs, .. } => attrs.configurable,
            Slot::Accessor { configurable, .. } => *configurable,
        }
    }
    pub fn writable_data(&self) -> Option<bool> {
        match self {
            Slot::Data { attrs, .. } => Some(attrs.writable),
            Slot::Accessor { .. } => None,
        }
    }
}

pub struct ArrayData {
    /// `None` is a hole.
    pub items: Vec<Option<Value>>,
    /// The attributes every live element shares; `seal` and `freeze` narrow them since elements have no slots.
    pub elements: Attributes,
}

/// A `Uint8Array` view over a shared buffer (`subarray` shares bytes). `node` marks a Node `Buffer` (2.3.1, #64365):
/// a Uint8Array whose text conversions default to UTF-8 and which crosses to the host as a Buffer.
#[derive(Clone)]
pub struct BytesData {
    pub buffer: Rc<RefCell<Vec<u8>>>,
    pub offset: usize,
    pub length: usize,
    pub node: bool,
}

impl BytesData {
    pub fn new(bytes: Vec<u8>) -> BytesData {
        let length = bytes.len();
        BytesData { buffer: Rc::new(RefCell::new(bytes)), offset: 0, length, node: false }
    }
    /// A Node `Buffer` of these bytes.
    pub fn node(bytes: Vec<u8>) -> BytesData {
        BytesData { node: true, ..BytesData::new(bytes) }
    }
    pub fn to_vec(&self) -> Vec<u8> {
        self.buffer.borrow()[self.offset..self.offset + self.length].to_vec()
    }
    pub fn get(&self, index: usize) -> Option<u8> {
        if index < self.length { Some(self.buffer.borrow()[self.offset + index]) } else { None }
    }
    pub fn set(&self, index: usize, byte: u8) {
        if index < self.length {
            self.buffer.borrow_mut()[self.offset + index] = byte;
        }
    }
}

/// Map/Set storage with live iteration: deleted entries leave tombstones so cursors keep their position.
#[derive(Default)]
pub struct MapData {
    pub entries: Vec<Option<(Value, Value)>>,
    pub index: std::collections::HashMap<crate::value::MapKey, usize>,
    pub live: usize,
}

impl MapData {
    pub fn get(&self, key: &Value) -> Option<&Value> {
        let k = crate::value::MapKey::of(key);
        self.index.get(&k).and_then(|&i| self.entries[i].as_ref().map(|(_, v)| v))
    }
    pub fn has(&self, key: &Value) -> bool {
        self.index.contains_key(&crate::value::MapKey::of(key))
    }
    pub fn set(&mut self, key: Value, value: Value) {
        let k = crate::value::MapKey::of(&key);
        if let Some(&i) = self.index.get(&k) {
            if let Some(entry) = self.entries[i].as_mut() {
                entry.1 = value;
            }
            return;
        }
        // Map keys normalize -0 to +0.
        let key = match key {
            Value::Number(n) if n == 0.0 => Value::Number(0.0),
            other => other,
        };
        self.index.insert(k, self.entries.len());
        self.entries.push(Some((key, value)));
        self.live += 1;
    }
    pub fn delete(&mut self, key: &Value) -> bool {
        let k = crate::value::MapKey::of(key);
        match self.index.remove(&k) {
            Some(i) => {
                self.entries[i] = None;
                self.live -= 1;
                true
            }
            None => false,
        }
    }
    pub fn clear(&mut self) {
        for e in self.entries.iter_mut() {
            *e = None;
        }
        self.index.clear();
        self.live = 0;
    }
    pub fn size(&self) -> usize {
        self.live
    }
    /// Live entries in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = &(Value, Value)> {
        self.entries.iter().flatten()
    }
}

/// Weak collections: keys are program objects, held weakly.
#[derive(Default)]
pub struct WeakData {
    pub entries: std::collections::HashMap<usize, (Weak<ObjData>, Value)>,
}

impl WeakData {
    fn live(&self, key: &Obj) -> Option<&Value> {
        self.entries.get(&key.addr()).and_then(|(w, v)| if w.upgrade().is_some() { Some(v) } else { None })
    }
    pub fn get(&self, key: &Obj) -> Option<Value> {
        self.live(key).cloned()
    }
    pub fn has(&self, key: &Obj) -> bool {
        self.live(key).is_some()
    }
    pub fn set(&mut self, key: &Obj, value: Value) {
        self.entries.retain(|_, (w, _)| w.strong_count() > 0);
        self.entries.insert(key.addr(), (Rc::downgrade(&key.0), value));
    }
    pub fn delete(&mut self, key: &Obj) -> bool {
        let had = self.has(key);
        self.entries.remove(&key.addr());
        had
    }
}

/// Built-in kinds of objects. Each answers how it prints, coerces, iterates, and crosses to the host.
pub enum ObjKind {
    Ordinary,
    Array(RefCell<ArrayData>),
    /// An object with the [[ErrorData]] slot: the interpreter failure it materialized from, and the `    at` lines of
    /// its `stack` while that is not formatted yet (2.4.1, #64939: V8 formats `name: message` at the first read).
    Error(RefCell<Option<Rc<PendingThrow>>>, RefCell<Option<String>>),
    Function(crate::interp::function::FuncKind),
    Arguments,
    Promise(Rc<crate::interp::runner::PromiseState>),
    Generator(Rc<crate::interp::generators::GeneratorHandle>),
    Iterator(Rc<dyn crate::interp::iteration::Cursor>),
    Date(Cell<f64>),
    RegExp(crate::stdlib::regexp::RegExpData),
    Map(RefCell<MapData>),
    Set(RefCell<MapData>),
    WeakMap(RefCell<WeakData>),
    WeakSet(RefCell<WeakData>),
    URLSearchParams(Rc<crate::stdlib::url::SearchParams>),
    Headers(RefCell<crate::stdlib::headers::HeadersData>),
    URL(crate::stdlib::url::UrlData),
    Bytes(BytesData),
}

pub struct ObjData {
    pub proto: RefCell<Option<Obj>>,
    pub props: RefCell<IndexMap<PropKey, Slot>>,
    /// [[Extensible]]: cleared by `Object.preventExtensions`, `seal`, and `freeze`.
    pub extensible: Cell<bool>,
    pub kind: ObjKind,
}

/// A reference to a program object (identity = pointer).
#[derive(Clone)]
pub struct Obj(pub Rc<ObjData>);

thread_local! {
    /// Every object allocated on this interpreter thread, so teardown can break reference cycles.
    static REGISTRY: RefCell<Registry> = RefCell::new(Registry::default());
}

#[derive(Default)]
struct Registry {
    objects: Vec<Weak<ObjData>>,
    compact_at: usize,
}

/// Clears every object allocated on this thread (properties, prototypes, element storage), breaking cycles so the
/// memory is released when the execution ends.
pub fn teardown_registry() {
    let objects = REGISTRY.with(|r| std::mem::take(&mut r.borrow_mut().objects));
    for weak in objects {
        if let Some(obj) = weak.upgrade() {
            let props = std::mem::take(&mut *obj.props.borrow_mut());
            let proto = obj.proto.borrow_mut().take();
            let kind_parts = clear_kind(&obj.kind);
            drop(props);
            drop(proto);
            drop(kind_parts);
        }
    }
}

fn clear_kind(kind: &ObjKind) -> Box<dyn std::any::Any> {
    match kind {
        ObjKind::Array(a) => Box::new(std::mem::take(&mut a.borrow_mut().items)),
        ObjKind::Map(m) | ObjKind::Set(m) => Box::new(std::mem::take(&mut *m.borrow_mut())),
        ObjKind::WeakMap(m) | ObjKind::WeakSet(m) => Box::new(std::mem::take(&mut *m.borrow_mut())),
        ObjKind::Error(h, _) => Box::new(h.borrow_mut().take()),
        ObjKind::Function(f) => Box::new(f.teardown()),
        ObjKind::Promise(cell) => Box::new(cell.teardown()),
        ObjKind::Generator(g) => Box::new(g.teardown()),
        ObjKind::Iterator(c) => Box::new(c.teardown()),
        ObjKind::URLSearchParams(p) => Box::new(p.teardown()),
        ObjKind::URL(u) => Box::new(u.teardown()),
        _ => Box::new(()),
    }
}

impl Obj {
    pub fn new(proto: Option<Obj>, kind: ObjKind) -> Obj {
        let obj = Obj(Rc::new(ObjData {
            proto: RefCell::new(proto),
            props: RefCell::new(IndexMap::new()),
            extensible: Cell::new(true),
            kind,
        }));
        REGISTRY.with(|r| {
            let mut r = r.borrow_mut();
            r.objects.push(Rc::downgrade(&obj.0));
            if r.objects.len() > r.compact_at.max(1024) {
                r.objects.retain(|w| w.strong_count() > 0);
                r.compact_at = r.objects.len() * 2;
            }
        });
        obj
    }
    pub fn ordinary(proto: &Obj) -> Obj {
        Obj::new(Some(proto.clone()), ObjKind::Ordinary)
    }
    pub fn array(proto: &Obj, items: Vec<Value>) -> Obj {
        Obj::array_holes(proto, items.into_iter().map(Some).collect())
    }
    pub fn array_holes(proto: &Obj, items: Vec<Option<Value>>) -> Obj {
        Obj::new(Some(proto.clone()), ObjKind::Array(RefCell::new(ArrayData { items, elements: DATA })))
    }
    pub fn ptr_eq(a: &Obj, b: &Obj) -> bool {
        Rc::ptr_eq(&a.0, &b.0)
    }
    pub fn addr(&self) -> usize {
        Rc::as_ptr(&self.0) as *const u8 as usize
    }
    pub fn kind(&self) -> &ObjKind {
        &self.0.kind
    }
    pub fn proto(&self) -> Option<Obj> {
        self.0.proto.borrow().clone()
    }
    pub fn set_proto(&self, proto: Option<Obj>) {
        *self.0.proto.borrow_mut() = proto;
    }
    pub fn extensible(&self) -> bool {
        self.0.extensible.get()
    }

    /// The class name `Object.prototype.toString` reports: `[object Map]`.
    pub fn tag(&self) -> &'static str {
        match &self.0.kind {
            ObjKind::Ordinary => "Object",
            ObjKind::Array(_) => "Array",
            ObjKind::Error(..) => "Error",
            ObjKind::Function(_) => "Function",
            ObjKind::Arguments => "Arguments",
            ObjKind::Promise(_) => "Promise",
            ObjKind::Generator(_) => "Generator",
            ObjKind::Iterator(_) => "Iterator",
            ObjKind::Date(_) => "Date",
            ObjKind::RegExp(_) => "RegExp",
            ObjKind::Map(_) => "Map",
            ObjKind::Set(_) => "Set",
            ObjKind::WeakMap(_) => "WeakMap",
            ObjKind::WeakSet(_) => "WeakSet",
            ObjKind::URLSearchParams(_) => "URLSearchParams",
            ObjKind::Headers(_) => "Headers",
            ObjKind::URL(_) => "URL",
            ObjKind::Bytes(_) => "Uint8Array",
        }
    }

    /// How diagnostics refer to a value of this kind.
    pub fn describe(&self) -> String {
        match &self.0.kind {
            ObjKind::Array(_) => "an array".into(),
            ObjKind::Function(_) => "a function".into(),
            ObjKind::Promise(_) => "an un-awaited Promise".into(),
            ObjKind::Generator(_) => "a generator".into(),
            ObjKind::Iterator(_) => "an iterator".into(),
            _ => {
                let tag = self.tag();
                if tag == "Object" {
                    return "a data object".into();
                }
                let article = if tag.starts_with(['A', 'E', 'I', 'O']) { "an" } else { "a" };
                format!("{article} {tag}")
            }
        }
    }

    pub fn is_array(&self) -> bool {
        matches!(self.0.kind, ObjKind::Array(_))
    }
    pub fn is_callable(&self) -> bool {
        matches!(self.0.kind, ObjKind::Function(_))
    }
    pub fn is_error(&self) -> bool {
        matches!(self.0.kind, ObjKind::Error(..))
    }
    pub fn is_promise(&self) -> bool {
        matches!(self.0.kind, ObjKind::Promise(_))
    }
    /// Interpreter machinery a program can hold but never inspect, serialize, or hand to the host.
    pub fn is_opaque(&self) -> bool {
        matches!(
            self.0.kind,
            ObjKind::Function(_) | ObjKind::Promise(_) | ObjKind::Generator(_) | ObjKind::Iterator(_)
        )
    }
    /// A built-in object around a host value (Date, Map, URL, ...).
    pub fn is_wrapper(&self) -> bool {
        matches!(
            self.0.kind,
            ObjKind::Date(_)
                | ObjKind::RegExp(_)
                | ObjKind::Map(_)
                | ObjKind::Set(_)
                | ObjKind::WeakMap(_)
                | ObjKind::WeakSet(_)
                | ObjKind::URLSearchParams(_)
                | ObjKind::Headers(_)
                | ObjKind::URL(_)
                | ObjKind::Bytes(_)
        )
    }
    pub fn array_data(&self) -> Option<&RefCell<ArrayData>> {
        match &self.0.kind {
            ObjKind::Array(a) => Some(a),
            _ => None,
        }
    }
    /// Snapshot of an array's elements, holes as `undefined`.
    pub fn array_values(&self) -> Vec<Value> {
        match &self.0.kind {
            ObjKind::Array(a) => a.borrow().items.iter().map(|v| v.clone().unwrap_or_default()).collect(),
            _ => Vec::new(),
        }
    }
    pub fn array_len(&self) -> usize {
        match &self.0.kind {
            ObjKind::Array(a) => a.borrow().items.len(),
            _ => 0,
        }
    }
    pub fn bytes(&self) -> Option<&BytesData> {
        match &self.0.kind {
            ObjKind::Bytes(b) => Some(b),
            _ => None,
        }
    }
    pub fn date_time(&self) -> Option<f64> {
        match &self.0.kind {
            ObjKind::Date(t) => Some(t.get()),
            _ => None,
        }
    }
}

const MAX_ARRAY_INDEX: f64 = 4_294_967_295.0;

/// Canonical array index (`"0"`, `"12"`; not `"01"`), below 2^32 - 1.
pub fn parse_array_index(key: &JsStr) -> Option<usize> {
    let units = key.units();
    if units.is_empty() || units.len() > 10 {
        return None;
    }
    if units[0] == b'0' as u16 {
        return if units.len() == 1 { Some(0) } else { None };
    }
    let mut n: f64 = 0.0;
    for &u in units {
        if !(b'0' as u16..=b'9' as u16).contains(&u) {
            return None;
        }
        n = n * 10.0 + (u - b'0' as u16) as f64;
    }
    if n < MAX_ARRAY_INDEX { Some(n as usize) } else { None }
}

pub fn parse_array_index_key(key: &PropKey) -> Option<usize> {
    match key {
        PropKey::Str(s) => parse_array_index(s),
        PropKey::Sym(_) => None,
    }
}

fn is_indexed(target: &Obj) -> bool {
    matches!(target.0.kind, ObjKind::Array(_) | ObjKind::Bytes(_))
}

fn is_length(key: &PropKey) -> bool {
    matches!(key, PropKey::Str(s) if s.eq_str("length"))
}

/// Formats an error's `stack` on its first read: `name: message` as display reads them (data only, a program getter
/// reads as undefined), then the `    at` lines taken when the error was made.
fn format_stack(target: &Obj, trace: &RefCell<Option<String>>) {
    let Some(trace) = trace.borrow_mut().take() else { return };
    let key = PropKey::str("stack");
    if !target.0.props.borrow().contains_key(&key) {
        return;
    }
    let read = |name: &str| peek(target, &PropKey::str(name));
    let name = read("name");
    let message = read("message");
    let shown_name = if name.is_undefined() {
        "Error".to_string()
    } else {
        crate::interp::coerce::coerce_to_string(&name).to_rust()
    };
    let shown_message =
        if message.is_undefined() { String::new() } else { crate::interp::coerce::coerce_to_string(&message).to_rust() };
    let text = crate::interp::coerce::error_text(&shown_name, &shown_message) + &trace;
    if let Some(Slot::Data { value, .. }) = target.0.props.borrow_mut().get_mut(&key) {
        *value = Value::from(text);
    }
}

/// A property's value as display reads it: data, or a native getter's; a program accessor reads as undefined.
pub fn peek(target: &Obj, key: &PropKey) -> Value {
    let mut current = Some(target.clone());
    while let Some(obj) = current {
        if let Some(slot) = own(&obj, key) {
            return match slot {
                Slot::Data { value, .. } => value,
                Slot::Accessor { get: Some(Half::Native(g)), .. } => g(&Value::Object(target.clone())).unwrap_or_default(),
                Slot::Accessor { .. } => Value::Undefined,
            };
        }
        current = obj.proto();
    }
    Value::Undefined
}

/// The own property under `key`, including an array's live indexes and `length`.
pub fn own(target: &Obj, key: &PropKey) -> Option<Slot> {
    match &target.0.kind {
        ObjKind::Error(_, trace) if trace.borrow().is_some() && matches!(key, PropKey::Str(s) if s.eq_str("stack")) => {
            format_stack(target, trace);
        }
        ObjKind::Array(a) => {
            if let Some(at) = parse_array_index_key(key) {
                let data = a.borrow();
                return match data.items.get(at) {
                    Some(Some(v)) => Some(Slot::Data { value: v.clone(), attrs: data.elements }),
                    _ => None,
                };
            }
            if is_length(key) {
                let data = a.borrow();
                return Some(Slot::Data {
                    value: Value::Number(data.items.len() as f64),
                    attrs: Attributes { writable: data.elements.writable, enumerable: false, configurable: false },
                });
            }
        }
        ObjKind::Bytes(b) => {
            if let Some(at) = parse_array_index_key(key) {
                return b.get(at).map(|byte| Slot::Data { value: Value::Number(byte as f64), attrs: DATA });
            }
        }
        _ => {}
    }
    target.0.props.borrow().get(key).cloned()
}

fn read(slot: Slot, receiver: &Value) -> JsResult<Value> {
    match slot {
        Slot::Data { value, .. } => Ok(value),
        Slot::Accessor { get, .. } => match get {
            Some(Half::Native(g)) => g(receiver),
            Some(Half::Program(g)) => crate::interp::eval::call_sync(&g, receiver.clone(), Vec::new()),
            None => Ok(Value::Undefined),
        },
    }
}

/// [[Get]]'s lookup: the slot `key` names on `target` or its prototype chain.
pub fn lookup(target: &Obj, key: &PropKey) -> Option<Slot> {
    let mut current = Some(target.clone());
    while let Some(obj) = current {
        if let Some(slot) = own(&obj, key) {
            return Some(slot);
        }
        current = obj.proto();
    }
    None
}

pub fn has_own(target: &Obj, key: &PropKey) -> bool {
    own(target, key).is_some()
}

pub fn get_own(target: &Obj, key: &PropKey) -> JsResult<Value> {
    match own(target, key) {
        Some(slot) => read(slot, &Value::Object(target.clone())),
        None => Ok(Value::Undefined),
    }
}

/// [[Get]]: walks the prototype chain; accessors see `receiver`, which is the primitive for wrapper prototypes.
pub fn get_with_receiver(target: &Obj, key: &PropKey, receiver: &Value) -> JsResult<Value> {
    let mut current = Some(target.clone());
    while let Some(obj) = current {
        if let Some(slot) = own(&obj, key) {
            return read(slot, receiver);
        }
        current = obj.proto();
    }
    Ok(Value::Undefined)
}

pub fn get(target: &Obj, key: &PropKey) -> JsResult<Value> {
    get_with_receiver(target, key, &Value::Object(target.clone()))
}

/// Convenience: `get` by a Rust string key.
pub fn get_str(target: &Obj, key: &str) -> JsResult<Value> {
    get(target, &PropKey::str(key))
}

pub fn has(target: &Obj, key: &PropKey) -> bool {
    let mut current = Some(target.clone());
    while let Some(obj) = current {
        if own(&obj, key).is_some() {
            return true;
        }
        current = obj.proto();
    }
    false
}

/// Whether `proto` is on `value`'s prototype chain (OrdinaryHasInstance).
pub fn has_prototype(value: &Value, proto: &Obj) -> bool {
    let mut current = match value {
        Value::Object(o) => o.proto(),
        _ => None,
    };
    while let Some(obj) = current {
        if Obj::ptr_eq(&obj, proto) {
            return true;
        }
        current = obj.proto();
    }
    false
}

/// Why [[Set]] refused: an accessor with only a getter, or a read-only property.
pub fn failed_assignment(receiver: &Value, key: &PropKey) -> crate::interp::errors::Thrown {
    let target = match receiver {
        Value::Object(o) => Some(o.clone()),
        other => crate::interp::current_rt().and_then(|rt| rt.builtins.primitive_prototype(other).cloned()),
    };
    if let Some(Slot::Accessor { .. }) = target.and_then(|t| lookup(&t, key)) {
        return type_error(format!(
            "Cannot set property '{}' of {} which has only a getter.",
            key.display(),
            crate::interp::describe_value(receiver)
        ));
    }
    type_error(format!("Cannot assign to read only property '{}'.", key.display()))
}

/// Creating a property on a non-extensible object is the one [[Set]] failure with its own message.
pub fn reject_addition(target: &Obj, key: &str) -> JsResult<()> {
    if target.extensible() {
        return Ok(());
    }
    Err(type_error(format!("Cannot add property {key}, object is not extensible.")))
}

/// ToNumber for writes into a Uint8Array and array `length` (no program-defined methods).
fn number_for_write(value: &Value) -> f64 {
    crate::interp::coerce::coerce_to_number(value)
}

/// ToUint8 (modulo 256), as typed array element writes do.
pub fn to_uint8(n: f64) -> u8 {
    if !n.is_finite() {
        return 0;
    }
    let t = n.trunc();
    let m = t.rem_euclid(256.0);
    m as u8
}

/// Writes an element or `length`; `None` when `key` is neither.
fn write_element(target: &Obj, key: &PropKey, value: &Value) -> JsResult<Option<bool>> {
    match &target.0.kind {
        ObjKind::Bytes(b) => {
            if let Some(at) = parse_array_index_key(key) {
                b.set(at, to_uint8(number_for_write(value)));
                return Ok(Some(true));
            }
            Ok(None)
        }
        ObjKind::Array(a) => {
            if let Some(at) = parse_array_index_key(key) {
                let exists = matches!(a.borrow().items.get(at), Some(Some(_)));
                if !exists {
                    reject_addition(target, &at.to_string())?;
                    if at >= crate::interp::limits::MAX_ARRAY_LENGTH {
                        return Err(crate::interp::errors::range_error("Invalid array length"));
                    }
                }
                let mut data = a.borrow_mut();
                if at >= data.items.len() {
                    data.items.resize(at + 1, None);
                }
                data.items[at] = Some(value.clone());
                return Ok(Some(true));
            }
            if !is_length(key) {
                return Ok(None);
            }
            let length = match value {
                Value::Number(n) => *n,
                other => number_for_write(other),
            };
            if !(length.fract() == 0.0 && length.is_finite()) || length < 0.0 {
                return Err(crate::interp::errors::range_error("Invalid array length"));
            }
            let length = length as usize;
            {
                let data = a.borrow();
                // Shrinking deletes elements, which a sealed array forbids.
                if length < data.items.len() && !data.elements.configurable {
                    return Ok(Some(false));
                }
            }
            check_array_length(length as f64)?;
            a.borrow_mut().items.resize(length, None);
            Ok(Some(true))
        }
        _ => Ok(None),
    }
}

/// [[Set]]: an inherited setter or read-only property decides before an own data property is created.
pub fn set(target: &Obj, key: &PropKey, value: Value) -> JsResult<bool> {
    let mut current = Some(target.clone());
    while let Some(obj) = current {
        let slot = own(&obj, key);
        let Some(slot) = slot else {
            current = obj.proto();
            continue;
        };
        match slot {
            Slot::Accessor { set: setter, .. } => {
                return match setter {
                    None => Ok(false),
                    Some(Half::Native(s)) => {
                        s(&Value::Object(target.clone()), value)?;
                        Ok(true)
                    }
                    Some(Half::Program(s)) => {
                        crate::interp::eval::call_sync(&s, Value::Object(target.clone()), vec![value])?;
                        Ok(true)
                    }
                };
            }
            Slot::Data { attrs, .. } => {
                if !attrs.writable {
                    return Ok(false);
                }
                if !Obj::ptr_eq(&obj, target) {
                    break;
                }
                if is_indexed(target)
                    && let Some(written) = write_element(target, key, &value)?
                {
                    return Ok(written);
                }
                if let Some(Slot::Data { value: v, .. }) = target.0.props.borrow_mut().get_mut(key) {
                    *v = value;
                }
                return Ok(true);
            }
        }
    }
    if is_indexed(target)
        && let Some(written) = write_element(target, key, &value)?
    {
        return Ok(written);
    }
    reject_addition(target, &key.display())?;
    target.0.props.borrow_mut().insert(key.clone(), Slot::Data { value, attrs: DATA });
    Ok(true)
}

/// An error's `stack` replaced or deleted before its first read is never formatted.
fn drop_stack_trace(target: &Obj, key: &PropKey) {
    if let ObjKind::Error(_, trace) = &target.0.kind
        && matches!(key, PropKey::Str(s) if s.eq_str("stack"))
    {
        trace.borrow_mut().take();
    }
}

/// [[DefineOwnProperty]] for a data property, ignoring the chain.
pub fn define(target: &Obj, key: impl Into<PropKey>, value: Value, attrs: Attributes) {
    let key = key.into();
    drop_stack_trace(target, &key);
    if is_indexed(target) {
        if let Ok(Some(_)) = write_element(target, &key, &value) {
            return;
        }
        if parse_array_index_key(&key).is_some() || (target.is_array() && is_length(&key)) {
            return;
        }
    }
    target.0.props.borrow_mut().insert(key, Slot::Data { value, attrs });
}

/// `define` with ordinary data attributes.
pub fn define_data(target: &Obj, key: impl Into<PropKey>, value: Value) {
    define(target, key, value, DATA);
}

/// A built-in's native accessor.
pub fn define_accessor(target: &Obj, key: impl Into<PropKey>, get: Option<Getter>, set: Option<Setter>) {
    target.0.props.borrow_mut().insert(
        key.into(),
        Slot::Accessor { get: get.map(Half::Native), set: set.map(Half::Native), enumerable: false, configurable: true },
    );
}

/// An object literal's `get x() {}` or `set x(v) {}`: an existing accessor keeps its other half; a data property, or
/// no property, becomes an accessor with only this half.
pub fn define_accessor_half(target: &Obj, key: PropKey, getter: bool, f: Obj, enumerable: bool) {
    drop_stack_trace(target, &key);
    let mut props = target.0.props.borrow_mut();
    let (mut get, mut set) = match props.get(&key) {
        Some(Slot::Accessor { get, set, .. }) => (get.clone(), set.clone()),
        _ => (None, None),
    };
    if getter {
        get = Some(Half::Program(f));
    } else {
        set = Some(Half::Program(f));
    }
    props.insert(key, Slot::Accessor { get, set, enumerable, configurable: true });
}

/// A property descriptor a program gave: a field is `Some` when the descriptor has it (`"get" in desc`).
#[derive(Clone, Default)]
pub struct Descriptor {
    pub value: Option<Value>,
    pub writable: Option<bool>,
    /// `Some(None)`: the descriptor's `get` is `undefined`.
    pub get: Option<Option<Obj>>,
    pub set: Option<Option<Obj>>,
    pub enumerable: Option<bool>,
    pub configurable: Option<bool>,
}

impl Descriptor {
    pub fn is_accessor(&self) -> bool {
        self.get.is_some() || self.set.is_some()
    }
    pub fn is_data(&self) -> bool {
        self.value.is_some() || self.writable.is_some()
    }
}

thread_local! {
    /// The function a program sees for each native accessor half (`Object.getOwnPropertyDescriptor(Map.prototype,
    /// "size").get`, named `get size`), by the closure's address, and the half each one stands for.
    static COMPANIONS: RefCell<std::collections::HashMap<usize, Obj>> = RefCell::new(std::collections::HashMap::new());
    static COMPANION_HALVES: RefCell<std::collections::HashMap<usize, (Option<Getter>, Option<Setter>)>> =
        RefCell::new(std::collections::HashMap::new());
}

/// Forgets the companions of this thread's native accessors (teardown).
pub fn clear_companions() {
    COMPANIONS.with(|c| c.borrow_mut().clear());
    COMPANION_HALVES.with(|c| c.borrow_mut().clear());
}

fn closure_addr<T: ?Sized>(f: &Rc<T>) -> usize {
    Rc::as_ptr(f) as *const u8 as usize
}

/// The function a program sees for an accessor half: the program function, or a native half's companion, made the
/// first time it is asked for.
pub fn get_half_function(half: &GetHalf, key: &PropKey) -> Option<Obj> {
    match half {
        Half::Program(f) => Some(f.clone()),
        Half::Native(g) => companion(closure_addr(g), key, true, || {
            let g = g.clone();
            (Some(g.clone()), None, Rc::new(move |this: &Value, _: &[Value]| g(this)) as Rc<dyn Fn(&Value, &[Value]) -> JsResult<Value>>)
        }),
    }
}

pub fn set_half_function(half: &SetHalf, key: &PropKey) -> Option<Obj> {
    match half {
        Half::Program(f) => Some(f.clone()),
        Half::Native(s) => companion(closure_addr(s), key, false, || {
            let s = s.clone();
            (None, Some(s.clone()), Rc::new(move |this: &Value, args: &[Value]| {
                s(this, args.first().cloned().unwrap_or_default())?;
                Ok(Value::Undefined)
            }) as Rc<dyn Fn(&Value, &[Value]) -> JsResult<Value>>)
        }),
    }
}

#[allow(clippy::type_complexity)]
fn companion(
    addr: usize,
    key: &PropKey,
    getter: bool,
    make: impl FnOnce() -> (Option<Getter>, Option<Setter>, Rc<dyn Fn(&Value, &[Value]) -> JsResult<Value>>),
) -> Option<Obj> {
    if let Some(f) = COMPANIONS.with(|c| c.borrow().get(&addr).cloned()) {
        return Some(f);
    }
    let rt = crate::interp::current_rt()?;
    let (g, st, call) = make();
    let label = match key {
        PropKey::Str(s) => s.to_rust(),
        PropKey::Sym(sym) => format!("[{}]", sym.description().trim_start_matches("Symbol(").trim_end_matches(')')),
    };
    let name = format!("{} {label}", if getter { "get" } else { "set" });
    let f = crate::interp::function::native(
        &rt.builtins.function,
        &name,
        if getter { 0 } else { 1 },
        crate::interp::function::NativeFn {
            call: Rc::new(move |_, _, this, args| crate::interp::function::NResult::Ready(call(&this, &args))),
            construct: None,
            callback: true,
        },
    );
    COMPANIONS.with(|c| c.borrow_mut().insert(addr, f.clone()));
    COMPANION_HALVES.with(|c| c.borrow_mut().insert(f.addr(), (g, st)));
    Some(f)
}

/// The half to store for a function a program gives: a companion's native closure, so reads stay on the native path.
fn get_half_of(f: Obj) -> GetHalf {
    match COMPANION_HALVES.with(|c| c.borrow().get(&f.addr()).and_then(|(g, _)| g.clone())) {
        Some(g) => Half::Native(g),
        None => Half::Program(f),
    }
}

fn set_half_of(f: Obj) -> SetHalf {
    match COMPANION_HALVES.with(|c| c.borrow().get(&f.addr()).and_then(|(_, s)| s.clone())) {
        Some(s) => Half::Native(s),
        None => Half::Program(f),
    }
}

fn same_get(a: &Option<GetHalf>, b: &Option<GetHalf>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(Half::Program(x)), Some(Half::Program(y))) => Obj::ptr_eq(x, y),
        (Some(Half::Native(x)), Some(Half::Native(y))) => Rc::ptr_eq(x, y),
        _ => false,
    }
}

fn same_set(a: &Option<SetHalf>, b: &Option<SetHalf>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(Half::Program(x)), Some(Half::Program(y))) => Obj::ptr_eq(x, y),
        (Some(Half::Native(x)), Some(Half::Native(y))) => Rc::ptr_eq(x, y),
        _ => false,
    }
}

/// ValidateAndApplyPropertyDescriptor for an own property: `Ok(false)` where the spec answers false (the caller
/// throws), a TypeError for what the object model cannot hold (an accessor on an array element, elements whose
/// attributes differ from their array's shared ones). Everything is checked before anything changes.
pub fn define_own_property(target: &Obj, key: &PropKey, desc: &Descriptor) -> JsResult<bool> {
    let current = own(target, key);
    if current.is_none() && !target.extensible() {
        return Ok(false);
    }
    if let Some(c) = &current
        && !compatible(c, desc)
    {
        return Ok(false);
    }
    if is_indexed(target) {
        if let Some(at) = parse_array_index_key(key) {
            return define_element(target, at, current, desc);
        }
        if target.is_array() && is_length(key) {
            return define_length(target, desc);
        }
    }
    drop_stack_trace(target, key);
    let slot = match &current {
        None => created(desc),
        Some(c) => applied(c, desc),
    };
    target.0.props.borrow_mut().insert(key.clone(), slot);
    Ok(true)
}

// Whether `desc` may change a non-configurable `current` (a configurable one may change in any way).
fn compatible(current: &Slot, desc: &Descriptor) -> bool {
    if current.configurable() {
        return true;
    }
    if desc.configurable == Some(true) {
        return false;
    }
    if desc.enumerable.is_some_and(|e| e != current.enumerable()) {
        return false;
    }
    match current {
        Slot::Data { value, attrs } => {
            if desc.is_accessor() {
                return false;
            }
            if attrs.writable {
                return true;
            }
            desc.writable != Some(true) && !desc.value.as_ref().is_some_and(|v| !v.same_value(value))
        }
        Slot::Accessor { get, set, .. } => {
            if desc.is_data() {
                return false;
            }
            desc.get.as_ref().is_none_or(|g| same_get(&g.clone().map(get_half_of), get))
                && desc.set.as_ref().is_none_or(|s| same_set(&s.clone().map(set_half_of), set))
        }
    }
}

// A new property: what the descriptor gives, and false or undefined for what it leaves out.
fn created(desc: &Descriptor) -> Slot {
    let enumerable = desc.enumerable.unwrap_or(false);
    let configurable = desc.configurable.unwrap_or(false);
    if desc.is_accessor() {
        return Slot::Accessor {
            get: desc.get.clone().flatten().map(get_half_of),
            set: desc.set.clone().flatten().map(set_half_of),
            enumerable,
            configurable,
        };
    }
    Slot::Data {
        value: desc.value.clone().unwrap_or_default(),
        attrs: Attributes { writable: desc.writable.unwrap_or(false), enumerable, configurable },
    }
}

// An existing property with the descriptor's fields applied: a kind change keeps only the attributes.
fn applied(current: &Slot, desc: &Descriptor) -> Slot {
    let enumerable = desc.enumerable.unwrap_or(current.enumerable());
    let configurable = desc.configurable.unwrap_or(current.configurable());
    let kept = Descriptor { enumerable: Some(enumerable), configurable: Some(configurable), ..desc.clone() };
    match current {
        Slot::Data { .. } if desc.is_accessor() => created(&kept),
        Slot::Accessor { .. } if desc.is_data() => created(&kept),
        Slot::Data { value, attrs } => Slot::Data {
            value: desc.value.clone().unwrap_or_else(|| value.clone()),
            attrs: Attributes { writable: desc.writable.unwrap_or(attrs.writable), enumerable, configurable },
        },
        Slot::Accessor { get, set, .. } => Slot::Accessor {
            get: match &desc.get {
                Some(g) => g.clone().map(get_half_of),
                None => get.clone(),
            },
            set: match &desc.set {
                Some(s) => s.clone().map(set_half_of),
                None => set.clone(),
            },
            enumerable,
            configurable,
        },
    }
}

// An array or Uint8Array element: elements have no slots, so only data with the collection's own attributes fits.
fn define_element(target: &Obj, at: usize, current: Option<Slot>, desc: &Descriptor) -> JsResult<bool> {
    if desc.is_accessor() {
        return Err(type_error(
            "Accessor properties are not supported on array elements; store the value or use a plain object.",
        ));
    }
    if let Some(b) = target.bytes()
        && at >= b.length
    {
        return Ok(false);
    }
    let shared = match target.array_data() {
        Some(a) => a.borrow().elements,
        None => DATA,
    };
    let base = match &current {
        Some(Slot::Data { attrs, .. }) => *attrs,
        _ => Attributes { writable: false, enumerable: false, configurable: false },
    };
    let writable = desc.writable.unwrap_or(base.writable);
    let enumerable = desc.enumerable.unwrap_or(base.enumerable);
    let configurable = desc.configurable.unwrap_or(base.configurable);
    if writable != shared.writable || enumerable != shared.enumerable || configurable != shared.configurable {
        return Err(type_error(
            "Array elements share their attributes; only writable, enumerable and configurable equal to the array's are supported.",
        ));
    }
    if current.is_none() || desc.value.is_some() {
        let value = desc.value.clone().unwrap_or_default();
        match &target.0.kind {
            ObjKind::Bytes(b) => b.set(at, to_uint8(number_for_write(&value))),
            ObjKind::Array(a) => {
                if at >= crate::interp::limits::MAX_ARRAY_LENGTH {
                    return Err(crate::interp::errors::range_error("Invalid array length"));
                }
                let mut data = a.borrow_mut();
                if at >= data.items.len() {
                    data.items.resize(at + 1, None);
                }
                data.items[at] = Some(value);
            }
            _ => {}
        }
    }
    Ok(true)
}

// An array's `length`: its value, or making it read-only, which the shared element attributes can't separate.
fn define_length(target: &Obj, desc: &Descriptor) -> JsResult<bool> {
    if desc.is_accessor() || desc.configurable == Some(true) || desc.enumerable == Some(true) {
        return Ok(false);
    }
    let writable = target.array_data().map(|a| a.borrow().elements.writable).unwrap_or(true);
    if desc.writable == Some(false) && writable {
        return Err(type_error("Making an array's length read-only is not supported; use Object.freeze."));
    }
    if let Some(value) = &desc.value {
        let length = number_for_write(value);
        if length != target.array_len() as f64
            && write_element(target, &PropKey::str("length"), &Value::Number(length))? == Some(false)
        {
            return Ok(false);
        }
    }
    Ok(true)
}

/// [[Delete]]; false when the property is non-configurable.
pub fn remove(target: &Obj, key: &PropKey) -> bool {
    drop_stack_trace(target, key);
    match &target.0.kind {
        ObjKind::Bytes(b) => {
            if let Some(at) = parse_array_index_key(key) {
                return at >= b.length;
            }
        }
        ObjKind::Array(a) => {
            if let Some(at) = parse_array_index_key(key) {
                let mut data = a.borrow_mut();
                let exists = matches!(data.items.get(at), Some(Some(_)));
                if exists && !data.elements.configurable {
                    return false;
                }
                if exists {
                    data.items[at] = None;
                }
                return true;
            }
            if is_length(key) {
                return false;
            }
        }
        _ => {}
    }
    let mut props = target.0.props.borrow_mut();
    match props.get(key) {
        None => true,
        Some(slot) if !slot.configurable() => false,
        Some(_) => {
            props.shift_remove(key);
            true
        }
    }
}

/// JS order: array indexes, integer-like keys ascending, other strings, then symbols.
pub fn own_keys(target: &Obj) -> Vec<PropKey> {
    let mut out: Vec<PropKey> = Vec::new();
    match &target.0.kind {
        ObjKind::Array(a) => {
            let data = a.borrow();
            for (i, item) in data.items.iter().enumerate() {
                if item.is_some() {
                    out.push(PropKey::Str(index_key(i)));
                }
            }
            out.push(PropKey::str("length"));
        }
        ObjKind::Bytes(b) => {
            for i in 0..b.length {
                out.push(PropKey::Str(index_key(i)));
            }
        }
        _ => {}
    }
    let props = target.0.props.borrow();
    let mut integers: Vec<(usize, PropKey)> = Vec::new();
    let mut strings: Vec<PropKey> = Vec::new();
    let mut symbols: Vec<PropKey> = Vec::new();
    for key in props.keys() {
        match key {
            PropKey::Str(s) => match parse_array_index(s) {
                Some(i) => integers.push((i, key.clone())),
                None => strings.push(key.clone()),
            },
            PropKey::Sym(_) => symbols.push(key.clone()),
        }
    }
    integers.sort_by_key(|(i, _)| *i);
    out.extend(integers.into_iter().map(|(_, k)| k));
    out.extend(strings);
    out.extend(symbols);
    out
}

pub fn index_key(i: usize) -> JsStr {
    JsStr::from(i.to_string())
}

fn enumerable(target: &Obj, key: &PropKey) -> bool {
    own(target, key).map(|s| s.enumerable()).unwrap_or(false)
}

/// Own enumerable keys, including the iterator symbols; what spread and `Object.assign` copy.
pub fn enumerable_keys(target: &Obj) -> Vec<PropKey> {
    own_keys(target).into_iter().filter(|k| enumerable(target, k)).collect()
}

/// Own enumerable string keys: `Object.keys`.
pub fn keys(target: &Obj) -> Vec<JsStr> {
    own_keys(target)
        .into_iter()
        .filter_map(|k| match k {
            PropKey::Str(s) if enumerable(target, &PropKey::Str(s.clone())) => Some(s),
            _ => None,
        })
        .collect()
}

/// Own enumerable string entries: `Object.entries` and serialization. Each key's enumerability is checked as it is
/// visited, so a getter that deletes or hides a later key is seen.
pub fn entries(target: &Obj) -> JsResult<Vec<(JsStr, Value)>> {
    let mut out = Vec::new();
    for key in own_keys(target) {
        let PropKey::Str(name) = &key else { continue };
        if !enumerable(target, &key) {
            continue;
        }
        let value = get_own(target, &key)?;
        out.push((name.clone(), value));
    }
    Ok(out)
}

/// Own enumerable string entries as display shows them, never calling program getters: a program accessor reads as
/// its label (`[Getter]`), returned as `Err(label)`.
pub fn shown_entries(target: &Obj) -> Vec<(JsStr, Result<Value, &'static str>)> {
    let mut out = Vec::new();
    for name in keys(target) {
        let key = PropKey::Str(name.clone());
        match own(target, &key) {
            Some(slot) if slot.is_program_accessor() => out.push((name, Err(slot.accessor_label()))),
            Some(slot) => out.push((name, Ok(read(slot, &Value::Object(target.clone())).unwrap_or_default()))),
            None => {}
        }
    }
    out
}

pub fn record(proto: &Obj, fields: Vec<(&str, Value)>) -> Obj {
    let target = Obj::ordinary(proto);
    for (key, value) in fields {
        define_data(&target, key, value);
    }
    target
}

/// Copies own enumerable properties with [[Set]], skipping `skip`. Each key's enumerability is checked as it is
/// visited, so a getter that deletes or hides a later key is seen.
pub fn assign(target: &Obj, source: &Obj, skip: Option<&[PropKey]>) -> JsResult<()> {
    for key in own_keys(source) {
        if let Some(skip) = skip
            && skip.contains(&key)
        {
            continue;
        }
        if !enumerable(source, &key) {
            continue;
        }
        let value = get_own(source, &key)?;
        set(target, &key, value)?;
    }
    Ok(())
}

/// The iterator symbols as property keys.
pub fn sym_key(sym: Sym) -> PropKey {
    PropKey::Sym(sym)
}
