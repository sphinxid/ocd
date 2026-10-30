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

/// One own property: a data slot or a native accessor pair.
#[derive(Clone)]
pub enum Slot {
    Data { value: Value, attrs: Attributes },
    Accessor { get: Option<Getter>, set: Option<Setter>, enumerable: bool, configurable: bool },
}

impl Slot {
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

/// A `Uint8Array` view over a shared buffer (`subarray` shares bytes).
#[derive(Clone)]
pub struct BytesData {
    pub buffer: Rc<RefCell<Vec<u8>>>,
    pub offset: usize,
    pub length: usize,
}

impl BytesData {
    pub fn new(bytes: Vec<u8>) -> BytesData {
        let length = bytes.len();
        BytesData { buffer: Rc::new(RefCell::new(bytes)), offset: 0, length }
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
    /// An object with the [[ErrorData]] slot; `host` is the interpreter failure it materialized from.
    Error(RefCell<Option<Rc<PendingThrow>>>),
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
        ObjKind::Error(h) => Box::new(h.borrow_mut().take()),
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
            ObjKind::Error(_) => "Error",
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
        matches!(self.0.kind, ObjKind::Error(_))
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

/// The own property under `key`, including an array's live indexes and `length`.
pub fn own(target: &Obj, key: &PropKey) -> Option<Slot> {
    match &target.0.kind {
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
            Some(g) => g(receiver),
            None => Ok(Value::Undefined),
        },
    }
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
                    Some(s) => {
                        s(&Value::Object(target.clone()), value)?;
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

/// [[DefineOwnProperty]] for a data property, ignoring the chain.
pub fn define(target: &Obj, key: impl Into<PropKey>, value: Value, attrs: Attributes) {
    let key = key.into();
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

pub fn define_accessor(target: &Obj, key: impl Into<PropKey>, get: Option<Getter>, set: Option<Setter>) {
    target
        .0
        .props
        .borrow_mut()
        .insert(key.into(), Slot::Accessor { get, set, enumerable: false, configurable: true });
}

/// [[Delete]]; false when the property is non-configurable.
pub fn remove(target: &Obj, key: &PropKey) -> bool {
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

/// Own enumerable string entries: `Object.entries` and serialization.
pub fn entries(target: &Obj) -> JsResult<Vec<(JsStr, Value)>> {
    let mut out = Vec::new();
    for key in keys(target) {
        let value = get_own(target, &PropKey::Str(key.clone()))?;
        out.push((key, value));
    }
    Ok(out)
}

pub fn record(proto: &Obj, fields: Vec<(&str, Value)>) -> Obj {
    let target = Obj::ordinary(proto);
    for (key, value) in fields {
        define_data(&target, key, value);
    }
    target
}

/// Copies own enumerable properties with [[Set]], skipping `skip`.
pub fn assign(target: &Obj, source: &Obj, skip: Option<&[PropKey]>) -> JsResult<()> {
    for key in enumerable_keys(source) {
        if let Some(skip) = skip
            && skip.contains(&key)
        {
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
