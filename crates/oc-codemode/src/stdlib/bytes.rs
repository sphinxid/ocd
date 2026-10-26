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
use crate::object::{BytesData, Getter, HIDDEN, Obj, ObjData, ObjKind, define, define_accessor, get, to_uint8};
use crate::stdlib::array::{Elements, sort_array};
use crate::value::{JsStr, PropKey, Sym, Value};

fn bytes_obj(proto: &Obj, bytes: Vec<u8>) -> Obj {
    Obj::new(Some(proto.clone()), ObjKind::Bytes(BytesData::new(bytes)))
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

/// Decodes a partial or full chunk of base64 digits, discarding leftover bits.
fn decode_chunk(chunk: &[u32], out: &mut Vec<u8>) {
    let mut n: u32 = 0;
    for (i, &d) in chunk.iter().enumerate() {
        n |= d << (18 - 6 * i);
    }
    let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
    out.extend_from_slice(&bytes[..chunk.len().saturating_sub(1)]);
}

/// `Uint8Array.fromBase64` with the default options (base64 alphabet, "loose" last-chunk handling).
fn from_base64(units: &[u16]) -> Option<Vec<u8>> {
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
                if chunk.len() == 1 {
                    return None;
                }
                decode_chunk(&chunk, &mut out);
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
                    return None;
                }
                if units[i] == b'=' as u16 {
                    i = skip(i + 1);
                }
            }
            if i < len {
                return None;
            }
            decode_chunk(&chunk, &mut out);
            return Some(out);
        }
        chunk.push(base64_digit(c)?);
        if chunk.len() == 4 {
            decode_chunk(&chunk, &mut out);
            chunk.clear();
        }
    }
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
            wrap: Rc::new(|rt, values| {
                let bytes = values.iter().map(|v| to_uint8(coerce_to_number(&v.clone().unwrap_or_default()))).collect();
                Value::Object(bytes_obj(&rt.builtins.uint8_array, bytes))
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
            let decoded = if name == "fromHex" { from_hex(text.units()) } else { from_base64(text.units()) };
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
                Value::Object(bytes_obj(&rt.builtins.uint8_array, slice))
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
                };
                Value::Object(Obj::new(Some(rt.builtins.uint8_array.clone()), ObjKind::Bytes(sub)))
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
        method("toBase64", 0, |_, _, this, _| {
            NResult::from(bytes_receiver(&this, "toBase64").map(|o| Value::from(encode_base64(&view_of(&o).to_vec()))))
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
                Value::Undefined | Value::Null => JsStr::empty(),
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

/// The options a `TextDecoder` was built with (always UTF-8).
#[derive(Clone, Copy)]
struct Decoder {
    fatal: bool,
    ignore_bom: bool,
}

thread_local! {
    /// TextDecoder state by object. A TextDecoder is an ordinary object upstream (`TextDecoderObj extends Obj`), so
    /// it has no `ObjKind`; the weak handle keeps the address from being reused while the entry exists.
    static DECODERS: RefCell<HashMap<usize, (Weak<ObjData>, Decoder)>> = RefCell::new(HashMap::new());
}

fn register_decoder(o: &Obj, decoder: Decoder) {
    DECODERS.with(|d| {
        let mut d = d.borrow_mut();
        if d.len() >= 1024 && d.len().is_power_of_two() {
            d.retain(|_, (w, _)| w.strong_count() > 0);
        }
        d.insert(o.addr(), (Rc::downgrade(&o.0), decoder));
    });
}

fn decoder_of(o: &Obj) -> Option<Decoder> {
    DECODERS.with(|d| d.borrow().get(&o.addr()).and_then(|(w, dec)| (w.as_ptr() == Rc::as_ptr(&o.0)).then_some(*dec)))
}

fn decoder_receiver(this: &Value, name: &str) -> JsResult<Decoder> {
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
    let fatal: Getter = Rc::new(|this| decoder_receiver(this, "fatal").map(|d| Value::Bool(d.fatal)));
    let ignore_bom: Getter = Rc::new(|this| decoder_receiver(this, "ignoreBOM").map(|d| Value::Bool(d.ignore_bom)));
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
                if input.is_undefined() {
                    return Ok(Value::str(""));
                }
                let bytes = match &input {
                    Value::Object(o) if o.bytes().is_some() => view_of(o).to_vec(),
                    other => {
                        return Err(type_error(format!(
                            "TextDecoder.decode expects a Uint8Array, received {}.",
                            describe_value(other)
                        )));
                    }
                };
                let body = if !decoder.ignore_bom && bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
                    &bytes[3..]
                } else {
                    &bytes[..]
                };
                let text = if decoder.fatal {
                    std::str::from_utf8(body)
                        .map_err(|_| type_error("TextDecoder.decode: the input is not valid utf-8."))?
                        .to_string()
                } else {
                    String::from_utf8_lossy(body).into_owned()
                };
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
                    let decoder = Decoder { fatal: flag("fatal")?, ignore_bom: flag("ignoreBOM")? };
                    let obj = Obj::ordinary(&prototype_from(&new_target, &construct_proto));
                    register_decoder(&obj, decoder);
                    Ok(Value::Object(obj))
                })())
            })),
            callback: true,
        },
    ))
}
