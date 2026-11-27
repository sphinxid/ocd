//! `Uint8Array`, `TextEncoder`, and `TextDecoder`. Port of `stdlib/bytes.ts`.
//!
//! A Uint8Array is a [`BytesData`] view over a shared buffer (`subarray` shares it). Upstream delegates the element
//! work to the host typed array; the index arithmetic, clamping, base64/hex codecs, and UTF-8 codec here follow the
//! same JS semantics. The callback methods and the comparator sort are the shared `Array` helpers
//! (`stdlib::array::callback_methods` and `sort_array`), as upstream reuses `callbackMethods` and `sortArray`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};

use crate::interp::coerce::{coerce_to_integer, coerce_to_number, coerce_to_string};
use crate::interp::callback::{Callback, apply_collection_callback};
use crate::interp::errors::{JsResult, range_error, syntax_error, type_error};
use crate::interp::function::{
    Method, NResult, NativeFn, arg, constructor, method, methods, prototype_from, receiver, requires_new,
};
use crate::interp::iteration::{ArrayCursor, Mode, iterate, iterator_object};
use crate::interp::limits::{check_array_length, check_string_length};
use crate::interp::{Rt, Site, describe_value};
use crate::object::{BytesData, FROZEN, Getter, HIDDEN, Obj, ObjData, ObjKind, define, define_accessor, get, to_uint8};
use crate::stdlib::array::{Elements, sort_array};
use crate::value::{JsStr, PropKey, Sym, Value};

fn bytes_obj(proto: &Obj, bytes: Vec<u8>) -> Obj {
    Obj::new(Some(proto.clone()), ObjKind::Bytes(BytesData::new(bytes)))
}

/// New bytes of `source`'s kind, as `slice`, `subarray`, `map` and `filter` make them: a Buffer's are a Buffer
/// (2.4.1, #64933, as JS's species does).
fn derive(rt: &Rt, source: &BytesData, bytes: BytesData) -> Obj {
    let proto = if source.node { &rt.builtins.buffer } else { &rt.builtins.uint8_array };
    Obj::new(Some(proto.clone()), ObjKind::Bytes(BytesData { node: source.node, ..bytes }))
}

fn view_of(o: &Obj) -> &BytesData {
    o.bytes().expect("receiver checked")
}

/// The bytes a Uint8Array, array, or other iterable of numbers describes; each value wraps to a byte. As in
/// TypedArray.from, a mapper sees the original values, after the source is consumed in full.
async fn collect_bytes(
    rt: &Rt,
    source: &Value,
    name: &str,
    site: Site,
    mapper: Option<(Callback, Value)>,
) -> JsResult<Vec<u8>> {
    if let Value::Object(o) = source
        && let Some(b) = o.bytes()
        && mapper.is_none()
    {
        return Ok(b.to_vec());
    }
    let Some(cursor) = iterate(rt, source, None, site).await? else {
        return Err(type_error(format!(
            "{name} expects a Uint8Array, an array, or an iterable of numbers, received {}.",
            describe_value(source)
        )));
    };
    let mut values: Vec<Value> = Vec::new();
    loop {
        let step = cursor.next().await?;
        if step.done {
            break;
        }
        values.push(step.value);
        check_array_length(values.len() as f64)?;
    }
    if let Some((apply, this_arg)) = mapper {
        let mut mapped = Vec::with_capacity(values.len());
        for (index, value) in values.into_iter().enumerate() {
            mapped.push(apply(rt, vec![value, Value::Number(index as f64)], this_arg.clone(), site).await?);
        }
        values = mapped;
    }
    Ok(values.iter().map(|v| to_uint8(coerce_to_number(v))).collect())
}

/// Uint8Array cannot contain NaN, so non-number search values never match a byte.
fn search_value(value: &Value) -> f64 {
    match value {
        Value::Number(n) => *n,
        _ => f64::NAN,
    }
}

/// An optional integer argument: `undefined` stays absent, anything else is ToIntegerOrInfinity.
fn opt_number(value: &Value) -> Option<f64> {
    if value.is_undefined() { None } else { Some(coerce_to_integer(value)) }
}

/// A relative index (`slice`, `subarray`, `fill`) resolved against `len`.
fn relative(value: Option<f64>, len: usize, default: usize) -> usize {
    match value {
        None => default,
        Some(n) if n < 0.0 => (len as f64 + n).max(0.0) as usize,
        Some(n) => n.min(len as f64) as usize,
    }
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard padded base64.
pub fn encode_base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n =
            (chunk[0] as u32) << 16 | (*chunk.get(1).unwrap_or(&0) as u32) << 8 | *chunk.get(2).unwrap_or(&0) as u32;
        out.push(BASE64[(n >> 18) as usize & 63] as char);
        out.push(BASE64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { BASE64[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { BASE64[n as usize & 63] as char } else { '=' });
    }
    out
}

pub fn base64_digit(c: u16) -> Option<u32> {
    match c {
        0x41..=0x5A => Some((c - 0x41) as u32),
        0x61..=0x7A => Some((c - 0x61 + 26) as u32),
        0x30..=0x39 => Some((c - 0x30 + 52) as u32),
        0x2B => Some(62),
        0x2F => Some(63),
        _ => None,
    }
}

fn is_ascii_whitespace(c: u16) -> bool {
    matches!(c, 0x09 | 0x0A | 0x0C | 0x0D | 0x20)
}

/// Decodes a partial or full chunk of base64 digits; `strict` refuses leftover bits that are not zero.
fn decode_chunk(chunk: &[u32], out: &mut Vec<u8>, strict: bool) -> Option<()> {
    let mut n: u32 = 0;
    for (i, &d) in chunk.iter().enumerate() {
        n |= d << (18 - 6 * i);
    }
    if strict && ((chunk.len() == 2 && n & 0xFFFF != 0) || (chunk.len() == 3 && n & 0xFF != 0)) {
        return None;
    }
    let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
    out.extend_from_slice(&bytes[..chunk.len().saturating_sub(1)]);
    Some(())
}

/// `Uint8Array.fromBase64`'s `lastChunkHandling` (2.4.1, #64931).
#[derive(Clone, Copy, PartialEq, Eq)]
enum LastChunk {
    Loose,
    Strict,
    StopBeforePartial,
}

/// The spec's FromBase64 (as Bun gives it): `None` is the SyntaxError.
fn from_base64(units: &[u16], url: bool, last: LastChunk) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut chunk: Vec<u32> = Vec::with_capacity(4);
    let len = units.len();
    let mut i = 0;
    let skip = |mut i: usize| {
        while i < len && is_ascii_whitespace(units[i]) {
            i += 1;
        }
        i
    };
    loop {
        i = skip(i);
        if i == len {
            if !chunk.is_empty() {
                match last {
                    LastChunk::StopBeforePartial => return Some(out),
                    LastChunk::Strict => return None,
                    LastChunk::Loose => {
                        if chunk.len() == 1 {
                            return None;
                        }
                        decode_chunk(&chunk, &mut out, false)?;
                    }
                }
            }
            return Some(out);
        }
        let c = units[i];
        i += 1;
        if c == b'=' as u16 {
            if chunk.len() < 2 {
                return None;
            }
            i = skip(i);
            if chunk.len() == 2 {
                if i == len {
                    return if last == LastChunk::StopBeforePartial { Some(out) } else { None };
                }
                if units[i] == b'=' as u16 {
                    i = skip(i + 1);
                }
            }
            if i < len {
                return None;
            }
            decode_chunk(&chunk, &mut out, last == LastChunk::Strict)?;
            return Some(out);
        }
        let c = match c {
            0x2B | 0x2F if url => return None,
            0x2D if url => 0x2B,
            0x5F if url => 0x2F,
            c => c,
        };
        chunk.push(base64_digit(c)?);
        if chunk.len() == 4 {
            decode_chunk(&chunk, &mut out, false)?;
            chunk.clear();
        }
    }
}

/// `toBase64` with its `alphabet` and `omitPadding` options (2.4.1, #64931).
fn to_base64(bytes: &[u8], url: bool, omit_padding: bool) -> String {
    let mut text = encode_base64(bytes);
    if omit_padding {
        text.truncate(text.trim_end_matches('=').len());
    }
    if url {
        text = text.replace('+', "-").replace('/', "_");
    }
    text
}

/// The base64 methods' options: `undefined` or an object (`<name> requires that options be an object.`).
fn base64_options(value: &Value, name: &str) -> JsResult<Option<Obj>> {
    match value {
        Value::Undefined => Ok(None),
        Value::Object(o) => Ok(Some(o.clone())),
        _ => Err(type_error(format!("{name} requires that options be an object."))),
    }
}

/// A string option of the base64 methods: one of `allowed`, the first by default; the index of the one given.
fn base64_option(options: &Option<Obj>, key: &str, allowed: &[&str], name: &str) -> JsResult<usize> {
    let value = match options {
        Some(o) => get(o, &PropKey::str(key))?,
        None => Value::Undefined,
    };
    if value.is_undefined() {
        return Ok(0);
    }
    if let Value::String(s) = &value
        && let Some(at) = allowed.iter().position(|a| s.eq_str(a))
    {
        return Ok(at);
    }
    let shown: Vec<String> = allowed.iter().map(|a| format!("\"{a}\"")).collect();
    Err(type_error(format!("{name} requires that {key} be {}.", shown.join(" or "))))
}

fn from_hex(units: &[u16]) -> Option<Vec<u8>> {
    if !units.len().is_multiple_of(2) {
        return None;
    }
    let digit = |c: u16| char::from_u32(c as u32).and_then(|c| c.to_digit(16));
    units.chunks(2).map(|p| Some((digit(p[0])? * 16 + digit(p[1])?) as u8)).collect()
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn join(bytes: &[u8], separator: &JsStr) -> JsResult<JsStr> {
    let mut out: Vec<u16> = Vec::new();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 {
            out.extend_from_slice(separator.units());
        }
        out.extend(b.to_string().encode_utf16());
        check_string_length(out.len())?;
    }
    Ok(JsStr::from_units(out))
}

fn bytes_receiver(this: &Value, name: &str) -> JsResult<Obj> {
    receiver(this, &format!("Uint8Array.prototype.{name}"), |o| o.bytes().is_some())
}

/// The Array callback methods over a byte view (upstream `callbackMethods(ctx, "Uint8Array", ...)`).
fn callback_methods() -> Vec<Method> {
    crate::stdlib::array::callback_methods(
        "Uint8Array",
        Elements {
            receiver: Rc::new(bytes_receiver),
            length: Rc::new(|o| view_of(o).length),
            get: Rc::new(|o, index| view_of(o).get(index).map(|b| Value::Number(b as f64))),
            wrap: Rc::new(|rt, target, values| {
                let bytes = values.iter().map(|v| to_uint8(coerce_to_number(&v.clone().unwrap_or_default()))).collect();
                Value::Object(derive(rt, view_of(target), BytesData::new(bytes)))
            }),
        },
    )
}

pub fn uint8_array_global(rt: &Rt) -> Value {
    let proto = rt.builtins.uint8_array.clone();
    let fp = rt.builtins.function.clone();
    let construct_proto = proto.clone();
    let ctor = constructor(
        &fp,
        &proto,
        "Uint8Array",
        3,
        NativeFn {
            call: requires_new("Uint8Array"),
            construct: Some(Rc::new(move |rt, site, args, new_target| {
                let into = prototype_from(&new_target, &construct_proto);
                let source = arg(&args, 0);
                // Upstream tests `typeof source === "object"`, which every interpreter object (functions included)
                // and tool reference passes.
                if matches!(source, Value::Object(_) | Value::Tool(_)) {
                    let rt = rt.clone();
                    return NResult::future(async move {
                        let bytes = collect_bytes(&rt, &source, "new Uint8Array(...)", site, None).await?;
                        Ok(Value::Object(bytes_obj(&into, bytes)))
                    });
                }
                let length = if source.is_undefined() { 0.0 } else { coerce_to_number(&source) };
                if !(length.is_finite() && length.fract() == 0.0) || length < 0.0 {
                    return NResult::err(range_error(format!(
                        "Invalid typed array length: {}",
                        coerce_to_string(&source).to_rust()
                    )));
                }
                if let Err(e) = check_array_length(length) {
                    return NResult::err(e);
                }
                NResult::ok(Value::Object(bytes_obj(&into, vec![0; length as usize])))
            })),
            callback: true,
        },
    );
    let decode = |name: &'static str| -> Method {
        method(name, 1, move |rt, _, _, args| {
            let Value::String(text) = arg(&args, 0) else {
                return NResult::err(type_error(format!("Uint8Array.{name} expects a string.")));
            };
            let decoded = if name == "fromHex" {
                from_hex(text.units())
            } else {
                let label = "Uint8Array.fromBase64";
                let read = base64_options(&arg(&args, 1), label).and_then(|options| {
                    let alphabet = base64_option(&options, "alphabet", &["base64", "base64url"], label)?;
                    let last = base64_option(&options, "lastChunkHandling", &["loose", "strict", "stop-before-partial"], label)?;
                    Ok((alphabet == 1, [LastChunk::Loose, LastChunk::Strict, LastChunk::StopBeforePartial][last]))
                });
                match read {
                    Ok((url, last)) => from_base64(text.units(), url, last),
                    Err(e) => return NResult::err(e),
                }
            };
            match decoded {
                Some(bytes) => NResult::ok(Value::Object(bytes_obj(&rt.builtins.uint8_array, bytes))),
                None => NResult::err(syntax_error(format!(
                    "Uint8Array.{name}: the string is not valid {}.",
                    if name == "fromHex" { "hex" } else { "base64" }
                ))),
            }
        })
    };
    methods(
        &fp,
        &ctor,
        vec![
            method("from", 1, |rt, site, _, args| {
                let mapper = match arg(&args, 1) {
                    Value::Undefined => None,
                    mapper => match apply_collection_callback(&mapper, "Uint8Array.from") {
                        Ok(apply) => Some((apply, arg(&args, 2))),
                        Err(e) => return NResult::err(e),
                    },
                };
                let rt = rt.clone();
                NResult::future(async move {
                    let bytes = collect_bytes(&rt, &arg(&args, 0), "Uint8Array.from", site, mapper).await?;
                    Ok(Value::Object(bytes_obj(&rt.builtins.uint8_array, bytes)))
                })
            }),
            method("of", 0, |rt, _, _, args| {
                let bytes = args.iter().map(|v| to_uint8(coerce_to_number(v))).collect();
                NResult::ok(Value::Object(bytes_obj(&rt.builtins.uint8_array, bytes)))
            }),
            decode("fromBase64"),
            decode("fromHex"),
        ],
    );

    // 2.4.1 (#64930): one byte per element, on the constructor and its instances.
    define(&ctor, "BYTES_PER_ELEMENT", Value::Number(1.0), FROZEN);
    define(&proto, "BYTES_PER_ELEMENT", Value::Number(1.0), FROZEN);
    let length: Getter = Rc::new(|this| Ok(Value::Number(view_of(&bytes_receiver(this, "length")?).length as f64)));
    define_accessor(&proto, "length", Some(length), None);
    let mut table = vec![
        method("at", 1, |_, _, this, args| {
            NResult::from(bytes_receiver(&this, "at").map(|o| {
                let view = view_of(&o);
                let n = opt_number(&arg(&args, 0)).unwrap_or(0.0);
                let k = if n >= 0.0 { n } else { view.length as f64 + n };
                if k < 0.0 || k >= view.length as f64 {
                    Value::Undefined
                } else {
                    view.get(k as usize).map(|b| Value::Number(b as f64)).unwrap_or_default()
                }
            }))
        }),
        method("slice", 2, |rt, _, this, args| {
            NResult::from(bytes_receiver(&this, "slice").map(|o| {
                let view = view_of(&o);
                let start = relative(opt_number(&arg(&args, 0)), view.length, 0);
                let end = relative(opt_number(&arg(&args, 1)), view.length, view.length);
                let bytes = view.to_vec();
                let slice = if start < end { bytes[start..end].to_vec() } else { Vec::new() };
                Value::Object(derive(rt, view, BytesData::new(slice)))
            }))
        }),
        // A view on the same bytes, as in JS: writes through one are visible through the other.
        method("subarray", 2, |rt, _, this, args| {
            NResult::from(bytes_receiver(&this, "subarray").map(|o| {
                let view = view_of(&o);
                let start = relative(opt_number(&arg(&args, 0)), view.length, 0);
                let end = relative(opt_number(&arg(&args, 1)), view.length, view.length);
                let sub = BytesData {
                    buffer: view.buffer.clone(),
                    offset: view.offset + start,
                    length: end.saturating_sub(start),
                    node: false,
                };
                Value::Object(derive(rt, view, sub))
            }))
        }),
        method("set", 1, |rt, site, this, args| {
            let target = match bytes_receiver(&this, "set") {
                Ok(t) => t,
                Err(e) => return NResult::err(e),
            };
            let offset = opt_number(&arg(&args, 1)).unwrap_or(0.0);
            let rt = rt.clone();
            NResult::future(async move {
                let source = collect_bytes(&rt, &arg(&args, 0), "Uint8Array.set", site, None).await?;
                let view = view_of(&target);
                if !(offset.is_finite() && offset >= 0.0) || source.len() as f64 + offset > view.length as f64 {
                    return Err(range_error("Uint8Array.set: the source does not fit at that offset."));
                }
                for (i, b) in source.iter().enumerate() {
                    view.set(offset as usize + i, *b);
                }
                Ok(Value::Undefined)
            })
        }),
        method("fill", 1, |_, _, this, args| {
            NResult::from(bytes_receiver(&this, "fill").map(|o| {
                let view = view_of(&o);
                let value = to_uint8(coerce_to_number(&arg(&args, 0)));
                let start = relative(opt_number(&arg(&args, 1)), view.length, 0);
                let end = relative(opt_number(&arg(&args, 2)), view.length, view.length);
                for i in start..end.max(start) {
                    view.set(i, value);
                }
                Value::Object(o)
            }))
        }),
        method("reverse", 0, |_, _, this, _| {
            NResult::from(bytes_receiver(&this, "reverse").map(|o| {
                let view = view_of(&o);
                let mut bytes = view.to_vec();
                bytes.reverse();
                for (i, b) in bytes.into_iter().enumerate() {
                    view.set(i, b);
                }
                Value::Object(o)
            }))
        }),
        method("sort", 1, |rt, site, this, args| {
            let target = match bytes_receiver(&this, "sort") {
                Ok(t) => t,
                Err(e) => return NResult::err(e),
            };
            let comparator = arg(&args, 0);
            // Without a comparator, typed arrays sort numerically rather than by string.
            if comparator.is_undefined() {
                let view = view_of(&target);
                let mut bytes = view.to_vec();
                bytes.sort_unstable();
                for (i, b) in bytes.into_iter().enumerate() {
                    view.set(i, b);
                }
                return NResult::ok(Value::Object(target));
            }
            let rt = rt.clone();
            NResult::future(async move {
                let items: Vec<Value> =
                    view_of(&target).to_vec().into_iter().map(|b| Value::Number(b as f64)).collect();
                let sorted = sort_array(&rt, items, &comparator, "Uint8Array.sort", site).await?;
                let view = view_of(&target);
                for (i, v) in sorted.iter().enumerate() {
                    view.set(i, to_uint8(coerce_to_number(v)));
                }
                Ok(Value::Object(target))
            })
        }),
        method("indexOf", 1, |_, _, this, args| {
            NResult::from(bytes_receiver(&this, "indexOf").map(|o| {
                let bytes = view_of(&o).to_vec();
                let search = search_value(&arg(&args, 0));
                Value::Number(index_of(&bytes, search, opt_number(&arg(&args, 1))).map(|i| i as f64).unwrap_or(-1.0))
            }))
        }),
        method("lastIndexOf", 1, |_, _, this, args| {
            NResult::from(bytes_receiver(&this, "lastIndexOf").map(|o| {
                let bytes = view_of(&o).to_vec();
                let search = search_value(&arg(&args, 0));
                let len = bytes.len() as f64;
                if bytes.is_empty() {
                    return Value::Number(-1.0);
                }
                let n = if args.len() < 2 { len - 1.0 } else { coerce_to_integer(&arg(&args, 1)) };
                if n == f64::NEG_INFINITY {
                    return Value::Number(-1.0);
                }
                let mut k = if n >= 0.0 { n.min(len - 1.0) } else { len + n };
                while k >= 0.0 {
                    if bytes[k as usize] as f64 == search {
                        return Value::Number(k);
                    }
                    k -= 1.0;
                }
                Value::Number(-1.0)
            }))
        }),
        method("includes", 1, |_, _, this, args| {
            NResult::from(bytes_receiver(&this, "includes").map(|o| {
                let bytes = view_of(&o).to_vec();
                let search = search_value(&arg(&args, 0));
                Value::Bool(index_of(&bytes, search, opt_number(&arg(&args, 1))).is_some())
            }))
        }),
        method("join", 1, |_, _, this, args| {
            NResult::from(bytes_receiver(&this, "join").and_then(|o| {
                let separator = match arg(&args, 0) {
                    Value::Undefined => JsStr::from(","),
                    other => coerce_to_string(&other),
                };
                Ok(Value::String(join(&view_of(&o).to_vec(), &separator)?))
            }))
        }),
        method("toString", 0, |_, _, this, _| {
            NResult::from(
                bytes_receiver(&this, "toString")
                    .and_then(|o| Ok(Value::String(join(&view_of(&o).to_vec(), &JsStr::from(","))?))),
            )
        }),
        method("toBase64", 0, |_, _, this, args| {
            NResult::from(bytes_receiver(&this, "toBase64").and_then(|o| {
                let label = "Uint8Array.prototype.toBase64";
                let options = base64_options(&arg(&args, 0), label)?;
                let url = base64_option(&options, "alphabet", &["base64", "base64url"], label)? == 1;
                let omit = match &options {
                    Some(o) => get(o, &PropKey::str("omitPadding"))?.truthy(),
                    None => false,
                };
                Ok(Value::from(to_base64(&view_of(&o).to_vec(), url, omit)))
            }))
        }),
        method("toHex", 0, |_, _, this, _| {
            NResult::from(bytes_receiver(&this, "toHex").map(|o| Value::from(to_hex(&view_of(&o).to_vec()))))
        }),
        method("keys", 0, |rt, _, this, _| {
            NResult::from(
                bytes_receiver(&this, "keys").map(|o| iterator_object(rt, ArrayCursor::new(rt, o, Mode::Keys))),
            )
        }),
        method("values", 0, |rt, _, this, _| {
            NResult::from(
                bytes_receiver(&this, "values").map(|o| iterator_object(rt, ArrayCursor::new(rt, o, Mode::Values))),
            )
        }),
        method("entries", 0, |rt, _, this, _| {
            NResult::from(
                bytes_receiver(&this, "entries").map(|o| iterator_object(rt, ArrayCursor::new(rt, o, Mode::Entries))),
            )
        }),
    ];
    table.extend(callback_methods());
    methods(&fp, &proto, table);
    let values_fn = get(&proto, &PropKey::str("values")).unwrap_or_default();
    define(&proto, PropKey::Sym(Sym::Iterator), values_fn, HIDDEN);
    Value::Object(ctor)
}

/// TypedArray `indexOf`/`includes` start resolution; bytes never hold NaN, so both reduce to a strict search.
fn index_of(bytes: &[u8], search: f64, from: Option<f64>) -> Option<usize> {
    let len = bytes.len() as f64;
    if bytes.is_empty() {
        return None;
    }
    let n = from.unwrap_or(0.0);
    if n == f64::INFINITY {
        return None;
    }
    let k = if n >= 0.0 { n } else { (len + n).max(0.0) };
    (k as usize..bytes.len()).find(|&i| bytes[i] as f64 == search)
}

// ---------------------------------------------------------------------------------------------------------------
// TextEncoder / TextDecoder

pub fn text_encoder_global(rt: &Rt) -> Value {
    let proto = rt.builtins.text_encoder.clone();
    let fp = rt.builtins.function.clone();
    let encoding: Getter = Rc::new(|_| Ok(Value::str("utf-8")));
    define_accessor(&proto, "encoding", Some(encoding), None);
    methods(
        &fp,
        &proto,
        vec![method("encode", 0, |rt, _, _, args| {
            let input = match arg(&args, 0) {
                // Only undefined is empty; null encodes as "null" (#57639).
                Value::Undefined => JsStr::empty(),
                other => coerce_to_string(&other),
            };
            let bytes = String::from_utf16_lossy(input.units()).into_bytes();
            NResult::ok(Value::Object(bytes_obj(&rt.builtins.uint8_array, bytes)))
        })],
    );
    let construct_proto = proto.clone();
    Value::Object(constructor(
        &fp,
        &proto,
        "TextEncoder",
        0,
        NativeFn {
            call: requires_new("TextEncoder"),
            construct: Some(Rc::new(move |_, _, _, new_target| {
                NResult::ok(Value::Object(Obj::ordinary(&prototype_from(&new_target, &construct_proto))))
            })),
            callback: true,
        },
    ))
}

/// A `TextDecoder`: the options it was built with (always UTF-8) and its stream state, the bytes of an unfinished
/// character (`queued`) and whether a stream already produced text (`seen`, so only its first BOM is stripped).
struct Decoder {
    fatal: bool,
    ignore_bom: bool,
    queued: Vec<u8>,
    seen: bool,
}

/// Where a streamed chunk stops being decodable (`streamBoundary`): replacement mode only needs to find an
/// incomplete suffix (at most three bytes); fatal mode also reports the first error, with the position after it.
fn stream_boundary(bytes: &[u8], fatal: bool) -> (usize, bool) {
    let mut next = if fatal { 0 } else { bytes.len().saturating_sub(3) };
    while next < bytes.len() {
        let start = next;
        let lead = bytes[next];
        next += 1;
        if lead < 0x80 {
            continue;
        }
        if !(0xC2..=0xF4).contains(&lead) {
            if fatal {
                return (next, true);
            }
            continue;
        }
        let width = if lead < 0xE0 { 2 } else if lead < 0xF0 { 3 } else { 4 };
        let mut lower = match lead {
            0xE0 => 0xA0,
            0xF0 => 0x90,
            _ => 0x80,
        };
        let mut upper = match lead {
            0xED => 0x9F,
            0xF4 => 0x8F,
            _ => 0xBF,
        };
        while next < start + width {
            if next == bytes.len() {
                return (start, false);
            }
            let byte = bytes[next];
            // A bad continuation is reprocessed as a leading byte, even after a fatal error.
            if byte < lower || byte > upper {
                if fatal {
                    return (next, true);
                }
                break;
            }
            next += 1;
            lower = 0x80;
            upper = 0xBF;
        }
    }
    (next, false)
}

impl Decoder {
    fn decode(&mut self, bytes: &[u8], stream: bool) -> JsResult<String> {
        let mut input: Vec<u8> = if !self.queued.is_empty() && !bytes.is_empty() {
            check_array_length((self.queued.len() + bytes.len()) as f64)?;
            let mut joined = std::mem::take(&mut self.queued);
            joined.extend_from_slice(bytes);
            joined
        } else if bytes.is_empty() {
            std::mem::take(&mut self.queued)
        } else {
            bytes.to_vec()
        };
        let (end, error) = if stream { stream_boundary(&input, self.fatal) } else { (input.len(), false) };
        self.queued = if end < input.len() { input.split_off(end) } else { Vec::new() };
        let invalid = || type_error("TextDecoder.decode: the input is not valid utf-8.");
        if error {
            return Err(invalid());
        }
        let strip_bom = !self.ignore_bom && !self.seen;
        if !stream {
            self.seen = false;
        }
        let text = if self.fatal {
            String::from_utf8(input).map_err(|_| invalid())?
        } else {
            String::from_utf8_lossy(&input).into_owned()
        };
        if stream && !text.is_empty() {
            self.seen = true;
        }
        Ok(match text.strip_prefix('\u{FEFF}') {
            Some(rest) if strip_bom => rest.to_string(),
            _ => text,
        })
    }
}

thread_local! {
    /// TextDecoder state by object. A TextDecoder is an ordinary object upstream (`TextDecoderObj extends Obj`), so
    /// it has no `ObjKind`; the weak handle keeps the address from being reused while the entry exists.
    static DECODERS: RefCell<HashMap<usize, (Weak<ObjData>, Rc<RefCell<Decoder>>)>> = RefCell::new(HashMap::new());
}

fn register_decoder(o: &Obj, decoder: Decoder) {
    DECODERS.with(|d| {
        let mut d = d.borrow_mut();
        if d.len() >= 1024 && d.len().is_power_of_two() {
            d.retain(|_, (w, _)| w.strong_count() > 0);
        }
        d.insert(o.addr(), (Rc::downgrade(&o.0), Rc::new(RefCell::new(decoder))));
    });
}

fn decoder_of(o: &Obj) -> Option<Rc<RefCell<Decoder>>> {
    DECODERS.with(|d| {
        d.borrow().get(&o.addr()).and_then(|(w, dec)| (w.as_ptr() == Rc::as_ptr(&o.0)).then(|| dec.clone()))
    })
}

fn decoder_receiver(this: &Value, name: &str) -> JsResult<Rc<RefCell<Decoder>>> {
    let o = receiver(this, &format!("TextDecoder.prototype.{name}"), |o| decoder_of(o).is_some())?;
    Ok(decoder_of(&o).expect("receiver checked"))
}

// The WHATWG labels for UTF-8, the only encoding CodeMode decodes.
const UTF8_LABELS: [&str; 6] =
    ["unicode-1-1-utf-8", "unicode11utf8", "unicode20utf8", "utf-8", "utf8", "x-unicode20utf8"];

pub fn text_decoder_global(rt: &Rt) -> Value {
    let proto = rt.builtins.text_decoder.clone();
    let fp = rt.builtins.function.clone();
    let encoding: Getter = Rc::new(|this| decoder_receiver(this, "encoding").map(|_| Value::str("utf-8")));
    let fatal: Getter = Rc::new(|this| decoder_receiver(this, "fatal").map(|d| Value::Bool(d.borrow().fatal)));
    let ignore_bom: Getter =
        Rc::new(|this| decoder_receiver(this, "ignoreBOM").map(|d| Value::Bool(d.borrow().ignore_bom)));
    define_accessor(&proto, "encoding", Some(encoding), None);
    define_accessor(&proto, "fatal", Some(fatal), None);
    define_accessor(&proto, "ignoreBOM", Some(ignore_bom), None);
    methods(
        &fp,
        &proto,
        vec![method("decode", 0, |_, _, this, args| {
            NResult::from((|| {
                let decoder = decoder_receiver(&this, "decode")?;
                let input = arg(&args, 0);
                let bytes = match &input {
                    Value::Undefined => Vec::new(),
                    Value::Object(o) if o.bytes().is_some() => view_of(o).to_vec(),
                    other => {
                        return Err(type_error(format!(
                            "TextDecoder.decode expects a Uint8Array, received {}.",
                            describe_value(other)
                        )));
                    }
                };
                let options = arg(&args, 1);
                if !options.is_undefined() && options.js_typeof() != "object" {
                    return Err(type_error("TextDecoder.decode expects an options object."));
                }
                let stream = match &options {
                    Value::Object(o) => get(o, &PropKey::str("stream"))?.truthy(),
                    _ => false,
                };
                let text = decoder.borrow_mut().decode(&bytes, stream)?;
                Ok(Value::from(text))
            })())
        })],
    );
    let construct_proto = proto.clone();
    Value::Object(constructor(
        &fp,
        &proto,
        "TextDecoder",
        0,
        NativeFn {
            call: requires_new("TextDecoder"),
            construct: Some(Rc::new(move |_, _, args, new_target| {
                NResult::from((|| {
                    let label = match arg(&args, 0) {
                        Value::Undefined => "utf-8".to_string(),
                        other => {
                            let text = coerce_to_string(&other);
                            String::from_utf16_lossy(crate::number::trim_units(text.units())).to_lowercase()
                        }
                    };
                    if !UTF8_LABELS.contains(&label.as_str()) {
                        return Err(range_error(format!("The \"{label}\" encoding is not supported; only UTF-8 is.")));
                    }
                    let options = match arg(&args, 1) {
                        Value::Object(o) => Some(o),
                        _ => None,
                    };
                    let flag = |name: &str| -> JsResult<bool> {
                        match &options {
                            Some(o) => Ok(get(o, &PropKey::str(name))?.truthy()),
                            None => Ok(false),
                        }
                    };
                    let decoder =
                        Decoder { fatal: flag("fatal")?, ignore_bom: flag("ignoreBOM")?, queued: Vec::new(), seen: false };
                    let obj = Obj::ordinary(&prototype_from(&new_target, &construct_proto));
                    register_decoder(&obj, decoder);
                    Ok(Value::Object(obj))
                })())
            })),
            callback: true,
        },
    ))
}
