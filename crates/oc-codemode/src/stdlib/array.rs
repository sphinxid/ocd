//! `Array` and `Array.prototype`. Port of `stdlib/array.ts`.
//!
//! Two helpers are shared with the `Uint8Array` port (`stdlib/bytes.rs`), as upstream exports them:
//!
//! - [`sort_array`]`(rt, values, &comparator, "Uint8Array.sort", site).await` sorts a snapshot like
//!   `Array.prototype.sort`: without a comparator by the built-in string form (UTF-16 code-unit order), with one by a
//!   stable top-down merge sort that calls it as `comparator(a, b)` (a NaN result counts as equal); `undefined`
//!   values sort last and are never passed to the comparator. The comparator is validated with
//!   `apply_collection_callback` under `name`.
//! - [`callback_methods`]`("Uint8Array", Elements { receiver, length, get, wrap })` builds `map`, `filter`, `find`,
//!   `findIndex`, `findLast`, `findLastIndex`, `some`, `every`, `forEach`, `reduce`, and `reduceRight`. They fix the
//!   iteration length when called and read the elements live through the [`Elements`] accessors:
//!   - `receiver(&this, name)` narrows `this` (`name` is the bare method name, e.g. `"map"`) or returns the
//!     incompatible-receiver TypeError, e.g. `receiver(this, &format!("Uint8Array.prototype.{name}"), |o| ...)`;
//!   - `length(&obj)` is the current length; `get(&obj, index)` is the element, or `None` for a hole or an index
//!     past the end (the `index in target` test);
//!   - `wrap(rt, values)` builds what `map` and `filter` return; `None` entries are holes `map` skipped (never
//!     produced for a collection without holes).

use std::rc::Rc;

use futures::future::LocalBoxFuture;

use crate::interp::callback::{apply_collection_callback, invoke, Callback};
use crate::interp::coerce::{coerce_to_integer, coerce_to_number, coerce_to_string, with_primitives, Hint};
use crate::interp::errors::{invalid_data, range_error, type_error, JsResult};
use crate::interp::function::{
    arg, constructor, method, methods, prototype_from, receiver, Method, NResult, NativeFn,
};
use crate::interp::iteration::{self, iterator_object, preserve_consumer_error, ArrayCursor, Mode};
use crate::interp::limits::{check_array_length, check_string_length, MAX_ARRAY_LENGTH, MAX_STRING_LENGTH};
use crate::interp::references::reject_circular_insertion;
use crate::interp::{describe_value, Rt, Site};
use crate::object::{define, get, get_str, index_key, reject_addition, Obj, ObjKind, HIDDEN};
use crate::value::{JsStr, PropKey, Sym, Value};

macro_rules! tri {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(e) => return NResult::err(e),
        }
    };
}

/// A snapshot of an array's elements (`None` is a hole).
fn items_of(o: &Obj) -> Vec<Option<Value>> {
    o.array_data().map(|a| a.borrow().items.clone()).unwrap_or_default()
}

/// A snapshot with holes read as `undefined`, as spread and `[...items]` see it.
fn dense_of(o: &Obj) -> Vec<Value> {
    o.array_values()
}

fn with_items<T>(o: &Obj, f: impl FnOnce(&mut Vec<Option<Value>>) -> T) -> T {
    let data = o.array_data().expect("array receiver");
    f(&mut data.borrow_mut().items)
}

fn read_items<T>(o: &Obj, f: impl FnOnce(&Vec<Option<Value>>) -> T) -> T {
    let data = o.array_data().expect("array receiver");
    f(&data.borrow().items)
}

fn wrap(rt: &Rt, items: Vec<Option<Value>>) -> Value {
    Value::Object(Obj::array_holes(&rt.builtins.array, items))
}

fn wrap_dense(rt: &Rt, items: Vec<Value>) -> Value {
    Value::Object(Obj::array(&rt.builtins.array, items))
}

/// `optNumber`: ToIntegerOrInfinity, keeping `undefined` absent.
fn opt_number(value: &Value) -> Option<f64> {
    if value.is_undefined() { None } else { Some(coerce_to_integer(value)) }
}

/// A relative index clamped to `0..=len` (slice, fill, splice, copyWithin).
fn relative(n: f64, len: usize) -> usize {
    let length = len as f64;
    if n < 0.0 { (length + n).max(0.0) as usize } else { n.min(length) as usize }
}

fn this_array(this: &Value, name: &str) -> JsResult<Obj> {
    receiver(this, &format!("Array.prototype.{name}"), |o| o.is_array())
}

/// A mutating method fails where its first element write, delete, or `length` write would on a frozen, sealed, or
/// non-extensible array. `growth` is given by the methods that always write `length`, even when it is 0; holes a
/// method would fill on a non-extensible array are not checked.
fn mutable(target: &Obj, growth: Option<i64>) -> JsResult<()> {
    let (length, elements) = {
        let data = target.array_data().expect("array receiver").borrow();
        (data.items.len(), data.elements)
    };
    let g = growth.unwrap_or(0);
    if g > 0 {
        reject_addition(target, &length.to_string())?;
    }
    if g < 0 && length > 0 && !elements.configurable {
        return Err(type_error(format!("Cannot delete property '{}'.", length - 1)));
    }
    if !elements.writable && (length > 0 || growth.is_some()) {
        let key = if length > 0 { "0".to_string() } else { "length".to_string() };
        return Err(type_error(format!("Cannot assign to read only property '{key}'.")));
    }
    Ok(())
}

fn array_like_source(source: &Value) -> JsResult<(usize, Obj)> {
    // JS would treat a promise as an empty array-like; that would hide a missing `await`.
    if let Value::Object(o) = source
        && !o.is_promise()
    {
        let length = coerce_to_integer(&get_str(o, "length")?).max(0.0);
        check_array_length(length)?;
        return Ok((length as usize, o.clone()));
    }
    Err(invalid_data(format!(
        "Array.from expects an iterable or array-like value, received {}.",
        describe_value(source)
    )))
}

async fn array_from(rt: Rt, args: Vec<Value>, apply: Option<Callback>, site: Site) -> JsResult<Value> {
    let source = arg(&args, 0);
    let this_arg = arg(&args, 2);
    let cursor = iteration::iterate(&rt, &source, None, site).await?;
    let Some(cursor) = cursor else {
        if let Value::Object(o) = &source
            && matches!(o.kind(), ObjKind::Generator(_))
        {
            return Err(type_error(format!(
                "Array.from expects a synchronous iterable or array-like value, received {}.",
                describe_value(&source)
            )));
        }
        let (length, from) = array_like_source(&source)?;
        let mut values = Vec::new();
        for index in 0..length {
            let item = get(&from, &PropKey::Str(index_key(index)))?;
            values.push(match &apply {
                None => item,
                Some(apply) => apply(&rt, vec![item, Value::Number(index as f64)], this_arg.clone(), site).await?,
            });
        }
        return Ok(wrap_dense(&rt, values));
    };
    let mut values = Vec::new();
    let mut index = 0usize;
    loop {
        let step = cursor.next().await?;
        if step.done {
            return Ok(wrap_dense(&rt, values));
        }
        values.push(match &apply {
            None => step.value,
            Some(apply) => {
                let mapped = apply(&rt, vec![step.value, Value::Number(index as f64)], this_arg.clone(), site).await;
                preserve_consumer_error(&cursor, mapped).await?
            }
        });
        index += 1;
    }
}

fn merge_sort(
    rt: Rt,
    apply: Rc<Callback>,
    items: Vec<Value>,
    site: Site,
) -> LocalBoxFuture<'static, JsResult<Vec<Value>>> {
    Box::pin(async move {
        if items.len() <= 1 {
            return Ok(items);
        }
        let midpoint = items.len() / 2;
        let mut left = items;
        let right = left.split_off(midpoint);
        let left = merge_sort(rt.clone(), apply.clone(), left, site).await?;
        let right = merge_sort(rt.clone(), apply.clone(), right, site).await?;
        let mut merged = Vec::with_capacity(left.len() + right.len());
        let (mut li, mut ri) = (0, 0);
        while li < left.len() && ri < right.len() {
            // Treat a NaN comparator result as equal to preserve stable ordering.
            let result = apply(&rt, vec![left[li].clone(), right[ri].clone()], Value::Undefined, site).await?;
            let order = coerce_to_number(&result);
            if order.is_nan() || order <= 0.0 {
                merged.push(left[li].clone());
                li += 1;
            } else {
                merged.push(right[ri].clone());
                ri += 1;
            }
        }
        merged.extend_from_slice(&left[li..]);
        merged.extend_from_slice(&right[ri..]);
        Ok(merged)
    })
}

/// Sorts a snapshot like `Array.prototype.sort` (see the module docs). Holes should be passed as `undefined`.
pub async fn sort_array(rt: &Rt, target: Vec<Value>, comparator: &Value, name: &str, site: Site) -> JsResult<Vec<Value>> {
    if comparator.is_undefined() {
        let mut keyed: Vec<(Option<JsStr>, Value)> = target
            .into_iter()
            .map(|v| (if v.is_undefined() { None } else { Some(coerce_to_string(&v)) }, v))
            .collect();
        keyed.sort_by(|a, b| match (&a.0, &b.0) {
            (Some(x), Some(y)) => x.cmp(y),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        });
        return Ok(keyed.into_iter().map(|(_, v)| v).collect());
    }
    let apply = Rc::new(apply_collection_callback(comparator, name)?);
    let defined: Vec<Value> = target.iter().filter(|v| !v.is_undefined()).cloned().collect();
    let undefined_count = target.len() - defined.len();
    let mut sorted = merge_sort(rt.clone(), apply, defined, site).await?;
    sorted.extend(std::iter::repeat_n(Value::Undefined, undefined_count));
    Ok(sorted)
}

/// The JS `splice` on a plain list: relative start, clamped delete count.
fn splice_list(list: &mut Vec<Value>, start: f64, delete_count: Option<f64>, inserted: Vec<Value>) {
    let len = list.len();
    let from = relative(start, len);
    let count = match delete_count {
        None => len - from,
        Some(n) => n.max(0.0).min((len - from) as f64) as usize,
    };
    list.splice(from..from + count, inserted);
}

fn flatten(items: &[Option<Value>], depth: f64, out: &mut Vec<Value>) -> JsResult<()> {
    for item in items.iter().flatten() {
        match item {
            Value::Object(o) if o.is_array() && depth > 0.0 => flatten(&items_of(o), depth - 1.0, out)?,
            _ => {
                out.push(item.clone());
                if out.len() > MAX_ARRAY_LENGTH {
                    check_array_length(out.len() as f64)?;
                }
            }
        }
    }
    Ok(())
}

// Array constructs identically with or without new, like JS.
fn construct_array(args: Vec<Value>, into: &Obj) -> JsResult<Value> {
    if args.len() != 1 {
        return Ok(Value::Object(Obj::array(into, args)));
    }
    let first = args.into_iter().next().unwrap_or_default();
    let Value::Number(n) = first else { return Ok(Value::Object(Obj::array(into, vec![first]))) };
    if !n.is_finite() || n.trunc() != n || n < 0.0 || n > MAX_ARRAY_LENGTH as f64 {
        return Err(range_error("Invalid array length."));
    }
    // Sparse like JS: Array(3) has holes, and combinator loops already skip them.
    Ok(Value::Object(Obj::array_holes(into, vec![None; n as usize])))
}

pub fn array_global(rt: &Rt) -> Value {
    let b = &rt.builtins;
    let proto = b.array.clone();
    let array = constructor(
        &b.function,
        &proto,
        "Array",
        1,
        NativeFn {
            call: Rc::new(|rt, _, _, args| NResult::Ready(construct_array(args, &rt.builtins.array))),
            construct: Some(Rc::new(|rt, _, args, new_target| {
                NResult::Ready(construct_array(args, &prototype_from(&new_target, &rt.builtins.array)))
            })),
            callback: true,
        },
    );
    methods(
        &b.function,
        &array,
        vec![
            method("isArray", 1, |_, _, _, args| NResult::ok(matches!(arg(&args, 0), Value::Object(o) if o.is_array()))),
            method("of", 0, |rt, _, _, args| NResult::ok(wrap_dense(rt, args))),
            method("from", 1, |rt, site, _, args| {
                let mapper = arg(&args, 1);
                let apply = if args.len() < 2 || mapper.is_undefined() {
                    None
                } else {
                    Some(tri!(apply_collection_callback(&mapper, "Array.from")))
                };
                NResult::future(array_from(rt.clone(), args, apply, site))
            }),
        ],
    );

    let mut table: Vec<Method> = vec![
        method("join", 1, |rt, site, this, args| {
            // Holes and nullish elements join as empty strings.
            let target = tri!(this_array(&this, "join"));
            let separator = match arg(&args, 0) {
                Value::Undefined => Value::str(","),
                other => other,
            };
            let mut values = vec![separator];
            values.extend(items_of(&target).into_iter().map(|item| match item {
                None | Some(Value::Undefined) | Some(Value::Null) => Value::String(JsStr::empty()),
                Some(v) => v,
            }));
            with_primitives(rt, site, vec![Hint::String], values, |primitives| {
                let separator = coerce_to_string(&primitives[0]);
                let mut out: Vec<u16> = Vec::new();
                for (i, item) in primitives[1..].iter().enumerate() {
                    if i > 0 {
                        out.extend_from_slice(separator.units());
                    }
                    out.extend_from_slice(coerce_to_string(item).units());
                    if out.len() > MAX_STRING_LENGTH {
                        check_string_length(out.len())?;
                    }
                }
                check_string_length(out.len())?;
                Ok(Value::String(JsStr::from_units(out)))
            })
        }),
        method("toString", 0, |rt, site, this, _| {
            let target = tri!(this_array(&this, "toString"));
            let values: Vec<Value> = items_of(&target)
                .into_iter()
                .map(|item| match item {
                    None | Some(Value::Undefined) | Some(Value::Null) => Value::String(JsStr::empty()),
                    Some(v) => v,
                })
                .collect();
            with_primitives(rt, site, vec![Hint::String], values, |primitives| {
                let mut out: Vec<u16> = Vec::new();
                for (i, item) in primitives.iter().enumerate() {
                    if i > 0 {
                        out.push(b',' as u16);
                    }
                    out.extend_from_slice(coerce_to_string(item).units());
                }
                Ok(Value::String(JsStr::from_units(out)))
            })
        }),
        method("includes", 1, |_, _, this, args| {
            let target = tri!(this_array(&this, "includes"));
            let needle = arg(&args, 0);
            let from = opt_number(&arg(&args, 1)).unwrap_or(0.0);
            NResult::ok(read_items(&target, |items| {
                let len = items.len();
                if len == 0 || from == f64::INFINITY {
                    return false;
                }
                let start = if from >= 0.0 { from.min(len as f64) as usize } else { (len as f64 + from).max(0.0) as usize };
                items[start..].iter().any(|item| match item {
                    Some(v) => v.same_value_zero(&needle),
                    None => needle.is_undefined(),
                })
            }))
        }),
        method("indexOf", 1, |_, _, this, args| {
            let target = tri!(this_array(&this, "indexOf"));
            let needle = arg(&args, 0);
            let from = opt_number(&arg(&args, 1)).unwrap_or(0.0);
            NResult::ok(read_items(&target, |items| {
                let len = items.len();
                if len == 0 || from == f64::INFINITY {
                    return -1.0;
                }
                let start = if from >= 0.0 { from.min(len as f64) as usize } else { (len as f64 + from).max(0.0) as usize };
                for (i, item) in items.iter().enumerate().skip(start) {
                    if let Some(v) = item
                        && v.strict_equals(&needle)
                    {
                        return i as f64;
                    }
                }
                -1.0
            }))
        }),
        method("lastIndexOf", 1, |_, _, this, args| {
            let target = tri!(this_array(&this, "lastIndexOf"));
            let needle = arg(&args, 0);
            // An explicit undefined is a fromIndex of 0, unlike omitting it.
            let from = if args.len() < 2 { None } else { Some(opt_number(&arg(&args, 1)).unwrap_or(0.0)) };
            NResult::ok(read_items(&target, |items| {
                let len = items.len();
                if len == 0 {
                    return -1.0;
                }
                let n = from.unwrap_or(len as f64 - 1.0);
                if n == f64::NEG_INFINITY {
                    return -1.0;
                }
                let k = if n >= 0.0 { n.min(len as f64 - 1.0) } else { len as f64 + n };
                if k < 0.0 {
                    return -1.0;
                }
                let mut i = k as usize as isize;
                while i >= 0 {
                    if let Some(v) = &items[i as usize]
                        && v.strict_equals(&needle)
                    {
                        return i as f64;
                    }
                    i -= 1;
                }
                -1.0
            }))
        }),
        method("at", 1, |_, _, this, args| {
            let target = tri!(this_array(&this, "at"));
            let n = opt_number(&arg(&args, 0)).unwrap_or(0.0);
            NResult::ok(read_items(&target, |items| {
                let len = items.len() as f64;
                let k = if n >= 0.0 { n } else { len + n };
                if k < 0.0 || k >= len { Value::Undefined } else { items[k as usize].clone().unwrap_or_default() }
            }))
        }),
        method("slice", 2, |rt, _, this, args| {
            let target = tri!(this_array(&this, "slice"));
            let start = opt_number(&arg(&args, 0));
            let end = opt_number(&arg(&args, 1));
            let sliced = read_items(&target, |items| {
                let len = items.len();
                let from = relative(start.unwrap_or(0.0), len);
                let to = end.map(|e| relative(e, len)).unwrap_or(len);
                if to <= from { Vec::new() } else { items[from..to].to_vec() }
            });
            NResult::ok(wrap(rt, sliced))
        }),
        method("concat", 1, |rt, _, this, args| {
            let target = tri!(this_array(&this, "concat"));
            let mut joined = items_of(&target);
            let total = joined.len()
                + args
                    .iter()
                    .map(|item| match item {
                        Value::Object(o) if o.is_array() => o.array_len(),
                        _ => 1,
                    })
                    .sum::<usize>();
            tri!(check_array_length(total as f64));
            for item in args {
                match &item {
                    Value::Object(o) if o.is_array() => joined.extend(items_of(o)),
                    _ => joined.push(Some(item)),
                }
            }
            NResult::ok(wrap(rt, joined))
        }),
        method("flat", 0, |rt, _, this, args| {
            let target = tri!(this_array(&this, "flat"));
            let depth = opt_number(&arg(&args, 0)).unwrap_or(1.0);
            let mut out = Vec::new();
            tri!(flatten(&items_of(&target), depth, &mut out));
            tri!(check_array_length(out.len() as f64));
            NResult::ok(wrap_dense(rt, out))
        }),
        method("reverse", 0, |_, _, this, _| {
            let target = tri!(this_array(&this, "reverse"));
            // Fewer than two elements means no writes at all, so a frozen one-element array reverses fine.
            if target.array_len() > 1 {
                tri!(mutable(&target, None));
            }
            with_items(&target, |items| items.reverse());
            NResult::ok(Value::Object(target))
        }),
        method("sort", 1, |rt, site, this, args| {
            let target = tri!(this_array(&this, "sort"));
            tri!(mutable(&target, None));
            let items = items_of(&target);
            let length = items.len();
            let hole_count = items.iter().filter(|i| i.is_none()).count();
            let item_count = length - hole_count;
            let values: Vec<Value> = items.into_iter().map(Option::unwrap_or_default).collect();
            let comparator = arg(&args, 0);
            let rt = rt.clone();
            NResult::future(async move {
                let sorted = sort_array(&rt, values, &comparator, "Array.sort", site).await?;
                with_items(&target, |items| {
                    for (index, item) in sorted.into_iter().take(item_count).enumerate() {
                        if index >= items.len() {
                            items.resize(index + 1, None);
                        }
                        items[index] = Some(item);
                    }
                    for index in item_count..item_count + hole_count {
                        if index < items.len() {
                            items[index] = None;
                        }
                    }
                });
                Ok(Value::Object(target))
            })
        }),
        method("toSorted", 1, |rt, site, this, args| {
            let target = tri!(this_array(&this, "toSorted"));
            let values = dense_of(&target);
            let comparator = arg(&args, 0);
            let rt = rt.clone();
            NResult::future(async move {
                let sorted = sort_array(&rt, values, &comparator, "Array.toSorted", site).await?;
                Ok(wrap_dense(&rt, sorted))
            })
        }),
        method("toReversed", 0, |rt, _, this, _| {
            let target = tri!(this_array(&this, "toReversed"));
            let mut values = dense_of(&target);
            values.reverse();
            NResult::ok(wrap_dense(rt, values))
        }),
        method("with", 2, |rt, _, this, args| {
            let target = tri!(this_array(&this, "with"));
            let mut copied = dense_of(&target);
            let index = opt_number(&arg(&args, 0)).unwrap_or(0.0);
            let len = copied.len() as f64;
            let resolved = if index < 0.0 { len + index } else { index };
            if resolved < 0.0 || resolved >= len {
                return NResult::err(range_error("Array.with index is out of range."));
            }
            copied[resolved as usize] = arg(&args, 1);
            NResult::ok(wrap_dense(rt, copied))
        }),
        method("push", 1, |_, _, this, args| {
            let target = tri!(this_array(&this, "push"));
            tri!(mutable(&target, Some(args.len() as i64)));
            // Validate all insertions before mutating to avoid partial cyclic updates.
            for item in &args {
                tri!(reject_circular_insertion(&target, item, "Array.push result"));
            }
            let length = with_items(&target, |items| {
                items.extend(args.into_iter().map(Some));
                items.len()
            });
            NResult::ok(length as f64)
        }),
        method("unshift", 1, |_, _, this, args| {
            let target = tri!(this_array(&this, "unshift"));
            tri!(mutable(&target, Some(args.len() as i64)));
            for item in &args {
                tri!(reject_circular_insertion(&target, item, "Array.unshift result"));
            }
            let length = with_items(&target, |items| {
                items.splice(0..0, args.into_iter().map(Some));
                items.len()
            });
            NResult::ok(length as f64)
        }),
        method("pop", 0, |_, _, this, _| {
            let target = tri!(this_array(&this, "pop"));
            tri!(mutable(&target, Some(-1)));
            NResult::ok(with_items(&target, |items| items.pop().flatten().unwrap_or_default()))
        }),
        method("shift", 0, |_, _, this, _| {
            let target = tri!(this_array(&this, "shift"));
            tri!(mutable(&target, Some(-1)));
            NResult::ok(with_items(&target, |items| {
                if items.is_empty() { Value::Undefined } else { items.remove(0).unwrap_or_default() }
            }))
        }),
        method("splice", 2, |rt, _, this, args| {
            let target = tri!(this_array(&this, "splice"));
            let length = target.array_len();
            let start = opt_number(&arg(&args, 0)).unwrap_or(0.0);
            let from = relative(start, length);
            let delete_count = if args.len() == 1 {
                length - from
            } else {
                opt_number(&arg(&args, 1)).unwrap_or(0.0).max(0.0).min((length - from) as f64) as usize
            };
            let inserted: Vec<Value> = args.get(2..).map(|s| s.to_vec()).unwrap_or_default();
            for item in &inserted {
                tri!(reject_circular_insertion(&target, item, "Array.splice result"));
            }
            tri!(mutable(&target, Some(inserted.len() as i64 - delete_count as i64)));
            let removed = with_items(&target, |items| {
                // Program code ran nowhere since `length` was read, but stay in bounds regardless.
                let from = from.min(items.len());
                let end = (from + delete_count).min(items.len());
                items.splice(from..end, inserted.into_iter().map(Some)).collect::<Vec<_>>()
            });
            NResult::ok(wrap(rt, removed))
        }),
        method("toSpliced", 2, |rt, _, this, args| {
            let target = tri!(this_array(&this, "toSpliced"));
            let mut copied = dense_of(&target);
            if args.is_empty() {
                return NResult::ok(wrap_dense(rt, copied));
            }
            let start = opt_number(&arg(&args, 0)).unwrap_or(0.0);
            if args.len() == 1 {
                splice_list(&mut copied, start, None, Vec::new());
            } else {
                let count = opt_number(&arg(&args, 1)).unwrap_or(0.0);
                splice_list(&mut copied, start, Some(count), args[2..].to_vec());
            }
            NResult::ok(wrap_dense(rt, copied))
        }),
        method("fill", 1, |_, _, this, args| {
            let target = tri!(this_array(&this, "fill"));
            tri!(mutable(&target, None));
            let value = arg(&args, 0);
            tri!(reject_circular_insertion(&target, &value, "Array.fill result"));
            let start = opt_number(&arg(&args, 1));
            let end = opt_number(&arg(&args, 2));
            with_items(&target, |items| {
                let len = items.len();
                let from = relative(start.unwrap_or(0.0), len);
                let to = end.map(|e| relative(e, len)).unwrap_or(len);
                for item in items.iter_mut().take(to).skip(from) {
                    *item = Some(value.clone());
                }
            });
            NResult::ok(Value::Object(target))
        }),
        method("copyWithin", 2, |_, _, this, args| {
            let target = tri!(this_array(&this, "copyWithin"));
            tri!(mutable(&target, None));
            let to = opt_number(&arg(&args, 0)).unwrap_or(0.0);
            let start = opt_number(&arg(&args, 1)).unwrap_or(0.0);
            let end = opt_number(&arg(&args, 2));
            with_items(&target, |items| {
                let len = items.len();
                let to = relative(to, len);
                let from = relative(start, len);
                let last = end.map(|e| relative(e, len)).unwrap_or(len);
                let count = last.saturating_sub(from).min(len - to);
                if count > 0 {
                    let source: Vec<Option<Value>> = items[from..from + count].to_vec();
                    items[to..to + count].clone_from_slice(&source);
                }
            });
            NResult::ok(Value::Object(target))
        }),
        method("toLocaleString", 0, |rt, site, this, _| {
            let target = tri!(this_array(&this, "toLocaleString"));
            let values = dense_of(&target);
            let rt = rt.clone();
            NResult::future(async move {
                let mut out: Vec<u16> = Vec::new();
                for (i, item) in values.iter().enumerate() {
                    if i > 0 {
                        out.push(b',' as u16);
                    }
                    if item.is_nullish() {
                        continue;
                    }
                    let shown = invoke(&rt, item, "toLocaleString", "Array.prototype.toLocaleString", site).await?;
                    out.extend_from_slice(coerce_to_string(&shown).units());
                }
                Ok(Value::String(JsStr::from_units(out)))
            })
        }),
        method("keys", 0, |rt, _, this, _| {
            let target = tri!(this_array(&this, "keys"));
            NResult::ok(iterator_object(rt, ArrayCursor::new(rt, target, Mode::Keys)))
        }),
        method("values", 0, |rt, _, this, _| {
            let target = tri!(this_array(&this, "values"));
            NResult::ok(iterator_object(rt, ArrayCursor::new(rt, target, Mode::Values)))
        }),
        method("entries", 0, |rt, _, this, _| {
            let target = tri!(this_array(&this, "entries"));
            NResult::ok(iterator_object(rt, ArrayCursor::new(rt, target, Mode::Entries)))
        }),
        method("flatMap", 1, |rt, site, this, args| {
            let target = tri!(this_array(&this, "flatMap"));
            let apply = tri!(apply_collection_callback(&arg(&args, 0), "Array.flatMap"));
            let this_arg = arg(&args, 1);
            let rt = rt.clone();
            NResult::future(async move {
                let length = target.array_len();
                let mut values = Vec::new();
                for index in 0..length {
                    let Some(item) = array_element(&target, index) else { continue };
                    let mapped = apply(
                        &rt,
                        vec![item, Value::Number(index as f64), Value::Object(target.clone())],
                        this_arg.clone(),
                        site,
                    )
                    .await?;
                    match &mapped {
                        // Holes are skipped, as flattening does; they never become undefined.
                        Value::Object(o) if o.is_array() => {
                            values.extend((0..o.array_len()).filter_map(|i| array_element(o, i)))
                        }
                        _ => values.push(mapped),
                    }
                }
                Ok(wrap_dense(&rt, values))
            })
        }),
    ];
    table.extend(callback_methods(
        "Array",
        Elements {
            receiver: Rc::new(|this, name| this_array(this, name)),
            length: Rc::new(|o| o.array_len()),
            get: Rc::new(array_element),
            wrap: Rc::new(wrap),
        },
    ));
    methods(&b.function, &proto, table);
    let values = get(&proto, &PropKey::str("values")).unwrap_or_default();
    define(&proto, PropKey::Sym(Sym::Iterator), values, HIDDEN);
    Value::Object(array)
}

/// An array element, or `None` for a hole or an index past the end.
pub fn array_element(target: &Obj, index: usize) -> Option<Value> {
    target.array_data().and_then(|a| a.borrow().items.get(index).cloned().flatten())
}

/// How [`callback_methods`] reads its receiver; see the module docs.
#[derive(Clone)]
pub struct Elements {
    pub receiver: Rc<dyn Fn(&Value, &str) -> JsResult<Obj>>,
    pub length: Rc<dyn Fn(&Obj) -> usize>,
    pub get: Rc<dyn Fn(&Obj, usize) -> Option<Value>>,
    pub wrap: Rc<dyn Fn(&Rt, Vec<Option<Value>>) -> Value>,
}

/// One callback-method call: the receiver, its accessors, and the admitted callback.
struct Walk {
    rt: Rt,
    site: Site,
    elements: Elements,
    target: Obj,
    call: Callback,
    this_arg: Value,
    args: Vec<Value>,
    label: &'static str,
}

impl Walk {
    fn len(&self) -> usize {
        (self.elements.length)(&self.target)
    }
    fn at(&self, index: usize) -> Option<Value> {
        (self.elements.get)(&self.target, index)
    }
    async fn apply(&self, args: Vec<Value>) -> JsResult<Value> {
        (self.call)(&self.rt, args, self.this_arg.clone(), self.site).await
    }
    /// `[element, index, receiver]`.
    async fn visit(&self, item: Value, index: usize) -> JsResult<Value> {
        self.apply(vec![item, Value::Number(index as f64), Value::Object(self.target.clone())]).await
    }
}

type Body = fn(Walk) -> LocalBoxFuture<'static, JsResult<Value>>;

async fn walk_map(w: Walk) -> JsResult<Value> {
    let length = w.len();
    let mut values: Vec<Option<Value>> = vec![None; length];
    for (index, slot) in values.iter_mut().enumerate() {
        let Some(item) = w.at(index) else { continue };
        *slot = Some(w.visit(item, index).await?);
    }
    Ok((w.elements.wrap)(&w.rt, values))
}

async fn walk_filter(w: Walk) -> JsResult<Value> {
    let length = w.len();
    let mut values = Vec::new();
    for index in 0..length {
        let Some(item) = w.at(index) else { continue };
        if w.visit(item.clone(), index).await?.truthy() {
            values.push(Some(item));
        }
    }
    Ok((w.elements.wrap)(&w.rt, values))
}

async fn walk_find(w: Walk) -> JsResult<Value> {
    let length = w.len();
    for index in 0..length {
        let item = w.at(index).unwrap_or_default();
        if w.visit(item.clone(), index).await?.truthy() {
            return Ok(item);
        }
    }
    Ok(Value::Undefined)
}

async fn walk_find_index(w: Walk) -> JsResult<Value> {
    let length = w.len();
    for index in 0..length {
        if w.visit(w.at(index).unwrap_or_default(), index).await?.truthy() {
            return Ok(Value::Number(index as f64));
        }
    }
    Ok(Value::Number(-1.0))
}

async fn walk_find_last(w: Walk) -> JsResult<Value> {
    for index in (0..w.len()).rev() {
        let item = w.at(index).unwrap_or_default();
        if w.visit(item.clone(), index).await?.truthy() {
            return Ok(item);
        }
    }
    Ok(Value::Undefined)
}

async fn walk_find_last_index(w: Walk) -> JsResult<Value> {
    for index in (0..w.len()).rev() {
        if w.visit(w.at(index).unwrap_or_default(), index).await?.truthy() {
            return Ok(Value::Number(index as f64));
        }
    }
    Ok(Value::Number(-1.0))
}

async fn walk_some(w: Walk) -> JsResult<Value> {
    let length = w.len();
    for index in 0..length {
        let Some(item) = w.at(index) else { continue };
        if w.visit(item, index).await?.truthy() {
            return Ok(Value::Bool(true));
        }
    }
    Ok(Value::Bool(false))
}

async fn walk_every(w: Walk) -> JsResult<Value> {
    let length = w.len();
    for index in 0..length {
        let Some(item) = w.at(index) else { continue };
        if !w.visit(item, index).await?.truthy() {
            return Ok(Value::Bool(false));
        }
    }
    Ok(Value::Bool(true))
}

async fn walk_for_each(w: Walk) -> JsResult<Value> {
    let length = w.len();
    for index in 0..length {
        if let Some(item) = w.at(index) {
            w.visit(item, index).await?;
        }
    }
    Ok(Value::Undefined)
}

async fn walk_reduce(w: Walk) -> JsResult<Value> {
    let length = w.len();
    let mut start = 0;
    let mut accumulator = arg(&w.args, 1);
    if w.args.len() < 2 {
        while start < length && w.at(start).is_none() {
            start += 1;
        }
        if start == length {
            return Err(type_error(format!("{}.reduce of an empty array with no initial value.", w.label)));
        }
        accumulator = w.at(start).unwrap_or_default();
        start += 1;
    }
    for index in start..length {
        let Some(item) = w.at(index) else { continue };
        accumulator = w
            .apply(vec![accumulator, item, Value::Number(index as f64), Value::Object(w.target.clone())])
            .await?;
    }
    Ok(accumulator)
}

async fn walk_reduce_right(w: Walk) -> JsResult<Value> {
    let mut start = w.len() as isize - 1;
    let mut accumulator = arg(&w.args, 1);
    if w.args.len() < 2 {
        while start >= 0 && w.at(start as usize).is_none() {
            start -= 1;
        }
        if start < 0 {
            return Err(type_error(format!("{}.reduceRight of an empty array with no initial value.", w.label)));
        }
        accumulator = w.at(start as usize).unwrap_or_default();
        start -= 1;
    }
    let mut index = start;
    while index >= 0 {
        if let Some(item) = w.at(index as usize) {
            accumulator = w
                .apply(vec![accumulator, item, Value::Number(index as f64), Value::Object(w.target.clone())])
                .await?;
        }
        index -= 1;
    }
    Ok(accumulator)
}

/// The callback methods Array and Uint8Array share; see the module docs.
pub fn callback_methods(label: &'static str, elements: Elements) -> Vec<Method> {
    let iterate = |name: &'static str, body: Body| -> Method {
        let elements = elements.clone();
        method(name, 1, move |rt, site, this, args| {
            let target = tri!((elements.receiver)(&this, name));
            let call = tri!(apply_collection_callback(&arg(&args, 0), &format!("{label}.{name}")));
            // reduce and reduceRight take an initial value where the others take a thisArg.
            let this_arg = if name.starts_with("reduce") { Value::Undefined } else { arg(&args, 1) };
            NResult::Future(body(Walk {
                rt: rt.clone(),
                site,
                elements: elements.clone(),
                target,
                call,
                this_arg,
                args,
                label,
            }))
        })
    };
    vec![
        iterate("map", |w| Box::pin(walk_map(w))),
        iterate("filter", |w| Box::pin(walk_filter(w))),
        iterate("find", |w| Box::pin(walk_find(w))),
        iterate("findIndex", |w| Box::pin(walk_find_index(w))),
        iterate("findLast", |w| Box::pin(walk_find_last(w))),
        iterate("findLastIndex", |w| Box::pin(walk_find_last_index(w))),
        iterate("some", |w| Box::pin(walk_some(w))),
        iterate("every", |w| Box::pin(walk_every(w))),
        iterate("forEach", |w| Box::pin(walk_for_each(w))),
        iterate("reduce", |w| Box::pin(walk_reduce(w))),
        iterate("reduceRight", |w| Box::pin(walk_reduce_right(w))),
    ]
}
