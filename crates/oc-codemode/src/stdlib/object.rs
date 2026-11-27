//! `Object` statics and `Object.prototype`. Port of `stdlib/object.ts`.

use std::collections::HashSet;
use std::rc::Rc;

use crate::ast::Loc;
use crate::interp::callback::invoke;
use crate::interp::coerce::coerce_to_string;
use crate::interp::errors::{invalid_data, locate, type_error, JsResult, Thrown};
use crate::interp::function::{arg, constructor, method, methods, receiver, NResult, NativeFn};
use crate::interp::iteration::{self, preserve_consumer_error};
use crate::interp::references::{contains_opaque_reference, reject_circular_insertion_seen};
use crate::interp::{describe_value, Rt};
use crate::object::{
    define_own_property, Descriptor, define, define_data, entries, get_own, has_own, has_prototype, keys, own, own_keys, set, Obj,
    ObjKind, Slot, HIDDEN,
};
use crate::value::{JsStr, PropKey, Sym, Value};

// ToObject for enumeration.
pub fn enumerable_source(rt: &Rt, label: &str, value: &Value, node: Option<Loc>) -> JsResult<Obj> {
    let at = |e: Thrown| locate(e, node);
    match value {
        Value::Undefined | Value::Null => {
            Err(at(type_error(format!("{label} cannot convert {} to an object.", describe_value(value)))))
        }
        Value::Object(o) if o.is_promise() => Err(at(invalid_data(format!(
            "{label} received an un-awaited Promise; await it before inspecting the result."
        )))),
        Value::Tool(_) => Err(at(invalid_data(format!(
            "{label} cannot read tools: it is a namespace of host functions, not plain data. Use Object.keys(tools) for its names."
        )))),
        Value::String(s) => Ok(Obj::array(&rt.builtins.array, iteration::string_code_points(s))),
        Value::Object(o) => Ok(o.clone()),
        _ => Ok(Obj::ordinary(&rt.builtins.object)),
    }
}

pub fn object_assign(rt: &Rt, args: &[Value]) -> JsResult<Value> {
    let target = arg(args, 0);
    let Value::Object(t) = &target else {
        return Err(type_error(format!(
            "Object.assign expects a data object or array target, received {}.",
            describe_value(&target)
        )));
    };
    let mut seen = HashSet::new();
    for source in args.iter().skip(1) {
        if source.is_nullish() {
            continue;
        }
        let from = enumerable_source(rt, "Object.assign(...)", source, None)?;
        // Get from each source, then Set on the target, key by key; enumerability is checked as each key is visited
        // (a getter may delete or hide a later one).
        for key in crate::object::own_keys(&from) {
            if !crate::object::own(&from, &key).map(|s| s.enumerable()).unwrap_or(false) {
                continue;
            }
            let value = get_own(&from, &key)?;
            reject_circular_insertion_seen(t, &value, "Object.assign result", &mut seen)?;
            if !set(t, &key, value)? {
                return Err(crate::object::failed_assignment(&target, &key));
            }
        }
    }
    Ok(target)
}

fn class_tag(value: &Value) -> &'static str {
    match value {
        Value::Null => "Null",
        Value::Undefined => "Undefined",
        Value::Object(o) => o.tag(),
        Value::String(_) => "String",
        Value::Number(_) => "Number",
        Value::Bool(_) => "Boolean",
        _ => "Object",
    }
}

fn property_key(value: &Value) -> PropKey {
    match value {
        Value::Symbol(s) => PropKey::Sym(*s),
        other => PropKey::Str(coerce_to_string(other)),
    }
}

// SetIntegrityLevel: primitives pass through.
fn restrict(level: &str, value: Value) -> JsResult<Value> {
    let Value::Object(o) = &value else { return Ok(value) };
    o.0.extensible.set(false);
    if level == "preventExtensions" {
        return Ok(value);
    }
    if let Some(b) = o.bytes()
        && b.length > 0
    {
        return Err(type_error(format!("Cannot {level} array buffer views with elements.")));
    }
    for slot in o.0.props.borrow_mut().values_mut() {
        match slot {
            Slot::Data { attrs, .. } => {
                attrs.configurable = false;
                if level == "freeze" {
                    attrs.writable = false;
                }
            }
            Slot::Accessor { configurable, .. } => *configurable = false,
        }
    }
    if let Some(a) = o.array_data() {
        a.borrow_mut().elements =
            crate::object::Attributes { writable: level == "seal", enumerable: true, configurable: false };
    }
    Ok(value)
}

// TestIntegrityLevel.
fn integrity(value: &Value, frozen: bool) -> bool {
    let Value::Object(o) = value else { return true };
    if o.extensible() {
        return false;
    }
    own_keys(o).iter().all(|k| match own(o, k) {
        Some(slot) => !slot.configurable() && !(frozen && slot.writable_data() == Some(true)),
        None => false,
    })
}

async fn object_from_entries(rt: Rt, source: Value, site: crate::interp::Site) -> JsResult<Value> {
    let out = Obj::ordinary(&rt.builtins.object);
    let cursor = iteration::iterate(&rt, &source, None, site).await?;
    let Some(cursor) = cursor else {
        return Err(iteration::not_iterable(&source));
    };
    loop {
        let step = cursor.next().await?;
        if step.done {
            return Ok(Value::Object(out));
        }
        let result = (|| -> JsResult<()> {
            let Value::Object(entry) = &step.value else {
                return Err(type_error("Object.fromEntries expects [key, value] entry objects."));
            };
            if contains_opaque_reference(&step.value) {
                return Err(type_error("Object.fromEntries expects [key, value] entry objects."));
            }
            let key = coerce_to_string(&get_own(entry, &PropKey::str("0"))?);
            define_data(&out, key, get_own(entry, &PropKey::str("1"))?);
            Ok(())
        })();
        preserve_consumer_error(&cursor, result).await?;
    }
}

// ---- Property descriptors (2.4.1, #64908) ----

/// ToPropertyKey: a symbol stays itself; anything else is its string, through the program's `toString`.
async fn to_property_key(rt: &Rt, value: &Value, site: crate::interp::Site) -> JsResult<PropKey> {
    if let Value::Symbol(sym) = value {
        return Ok(PropKey::Sym(*sym));
    }
    let primitive = crate::interp::coerce::to_primitive(rt, value, crate::interp::coerce::Hint::String, site).await?;
    Ok(property_key(&primitive))
}

/// ToPropertyDescriptor: the fields the object has (own or inherited), read through getters.
fn to_property_descriptor(value: &Value) -> JsResult<Descriptor> {
    let Value::Object(o) = value else {
        return Err(type_error(format!("Property description must be an object: {}", describe_value(value))));
    };
    let has = |k: &str| crate::object::has(o, &PropKey::str(k));
    let read = |k: &str| crate::object::get(o, &PropKey::str(k));
    let mut desc = Descriptor::default();
    if has("enumerable") {
        desc.enumerable = Some(read("enumerable")?.truthy());
    }
    if has("configurable") {
        desc.configurable = Some(read("configurable")?.truthy());
    }
    if has("value") {
        desc.value = Some(read("value")?);
    }
    if has("writable") {
        desc.writable = Some(read("writable")?.truthy());
    }
    for half in ["get", "set"] {
        if !has(half) {
            continue;
        }
        let f = match read(half)? {
            Value::Undefined => None,
            Value::Object(f) if f.is_callable() => Some(f),
            other => {
                let which = if half == "get" { "Getter" } else { "Setter" };
                return Err(type_error(format!("{which} must be a function: {}", describe_value(&other))));
            }
        };
        if half == "get" {
            desc.get = Some(f);
        } else {
            desc.set = Some(f);
        }
    }
    if desc.is_accessor() && desc.is_data() {
        return Err(type_error(
            "Invalid property descriptor. Cannot both specify accessors and a value or writable attribute.",
        ));
    }
    Ok(desc)
}

fn define_or_throw(target: &Obj, key: &PropKey, desc: &Descriptor) -> JsResult<()> {
    if let Some(value @ Value::Object(_)) = &desc.value {
        let label = if target.is_array() { "Array assignment result" } else { "Object assignment result" };
        crate::interp::references::reject_circular_insertion(target, value, label)?;
    }
    if define_own_property(target, key, desc)? {
        return Ok(());
    }
    if !target.extensible() && crate::object::own(target, key).is_none() {
        return Err(type_error(format!("Cannot define property {}, object is not extensible", key.display())));
    }
    Err(type_error(format!("Cannot redefine property: {}", key.display())))
}

fn require_target(value: &Value, name: &str) -> JsResult<Obj> {
    match value {
        Value::Object(o) => Ok(o.clone()),
        _ => Err(type_error(format!("Object.{name} called on non-object"))),
    }
}

/// ObjectDefineProperties: every descriptor is read (own enumerable keys, in order) before any is defined.
fn define_properties(target: &Obj, props: &Value) -> JsResult<()> {
    let descriptors: Vec<(PropKey, Descriptor)> = match props {
        Value::Undefined | Value::Null => {
            return Err(type_error(format!("Cannot convert {} to object", coerce_to_string(props))));
        }
        Value::Object(p) => {
            let mut out = Vec::new();
            for key in own_keys(p) {
                if crate::object::own(p, &key).is_some_and(|s| s.enumerable()) {
                    out.push((key.clone(), to_property_descriptor(&crate::object::get(p, &key)?)?));
                }
            }
            out
        }
        Value::String(text) => {
            let mut out = Vec::new();
            for (index, unit) in crate::interp::iteration::string_code_points(text).into_iter().enumerate() {
                out.push((PropKey::Str(JsStr::from(index.to_string())), to_property_descriptor(&unit)?));
            }
            out
        }
        _ => Vec::new(),
    };
    for (key, desc) in descriptors {
        define_or_throw(target, &key, &desc)?;
    }
    Ok(())
}

/// FromPropertyDescriptor: `{ value, writable, enumerable, configurable }` or `{ get, set, enumerable,
/// configurable }`; a built-in accessor's halves are functions too (`get size`).
fn from_slot(rt: &Rt, slot: Option<Slot>, key: &PropKey) -> Value {
    let Some(slot) = slot else { return Value::Undefined };
    let object = Obj::ordinary(&rt.builtins.object);
    match &slot {
        Slot::Data { value, attrs } => {
            define_data(&object, "value", value.clone());
            define_data(&object, "writable", Value::Bool(attrs.writable));
        }
        Slot::Accessor { get, set, .. } => {
            let f = |o: Option<Obj>| o.map(Value::Object).unwrap_or_default();
            define_data(&object, "get", f(get.as_ref().and_then(|h| crate::object::get_half_function(h, key))));
            define_data(&object, "set", f(set.as_ref().and_then(|h| crate::object::set_half_function(h, key))));
        }
    }
    define_data(&object, "enumerable", Value::Bool(slot.enumerable()));
    define_data(&object, "configurable", Value::Bool(slot.configurable()));
    Value::Object(object)
}

/// A string's own `length` and indexes, as `getOwnPropertyDescriptor("abc", 0)` sees them.
fn primitive_slot(value: &Value, key: &PropKey) -> Option<Slot> {
    let Value::String(text) = value else { return None };
    let PropKey::Str(name) = key else { return None };
    let attrs = |enumerable| crate::object::Attributes { writable: false, enumerable, configurable: false };
    if name.eq_str("length") {
        return Some(Slot::Data { value: Value::Number(text.len() as f64), attrs: attrs(false) });
    }
    let at = crate::object::parse_array_index(name)?;
    (at < text.len()).then(|| Slot::Data { value: Value::String(text.slice(at, at + 1)), attrs: attrs(true) })
}

fn primitive_keys(value: &Value) -> Vec<PropKey> {
    let Value::String(text) = value else { return Vec::new() };
    (0..text.len()).map(|i| PropKey::Str(JsStr::from(i.to_string()))).chain([PropKey::str("length")]).collect()
}

fn require_object_coercible(value: &Value, name: &str) -> JsResult<()> {
    match value {
        Value::Undefined | Value::Null => {
            Err(type_error(format!("Object.{name} cannot convert {} to an object.", coerce_to_string(value))))
        }
        Value::Tool(_) => Err(type_error(format!(
            "Object.{name} cannot read tools: it is a namespace of host functions, not an object."
        ))),
        _ => Ok(()),
    }
}

// Object constructs identically with or without new, like JS.
pub fn object_global(rt: &Rt) -> Value {
    let b = &rt.builtins;
    let construct = |rt: &Rt, args: &[Value]| -> JsResult<Value> {
        match arg(args, 0) {
            Value::Undefined | Value::Null => Ok(Value::Object(Obj::ordinary(&rt.builtins.object))),
            v @ Value::Object(_) => Ok(v),
            other => Err(type_error(format!(
                "Object({}) wrapper objects are not supported; use the primitive value directly.",
                other.js_typeof()
            ))),
        }
    };
    let object = constructor(
        &b.function,
        &b.object,
        "Object",
        1,
        NativeFn {
            call: Rc::new(move |rt, _, _, args| NResult::Ready(construct(rt, &args))),
            construct: Some(Rc::new(move |rt, _, args, _| NResult::Ready(construct(rt, &args)))),
            callback: true,
        },
    );
    methods(
        &b.function,
        &object,
        vec![
            method("keys", 1, |rt, _, _, args| {
                NResult::Ready((|| {
                    let list: Vec<Value> = match &arg(&args, 0) {
                        Value::Tool(t) => rt.tools.keys(t.path())?.into_iter().map(Value::from).collect(),
                        other => keys(&enumerable_source(rt, "Object.keys(...)", other, None)?)
                            .into_iter()
                            .map(Value::String)
                            .collect(),
                    };
                    Ok(Value::Object(Obj::array(&rt.builtins.array, list)))
                })())
            }),
            method("values", 1, |rt, _, _, args| {
                NResult::Ready((|| {
                    let source = enumerable_source(rt, "Object.values(...)", &arg(&args, 0), None)?;
                    let list = entries(&source)?.into_iter().map(|(_, v)| v).collect();
                    Ok(Value::Object(Obj::array(&rt.builtins.array, list)))
                })())
            }),
            method("entries", 1, |rt, _, _, args| {
                NResult::Ready((|| {
                    let source = enumerable_source(rt, "Object.entries(...)", &arg(&args, 0), None)?;
                    let list = entries(&source)?
                        .into_iter()
                        .map(|(k, v)| Value::Object(Obj::array(&rt.builtins.array, vec![Value::String(k), v])))
                        .collect();
                    Ok(Value::Object(Obj::array(&rt.builtins.array, list)))
                })())
            }),
            method("hasOwn", 2, |rt, _, _, args| {
                NResult::Ready(
                    enumerable_source(rt, "Object.hasOwn(...)", &arg(&args, 0), None)
                        .map(|o| Value::Bool(has_own(&o, &property_key(&arg(&args, 1))))),
                )
            }),
            method("is", 2, |_, _, _, args| NResult::ok(arg(&args, 0).same_value(&arg(&args, 1)))),
            method("assign", 2, |rt, _, _, args| NResult::Ready(object_assign(rt, &args))),
            method("fromEntries", 1, |rt, site, _, args| {
                NResult::future(object_from_entries(rt.clone(), arg(&args, 0), site))
            }),
            method("freeze", 1, |_, _, _, args| NResult::Ready(restrict("freeze", arg(&args, 0)))),
            method("seal", 1, |_, _, _, args| NResult::Ready(restrict("seal", arg(&args, 0)))),
            method("preventExtensions", 1, |_, _, _, args| NResult::Ready(restrict("preventExtensions", arg(&args, 0)))),
            method("isFrozen", 1, |_, _, _, args| NResult::ok(integrity(&arg(&args, 0), true))),
            method("isSealed", 1, |_, _, _, args| NResult::ok(integrity(&arg(&args, 0), false))),
            method("isExtensible", 1, |_, _, _, args| {
                NResult::ok(matches!(arg(&args, 0), Value::Object(o) if o.extensible()))
            }),
            method("getPrototypeOf", 1, |rt, _, _, args| {
                let v = arg(&args, 0);
                if let Value::Object(o) = &v {
                    return NResult::ok(o.proto().map(Value::Object).unwrap_or(Value::Null));
                }
                match rt.builtins.primitive_prototype(&v) {
                    Some(p) => NResult::ok(Value::Object(p.clone())),
                    None => NResult::err(type_error(format!(
                        "Object.getPrototypeOf cannot convert {} to an object.",
                        describe_value(&v)
                    ))),
                }
            }),
            method("create", 2, |_, _, _, args| {
                let proto = match arg(&args, 0) {
                    Value::Null => None,
                    Value::Object(o) => Some(o),
                    _ => return NResult::err(type_error("Object prototype may only be an Object or null.")),
                };
                let created = Obj::new(proto, ObjKind::Ordinary);
                if arg(&args, 1).is_undefined() {
                    return NResult::ok(Value::Object(created));
                }
                // 2.4.1: properties from descriptors, as `Object.defineProperties` defines them.
                NResult::Ready(define_properties(&created, &arg(&args, 1)).map(|_| Value::Object(created)))
            }),
            method("defineProperty", 3, |rt, site, _, args| {
                let target = match require_target(&arg(&args, 0), "defineProperty") {
                    Ok(t) => t,
                    Err(e) => return NResult::err(e),
                };
                let rt = rt.clone();
                NResult::future(async move {
                    let key = to_property_key(&rt, &arg(&args, 1), site).await?;
                    let desc = to_property_descriptor(&arg(&args, 2))?;
                    define_or_throw(&target, &key, &desc)?;
                    Ok(Value::Object(target))
                })
            }),
            method("defineProperties", 2, |_, _, _, args| {
                NResult::Ready(
                    require_target(&arg(&args, 0), "defineProperties")
                        .and_then(|t| define_properties(&t, &arg(&args, 1)).map(|_| Value::Object(t))),
                )
            }),
            method("getOwnPropertyDescriptor", 2, |rt, site, _, args| {
                let value = arg(&args, 0);
                if let Err(e) = require_object_coercible(&value, "getOwnPropertyDescriptor") {
                    return NResult::err(e);
                }
                let rt = rt.clone();
                NResult::future(async move {
                    let key = to_property_key(&rt, &arg(&args, 1), site).await?;
                    let slot = match &value {
                        Value::Object(o) => crate::object::own(o, &key),
                        other => primitive_slot(other, &key),
                    };
                    Ok(from_slot(&rt, slot, &key))
                })
            }),
            method("getOwnPropertyDescriptors", 1, |rt, _, _, args| {
                let value = arg(&args, 0);
                if let Err(e) = require_object_coercible(&value, "getOwnPropertyDescriptors") {
                    return NResult::err(e);
                }
                let result = Obj::ordinary(&rt.builtins.object);
                let keys: Vec<PropKey> = match &value {
                    Value::Object(o) => own_keys(o),
                    other => primitive_keys(other),
                };
                for key in keys {
                    let slot = match &value {
                        Value::Object(o) => crate::object::own(o, &key),
                        other => primitive_slot(other, &key),
                    };
                    define_data(&result, key.clone(), from_slot(rt, slot, &key));
                }
                NResult::ok(Value::Object(result))
            }),
            method("getOwnPropertyNames", 1, |rt, _, _, args| {
                NResult::Ready((|| {
                    let value = arg(&args, 0);
                    let names: Vec<Value> = match &value {
                        Value::Tool(t) => rt.tools.keys(t.path())?.into_iter().map(Value::from).collect(),
                        Value::Object(o) => own_keys(o)
                            .into_iter()
                            .filter_map(|k| match k {
                                PropKey::Str(s) => Some(Value::String(s)),
                                PropKey::Sym(_) => None,
                            })
                            .collect(),
                        other => {
                            require_object_coercible(other, "getOwnPropertyNames")?;
                            primitive_keys(other).into_iter().map(|k| k.to_value()).collect()
                        }
                    };
                    Ok(Value::Object(Obj::array(&rt.builtins.array, names)))
                })())
            }),
        ],
    );
    define(&object, "groupBy", super::collections::group_by(rt, "Object"), HIDDEN);
    methods(
        &b.function,
        &b.object,
        vec![
            method("hasOwnProperty", 1, |_, _, this, args| {
                NResult::Ready(
                    receiver(&this, "Object.prototype.hasOwnProperty", |_| true)
                        .map(|o| Value::Bool(has_own(&o, &property_key(&arg(&args, 0))))),
                )
            }),
            method("isPrototypeOf", 1, |_, _, this, args| {
                NResult::Ready(
                    receiver(&this, "Object.prototype.isPrototypeOf", |_| true)
                        .map(|o| Value::Bool(has_prototype(&arg(&args, 0), &o))),
                )
            }),
            method("propertyIsEnumerable", 1, |_, _, this, args| {
                NResult::Ready(receiver(&this, "Object.prototype.propertyIsEnumerable", |_| true).map(|o| {
                    Value::Bool(own(&o, &property_key(&arg(&args, 0))).map(|s| s.enumerable()).unwrap_or(false))
                }))
            }),
            method("toString", 0, |_, _, this, _| NResult::ok(format!("[object {}]", class_tag(&this)))),
            method("toLocaleString", 0, |rt, site, this, _| {
                let rt = rt.clone();
                NResult::future(async move {
                    invoke(&rt, &this, "toString", "Object.prototype.toLocaleString", site).await
                })
            }),
            method("valueOf", 0, |_, _, this, _| {
                if this.is_nullish() {
                    return NResult::err(type_error("Object.prototype.valueOf called on null or undefined."));
                }
                NResult::ok(this)
            }),
        ],
    );
    Value::Object(object)
}

#[allow(dead_code)]
fn _sym(_: Sym, _: JsStr) {}
