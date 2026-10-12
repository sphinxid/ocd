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
    define, define_data, entries, enumerable_keys, get_own, has_own, has_prototype, keys, own, own_keys, set, Obj,
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
            "{label} cannot read tool references: they are not plain data. Use Object.keys(tools) for names, or search({{ query }}) for signatures."
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
        for key in enumerable_keys(&from) {
            let value = get_own(&from, &key)?;
            reject_circular_insertion_seen(t, &value, "Object.assign result", &mut seen)?;
            if !set(t, &key, value)? {
                return Err(type_error(format!("Cannot assign to read only property '{}'.", key.display())));
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
        return Err(type_error("Object.fromEntries expects a synchronous iterable of entries."));
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
                if !arg(&args, 1).is_undefined() {
                    return NResult::err(type_error(
                        "Object.create property descriptors are not supported; assign the fields after creating the object.",
                    ));
                }
                NResult::ok(Value::Object(Obj::new(proto, ObjKind::Ordinary)))
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
