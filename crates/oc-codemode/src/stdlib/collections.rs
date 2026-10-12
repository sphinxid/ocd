//! `Map`, `Set`, `WeakMap`, `WeakSet`, and `Map.groupBy`/`Object.groupBy`. Port of `stdlib/collections.ts`.

use std::cell::RefCell;
use std::rc::Rc;

use crate::interp::callback::{apply_collection_callback, is_supported_callback, Callback};
use crate::interp::coerce::{coerce_to_string, to_primitive_number, to_primitive_string};
use crate::interp::errors::{invalid_data, type_error, JsResult};
use crate::interp::eval::call_value;
use crate::interp::function::{
    arg, constructor, fn_, method, methods, prototype_from, receiver, requires_new, Method, NResult, NativeFn,
};
use crate::interp::iteration::{self, iterate_direct, iterator_object, preserve_consumer_error, MapCursor, Mode};
use crate::interp::references::is_opaque;
use crate::interp::{describe_value, Rt, Site};
use crate::object::{
    define, define_accessor, define_data, get, get_own, get_str, MapData, Obj, ObjKind, WeakData, HIDDEN,
};
use crate::value::{JsStr, PropKey, Sym, Value};

macro_rules! tri {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(e) => return NResult::err(e),
        }
    };
}

fn map_data(o: &Obj) -> &RefCell<MapData> {
    match o.kind() {
        ObjKind::Map(m) | ObjKind::Set(m) => m,
        _ => unreachable!("map or set receiver"),
    }
}

fn weak_data(o: &Obj) -> &RefCell<WeakData> {
    match o.kind() {
        ObjKind::WeakMap(m) | ObjKind::WeakSet(m) => m,
        _ => unreachable!("weak collection receiver"),
    }
}

/// The first live entry at or after `from`, with its position (live iteration over tombstones).
fn entry_from(o: &Obj, from: usize) -> Option<(usize, Value, Value)> {
    let data = map_data(o).borrow();
    let mut i = from;
    while i < data.entries.len() {
        if let Some((k, v)) = &data.entries[i] {
            return Some((i, k.clone(), v.clone()));
        }
        i += 1;
    }
    None
}

/// Map/Set keys normalize -0 to +0.
fn normalize(value: Value) -> Value {
    match value {
        Value::Number(n) if n == 0.0 => Value::Number(0.0),
        other => other,
    }
}

fn set_add(o: &Obj, item: Value) {
    let item = normalize(item);
    let mut data = map_data(o).borrow_mut();
    if !data.has(&item) {
        data.set(item.clone(), item);
    }
}

fn new_set(rt: &Rt) -> Obj {
    Obj::new(Some(rt.builtins.set.clone()), ObjKind::Set(RefCell::new(MapData::default())))
}

fn is_map(o: &Obj) -> bool {
    matches!(o.kind(), ObjKind::Map(_))
}

fn is_set(o: &Obj) -> bool {
    matches!(o.kind(), ObjKind::Set(_))
}

async fn coerce_group_by_property_key(rt: &Rt, value: Value, site: Site) -> JsResult<JsStr> {
    if let Value::Object(o) = &value
        && o.is_promise()
    {
        return Ok(coerce_to_string(&value));
    }
    if is_opaque(&value) {
        return Err(invalid_data(format!(
            "Object.groupBy callback must return a data value, received {}.",
            describe_value(&value)
        )));
    }
    to_primitive_string(rt, &value, site).await
}

async fn group_by_run(rt: Rt, namespace: &'static str, source: Value, apply: Callback, site: Site) -> JsResult<Value> {
    let cursor = iteration::iterate(&rt, &source, None, site).await?;
    let Some(cursor) = cursor else {
        return Err(type_error(format!("{namespace}.groupBy expects an iterable collection.")));
    };
    let mut index = 0usize;
    if namespace == "Map" {
        let result = Obj::new(Some(rt.builtins.map.clone()), ObjKind::Map(RefCell::new(MapData::default())));
        loop {
            let step = cursor.next().await?;
            if step.done {
                return Ok(Value::Object(result));
            }
            let item = step.value;
            let called = apply(&rt, vec![item.clone(), Value::Number(index as f64)], Value::Undefined, site).await;
            let key = preserve_consumer_error(&cursor, called).await?;
            let group = map_data(&result).borrow().get(&key).cloned();
            match group {
                Some(Value::Object(g)) => {
                    if let Some(a) = g.array_data() {
                        a.borrow_mut().items.push(Some(item));
                    }
                }
                _ => map_data(&result)
                    .borrow_mut()
                    .set(key, Value::Object(Obj::array(&rt.builtins.array, vec![item]))),
            }
            index += 1;
        }
    }
    // Object.groupBy returns a null-prototype object, so group names never collide with inherited methods.
    let result = Obj::new(None, ObjKind::Ordinary);
    loop {
        let step = cursor.next().await?;
        if step.done {
            return Ok(Value::Object(result));
        }
        let item = step.value;
        let keyed = async {
            let value = apply(&rt, vec![item.clone(), Value::Number(index as f64)], Value::Undefined, site).await?;
            coerce_group_by_property_key(&rt, value, site).await
        }
        .await;
        let key = preserve_consumer_error(&cursor, keyed).await?;
        let key = PropKey::Str(key);
        match get_own(&result, &key)? {
            Value::Object(g) if g.is_array() => {
                if let Some(a) = g.array_data() {
                    a.borrow_mut().items.push(Some(item));
                }
            }
            _ => define_data(&result, key, Value::Object(Obj::array(&rt.builtins.array, vec![item]))),
        }
        index += 1;
    }
}

/// `Map.groupBy` and `Object.groupBy`: the same iteration, keyed into a Map or a data object.
pub fn group_by(rt: &Rt, namespace: &str) -> Value {
    let namespace: &'static str = if namespace == "Map" { "Map" } else { "Object" };
    Value::Object(fn_(&rt.builtins.function, "groupBy", 2, move |rt, site, _, args| {
        let source = arg(&args, 0);
        if source.is_nullish() {
            return NResult::err(type_error(format!("{namespace}.groupBy expects an iterable collection.")));
        }
        let apply = tri!(apply_collection_callback(&arg(&args, 1), &format!("{namespace}.groupBy")));
        NResult::future(group_by_run(rt.clone(), namespace, source, apply, site))
    }))
}

/// Shared constructor loop for Map and WeakMap: `[key, value]` entry objects from a synchronous iterable.
async fn fill_from_entries(
    rt: &Rt,
    init: &Value,
    label: &str,
    site: Site,
    mut insert: impl FnMut(Value, Value) -> JsResult<()>,
) -> JsResult<()> {
    let cursor = iteration::iterate(rt, init, None, site).await?;
    let Some(cursor) = cursor else {
        return Err(type_error(format!(
            "new {label}(...) expects an iterable of [key, value] pairs, received {}.",
            describe_value(init)
        )));
    };
    loop {
        let step = cursor.next().await?;
        if step.done {
            return Ok(());
        }
        let result = (|| {
            let Value::Object(entry) = &step.value else {
                return Err(type_error(format!("new {label}(...) expects [key, value] pairs as entry objects.")));
            };
            let key = get_own(entry, &PropKey::str("0"))?;
            let value = get_own(entry, &PropKey::str("1"))?;
            insert(key, value)
        })();
        preserve_consumer_error(&cursor, result).await?;
    }
}

fn construct_map(rt: &Rt, site: Site, init: Value, proto: Obj) -> NResult {
    let target = Obj::new(Some(proto), ObjKind::Map(RefCell::new(MapData::default())));
    if init.is_nullish() {
        return NResult::ok(Value::Object(target));
    }
    let rt = rt.clone();
    NResult::future(async move {
        let t = target.clone();
        fill_from_entries(&rt, &init, "Map", site, move |k, v| {
            map_data(&t).borrow_mut().set(k, v);
            Ok(())
        })
        .await?;
        Ok(Value::Object(target))
    })
}

fn construct_set(rt: &Rt, site: Site, init: Value, proto: Obj) -> NResult {
    let target = Obj::new(Some(proto), ObjKind::Set(RefCell::new(MapData::default())));
    if init.is_nullish() {
        return NResult::ok(Value::Object(target));
    }
    let rt = rt.clone();
    NResult::future(async move {
        let cursor = iteration::iterate(&rt, &init, None, site).await?;
        let Some(cursor) = cursor else {
            return Err(type_error(format!(
                "new Set(...) expects a synchronous iterable, received {}.",
                describe_value(&init)
            )));
        };
        loop {
            let step = cursor.next().await?;
            if step.done {
                return Ok(Value::Object(target));
            }
            set_add(&target, step.value);
        }
    })
}

/// A live Map/Set walk calling `visit(key, value)` for each entry, as `forEach` does.
async fn for_each_entry(
    rt: &Rt,
    target: &Obj,
    apply: &Callback,
    this_arg: &Value,
    site: Site,
    set: bool,
) -> JsResult<Value> {
    let mut position = 0;
    while let Some((i, key, value)) = entry_from(target, position) {
        position = i + 1;
        let value = if set { key.clone() } else { value };
        apply(rt, vec![value, key, Value::Object(target.clone())], this_arg.clone(), site).await?;
    }
    Ok(Value::Undefined)
}

pub fn map_global(rt: &Rt) -> Value {
    let b = &rt.builtins;
    let proto = b.map.clone();
    let map = constructor(
        &b.function,
        &proto,
        "Map",
        0,
        NativeFn {
            call: requires_new("Map"),
            construct: Some(Rc::new(|rt, site, args, new_target| {
                construct_map(rt, site, arg(&args, 0), prototype_from(&new_target, &rt.builtins.map))
            })),
            callback: true,
        },
    );
    define(&map, "groupBy", group_by(rt, "Map"), HIDDEN);
    let this_map = |this: &Value, name: &str| receiver(this, &format!("Map.prototype.{name}"), is_map);
    define_accessor(
        &proto,
        "size",
        Some(Rc::new(|this: &Value| {
            let target = receiver(this, "Map.prototype.size", is_map)?;
            Ok(Value::Number(map_data(&target).borrow().size() as f64))
        })),
        None,
    );
    methods(
        &b.function,
        &proto,
        vec![
            method("get", 1, move |_, _, this, args| {
                let target = tri!(this_map(&this, "get"));
                NResult::ok(map_data(&target).borrow().get(&arg(&args, 0)).cloned().unwrap_or_default())
            }),
            method("has", 1, move |_, _, this, args| {
                let target = tri!(this_map(&this, "has"));
                NResult::ok(map_data(&target).borrow().has(&arg(&args, 0)))
            }),
            method("set", 2, move |_, _, this, args| {
                let target = tri!(this_map(&this, "set"));
                map_data(&target).borrow_mut().set(arg(&args, 0), arg(&args, 1));
                NResult::ok(Value::Object(target))
            }),
            method("getOrInsert", 2, move |_, _, this, args| {
                let target = tri!(this_map(&this, "getOrInsert"));
                let key = arg(&args, 0);
                let mut data = map_data(&target).borrow_mut();
                if !data.has(&key) {
                    data.set(key.clone(), arg(&args, 1));
                }
                NResult::ok(data.get(&key).cloned().unwrap_or_default())
            }),
            method("getOrInsertComputed", 2, move |rt, site, this, args| {
                let target = tri!(this_map(&this, "getOrInsertComputed"));
                let apply = tri!(apply_collection_callback(&arg(&args, 1), "Map.getOrInsertComputed"));
                let key = arg(&args, 0);
                if let Some(existing) = map_data(&target).borrow().get(&key) {
                    return NResult::ok(existing.clone());
                }
                let rt = rt.clone();
                NResult::future(async move {
                    // The callback sees the stored key (-0 is +0) and its result wins over anything it inserted itself.
                    let value = apply(&rt, vec![normalize(key.clone())], Value::Undefined, site).await?;
                    map_data(&target).borrow_mut().set(key, value.clone());
                    Ok(value)
                })
            }),
            method("delete", 1, move |_, _, this, args| {
                let target = tri!(this_map(&this, "delete"));
                NResult::ok(map_data(&target).borrow_mut().delete(&arg(&args, 0)))
            }),
            method("clear", 0, move |_, _, this, _| {
                let target = tri!(this_map(&this, "clear"));
                map_data(&target).borrow_mut().clear();
                NResult::ok(Value::Undefined)
            }),
            method("keys", 0, move |rt, _, this, _| {
                let target = tri!(this_map(&this, "keys"));
                NResult::ok(iterator_object(rt, MapCursor::new(rt, target, Mode::Keys)))
            }),
            method("values", 0, move |rt, _, this, _| {
                let target = tri!(this_map(&this, "values"));
                NResult::ok(iterator_object(rt, MapCursor::new(rt, target, Mode::Values)))
            }),
            method("entries", 0, move |rt, _, this, _| {
                let target = tri!(this_map(&this, "entries"));
                NResult::ok(iterator_object(rt, MapCursor::new(rt, target, Mode::Entries)))
            }),
            method("forEach", 1, move |rt, site, this, args| {
                let target = tri!(this_map(&this, "forEach"));
                let apply = tri!(apply_collection_callback(&arg(&args, 0), "Map.forEach"));
                let this_arg = arg(&args, 1);
                let rt = rt.clone();
                NResult::future(async move { for_each_entry(&rt, &target, &apply, &this_arg, site, false).await })
            }),
        ],
    );
    let entries = get(&proto, &PropKey::str("entries")).unwrap_or_default();
    define(&proto, PropKey::Sym(Sym::Iterator), entries, HIDDEN);
    Value::Object(map)
}

/// GetSetRecord: the operand's size, `has`, and `keys`.
enum SetRecord {
    /// A built-in Set or Map, answered without program code.
    Builtin(Obj),
    Custom { source: Obj, size: f64, has: Value, keys: Value },
}

impl SetRecord {
    fn size(&self) -> f64 {
        match self {
            SetRecord::Builtin(o) => map_data(o).borrow().size() as f64,
            SetRecord::Custom { size, .. } => *size,
        }
    }
    async fn has(&self, rt: &Rt, item: &Value, site: Site) -> JsResult<bool> {
        match self {
            SetRecord::Builtin(o) => Ok(map_data(o).borrow().has(item)),
            SetRecord::Custom { source, has, .. } => {
                Ok(call_value(rt, has, Value::Object(source.clone()), vec![item.clone()], site).await?.truthy())
            }
        }
    }
    async fn keys(&self, rt: &Rt, site: Site) -> JsResult<Vec<Value>> {
        match self {
            SetRecord::Builtin(o) => Ok(map_data(o).borrow().iter().map(|(k, _)| k.clone()).collect()),
            SetRecord::Custom { source, keys, .. } => {
                let result = call_value(rt, keys, Value::Object(source.clone()), Vec::new(), site).await?;
                let cursor = match &result {
                    Value::Object(o) if o.is_array() => {
                        iteration::ArrayCursor::new(rt, o.clone(), Mode::Values)
                    }
                    _ => iterate_direct(rt, &result, None, site)?,
                };
                let mut items = Vec::new();
                loop {
                    let step = cursor.next().await?;
                    if step.done {
                        return Ok(items);
                    }
                    items.push(step.value);
                }
            }
        }
    }
}

async fn load_set_record(rt: &Rt, source: &Value, name: &str, site: Site) -> JsResult<SetRecord> {
    let Value::Object(o) = source else {
        return Err(type_error(format!("Set.{name} expects a Set-like object.")));
    };
    if is_set(o) || is_map(o) {
        return Ok(SetRecord::Builtin(o.clone()));
    }
    let size = to_primitive_number(rt, &get_str(o, "size")?, site).await?;
    if size.is_nan() {
        return Err(type_error(format!("Set.{name} received a Set-like object with an invalid size.")));
    }
    let has = get_str(o, "has")?;
    let keys = get_str(o, "keys")?;
    if !is_supported_callback(&has) || !is_supported_callback(&keys) {
        return Err(type_error(format!("Set.{name} expects callable 'has' and 'keys' methods.")));
    }
    Ok(SetRecord::Custom { source: o.clone(), size: size.trunc().max(0.0), has, keys })
}

fn copy_set(rt: &Rt, target: &Obj) -> Obj {
    let result = new_set(rt);
    for (k, _) in map_data(target).borrow().iter() {
        set_add(&result, k.clone());
    }
    result
}

async fn set_operation(rt: Rt, target: Obj, name: &'static str, source: Value, site: Site) -> JsResult<Value> {
    let other = load_set_record(&rt, &source, name, site).await?;
    // The operand's size is read once, when its record is loaded. Its keys are read in one go: a built-in operand is
    // never changed while they are visited, and a Set-like operand's `keys()` is drained before any is visited.
    let other_size = other.size();
    let target_size = || map_data(&target).borrow().size() as f64;
    match name {
        "union" => {
            let result = copy_set(&rt, &target);
            for item in other.keys(&rt, site).await? {
                set_add(&result, item);
            }
            Ok(Value::Object(result))
        }
        "intersection" => {
            let result = new_set(&rt);
            if target_size() <= other_size {
                let mut position = 0;
                while let Some((i, item, _)) = entry_from(&target, position) {
                    position = i + 1;
                    if other.has(&rt, &item, site).await? {
                        set_add(&result, item);
                    }
                }
                return Ok(Value::Object(result));
            }
            for item in other.keys(&rt, site).await? {
                if map_data(&target).borrow().has(&item) {
                    set_add(&result, item);
                }
            }
            Ok(Value::Object(result))
        }
        "difference" => {
            let result = copy_set(&rt, &target);
            if target_size() <= other_size {
                let mut position = 0;
                while let Some((i, item, _)) = entry_from(&result, position) {
                    position = i + 1;
                    if other.has(&rt, &item, site).await? {
                        map_data(&result).borrow_mut().delete(&item);
                    }
                }
                return Ok(Value::Object(result));
            }
            for item in other.keys(&rt, site).await? {
                map_data(&result).borrow_mut().delete(&item);
            }
            Ok(Value::Object(result))
        }
        "symmetricDifference" => {
            let result = copy_set(&rt, &target);
            for item in other.keys(&rt, site).await? {
                if map_data(&target).borrow().has(&item) {
                    map_data(&result).borrow_mut().delete(&item);
                } else {
                    set_add(&result, item);
                }
            }
            Ok(Value::Object(result))
        }
        "isSubsetOf" => {
            if target_size() > other_size {
                return Ok(Value::Bool(false));
            }
            let mut position = 0;
            while let Some((i, item, _)) = entry_from(&target, position) {
                position = i + 1;
                if !other.has(&rt, &item, site).await? {
                    return Ok(Value::Bool(false));
                }
            }
            Ok(Value::Bool(true))
        }
        "isSupersetOf" => {
            if target_size() < other_size {
                return Ok(Value::Bool(false));
            }
            for item in other.keys(&rt, site).await? {
                if !map_data(&target).borrow().has(&item) {
                    return Ok(Value::Bool(false));
                }
            }
            Ok(Value::Bool(true))
        }
        _ => {
            if target_size() <= other_size {
                let mut position = 0;
                while let Some((i, item, _)) = entry_from(&target, position) {
                    position = i + 1;
                    if other.has(&rt, &item, site).await? {
                        return Ok(Value::Bool(false));
                    }
                }
                return Ok(Value::Bool(true));
            }
            for item in other.keys(&rt, site).await? {
                if map_data(&target).borrow().has(&item) {
                    return Ok(Value::Bool(false));
                }
            }
            Ok(Value::Bool(true))
        }
    }
}

fn this_set(this: &Value, name: &str) -> JsResult<Obj> {
    receiver(this, &format!("Set.prototype.{name}"), is_set)
}

fn operation(name: &'static str) -> Method {
    method(name, 1, move |rt, site, this, args| {
        let target = tri!(this_set(&this, name));
        NResult::future(set_operation(rt.clone(), target, name, arg(&args, 0), site))
    })
}

pub fn set_global(rt: &Rt) -> Value {
    let b = &rt.builtins;
    let proto = b.set.clone();
    let set = constructor(
        &b.function,
        &proto,
        "Set",
        0,
        NativeFn {
            call: requires_new("Set"),
            construct: Some(Rc::new(|rt, site, args, new_target| {
                construct_set(rt, site, arg(&args, 0), prototype_from(&new_target, &rt.builtins.set))
            })),
            callback: true,
        },
    );
    define_accessor(
        &proto,
        "size",
        Some(Rc::new(|this: &Value| {
            let target = receiver(this, "Set.prototype.size", is_set)?;
            Ok(Value::Number(map_data(&target).borrow().size() as f64))
        })),
        None,
    );
    methods(
        &b.function,
        &proto,
        vec![
            method("has", 1, |_, _, this, args| {
                let target = tri!(this_set(&this, "has"));
                NResult::ok(map_data(&target).borrow().has(&arg(&args, 0)))
            }),
            method("add", 1, |_, _, this, args| {
                let target = tri!(this_set(&this, "add"));
                set_add(&target, arg(&args, 0));
                NResult::ok(Value::Object(target))
            }),
            method("delete", 1, |_, _, this, args| {
                let target = tri!(this_set(&this, "delete"));
                NResult::ok(map_data(&target).borrow_mut().delete(&arg(&args, 0)))
            }),
            method("clear", 0, |_, _, this, _| {
                let target = tri!(this_set(&this, "clear"));
                map_data(&target).borrow_mut().clear();
                NResult::ok(Value::Undefined)
            }),
            method("keys", 0, |rt, _, this, _| {
                let target = tri!(this_set(&this, "keys"));
                NResult::ok(iterator_object(rt, MapCursor::new(rt, target, Mode::Values)))
            }),
            method("values", 0, |rt, _, this, _| {
                let target = tri!(this_set(&this, "values"));
                NResult::ok(iterator_object(rt, MapCursor::new(rt, target, Mode::Values)))
            }),
            method("entries", 0, |rt, _, this, _| {
                let target = tri!(this_set(&this, "entries"));
                NResult::ok(iterator_object(rt, MapCursor::new(rt, target, Mode::Entries)))
            }),
            method("forEach", 1, |rt, site, this, args| {
                let target = tri!(this_set(&this, "forEach"));
                let apply = tri!(apply_collection_callback(&arg(&args, 0), "Set.forEach"));
                let this_arg = arg(&args, 1);
                let rt = rt.clone();
                NResult::future(async move { for_each_entry(&rt, &target, &apply, &this_arg, site, true).await })
            }),
            operation("union"),
            operation("intersection"),
            operation("difference"),
            operation("symmetricDifference"),
            operation("isSubsetOf"),
            operation("isSupersetOf"),
            operation("isDisjointFrom"),
        ],
    );
    let values = get(&proto, &PropKey::str("values")).unwrap_or_default();
    define(&proto, PropKey::Sym(Sym::Iterator), values, HIDDEN);
    Value::Object(set)
}

// CanBeHeldWeakly: only program objects; tool references are rebuilt on every access, so they could never be found
// again.
fn weak_key(value: &Value, label: &str) -> JsResult<Obj> {
    match value {
        Value::Object(o) => Ok(o.clone()),
        _ => Err(type_error(format!("Invalid value used {label}: {} cannot be held weakly.", describe_value(value)))),
    }
}

fn is_weak_map(o: &Obj) -> bool {
    matches!(o.kind(), ObjKind::WeakMap(_))
}

fn is_weak_set(o: &Obj) -> bool {
    matches!(o.kind(), ObjKind::WeakSet(_))
}

pub fn weak_map_global(rt: &Rt) -> Value {
    let b = &rt.builtins;
    let proto = b.weak_map.clone();
    let weak_map = constructor(
        &b.function,
        &proto,
        "WeakMap",
        0,
        NativeFn {
            call: requires_new("WeakMap"),
            construct: Some(Rc::new(|rt, site, args, new_target| {
                let target = Obj::new(
                    Some(prototype_from(&new_target, &rt.builtins.weak_map)),
                    ObjKind::WeakMap(RefCell::new(WeakData::default())),
                );
                let init = arg(&args, 0);
                if init.is_nullish() {
                    return NResult::ok(Value::Object(target));
                }
                let rt = rt.clone();
                NResult::future(async move {
                    let t = target.clone();
                    fill_from_entries(&rt, &init, "WeakMap", site, move |k, v| {
                        let key = weak_key(&k, "as weak map key")?;
                        weak_data(&t).borrow_mut().set(&key, v);
                        Ok(())
                    })
                    .await?;
                    Ok(Value::Object(target))
                })
            })),
            callback: true,
        },
    );
    let this_weak = |this: &Value, name: &str| receiver(this, &format!("WeakMap.prototype.{name}"), is_weak_map);
    methods(
        &b.function,
        &proto,
        vec![
            // Lookups pass any key through: a non-object is never present, as the spec requires.
            method("get", 1, move |_, _, this, args| {
                let target = tri!(this_weak(&this, "get"));
                NResult::ok(match arg(&args, 0) {
                    Value::Object(k) => weak_data(&target).borrow().get(&k).unwrap_or_default(),
                    _ => Value::Undefined,
                })
            }),
            method("has", 1, move |_, _, this, args| {
                let target = tri!(this_weak(&this, "has"));
                NResult::ok(match arg(&args, 0) {
                    Value::Object(k) => weak_data(&target).borrow().has(&k),
                    _ => false,
                })
            }),
            method("delete", 1, move |_, _, this, args| {
                let target = tri!(this_weak(&this, "delete"));
                NResult::ok(match arg(&args, 0) {
                    Value::Object(k) => weak_data(&target).borrow_mut().delete(&k),
                    _ => false,
                })
            }),
            method("set", 2, move |_, _, this, args| {
                let target = tri!(this_weak(&this, "set"));
                let key = tri!(weak_key(&arg(&args, 0), "as weak map key"));
                weak_data(&target).borrow_mut().set(&key, arg(&args, 1));
                NResult::ok(this)
            }),
            method("getOrInsert", 2, move |_, _, this, args| {
                let target = tri!(this_weak(&this, "getOrInsert"));
                let key = tri!(weak_key(&arg(&args, 0), "as weak map key"));
                let mut data = weak_data(&target).borrow_mut();
                if !data.has(&key) {
                    data.set(&key, arg(&args, 1));
                }
                NResult::ok(data.get(&key).unwrap_or_default())
            }),
            method("getOrInsertComputed", 2, move |rt, site, this, args| {
                let target = tri!(this_weak(&this, "getOrInsertComputed"));
                let key = tri!(weak_key(&arg(&args, 0), "as weak map key"));
                let apply = tri!(apply_collection_callback(&arg(&args, 1), "WeakMap.getOrInsertComputed"));
                if let Some(existing) = weak_data(&target).borrow().get(&key) {
                    return NResult::ok(existing);
                }
                let rt = rt.clone();
                NResult::future(async move {
                    let value = apply(&rt, vec![Value::Object(key.clone())], Value::Undefined, site).await?;
                    weak_data(&target).borrow_mut().set(&key, value.clone());
                    Ok(value)
                })
            }),
        ],
    );
    Value::Object(weak_map)
}

pub fn weak_set_global(rt: &Rt) -> Value {
    let b = &rt.builtins;
    let proto = b.weak_set.clone();
    let weak_set = constructor(
        &b.function,
        &proto,
        "WeakSet",
        0,
        NativeFn {
            call: requires_new("WeakSet"),
            construct: Some(Rc::new(|rt, site, args, new_target| {
                let target = Obj::new(
                    Some(prototype_from(&new_target, &rt.builtins.weak_set)),
                    ObjKind::WeakSet(RefCell::new(WeakData::default())),
                );
                let init = arg(&args, 0);
                if init.is_nullish() {
                    return NResult::ok(Value::Object(target));
                }
                let rt = rt.clone();
                NResult::future(async move {
                    let cursor = iteration::iterate(&rt, &init, None, site).await?;
                    let Some(cursor) = cursor else {
                        return Err(type_error(format!(
                            "new WeakSet(...) expects a synchronous iterable, received {}.",
                            describe_value(&init)
                        )));
                    };
                    loop {
                        let step = cursor.next().await?;
                        if step.done {
                            return Ok(Value::Object(target));
                        }
                        let added = weak_key(&step.value, "in weak set").map(|key| {
                            weak_data(&target).borrow_mut().set(&key, Value::Bool(true));
                        });
                        preserve_consumer_error(&cursor, added).await?;
                    }
                })
            })),
            callback: true,
        },
    );
    let this_weak = |this: &Value, name: &str| receiver(this, &format!("WeakSet.prototype.{name}"), is_weak_set);
    methods(
        &b.function,
        &proto,
        vec![
            method("has", 1, move |_, _, this, args| {
                let target = tri!(this_weak(&this, "has"));
                NResult::ok(match arg(&args, 0) {
                    Value::Object(k) => weak_data(&target).borrow().has(&k),
                    _ => false,
                })
            }),
            method("delete", 1, move |_, _, this, args| {
                let target = tri!(this_weak(&this, "delete"));
                NResult::ok(match arg(&args, 0) {
                    Value::Object(k) => weak_data(&target).borrow_mut().delete(&k),
                    _ => false,
                })
            }),
            method("add", 1, move |_, _, this, args| {
                let target = tri!(this_weak(&this, "add"));
                let key = tri!(weak_key(&arg(&args, 0), "in weak set"));
                weak_data(&target).borrow_mut().set(&key, Value::Bool(true));
                NResult::ok(this)
            }),
        ],
    );
    Value::Object(weak_set)
}
