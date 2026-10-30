//! The data boundary: what `JSON.stringify` would serialize for a program value, and host JSON copied back in.
//! Port of `data.ts`, plus JS-compatible JSON text rendering.

use std::collections::HashSet;

use serde_json::{Map, Number};

use crate::interp::callback::Callback;
use crate::interp::errors::{invalid_data, range_error, type_error, JsResult};
use crate::interp::eval::call_value;
use crate::interp::limits::MAX_VALUE_DEPTH;
use crate::interp::{Rt, Site};
use crate::object::{define_data, get, keys, record, Obj, ObjKind};
use crate::value::{JsStr, PropKey, Value};

pub type Json = serde_json::Value;

/// A JSON number from an f64, as an integer when exact (so hosts see `5`, not `5.0`).
pub fn json_number(n: f64) -> Json {
    if !n.is_finite() {
        return Json::Null;
    }
    if n == n.trunc() && n.abs() < 9_007_199_254_740_992.0 {
        return Json::Number(Number::from(n as i64));
    }
    Number::from_f64(n).map(Json::Number).unwrap_or(Json::Null)
}

/// What `JSON.stringify` would serialize for a program value, as host JSON.
pub async fn to_json(rt: &Rt, value: Value, replacer: Option<&Callback>, site: Site) -> JsResult<Option<Json>> {
    walk(rt, value, replacer, false, site).await
}

/// The host boundary: `to_json`, plus what a program most likely meant when a value cannot be JSON.
pub async fn to_boundary(rt: &Rt, value: Value, site: Site) -> JsResult<Option<Json>> {
    walk(rt, value, None, true, site).await
}

struct Walk<'a> {
    rt: &'a Rt,
    replacer: Option<&'a Callback>,
    boundary: bool,
    site: Site,
    stack: HashSet<usize>,
}

async fn walk(rt: &Rt, value: Value, replacer: Option<&Callback>, boundary: bool, site: Site) -> JsResult<Option<Json>> {
    let holder = record(&rt.builtins.object, vec![("", value)]);
    let mut w = Walk { rt, replacer, boundary, site, stack: HashSet::new() };
    w.visit(holder, PropKey::str(""), 0).await
}

impl Walk<'_> {
    fn visit(&mut self, holder: Obj, key: PropKey, depth: usize) -> futures::future::LocalBoxFuture<'_, JsResult<Option<Json>>> {
        Box::pin(async move {
            // Shared references expand into a tree, so the work is not bounded by the value's size or depth.
            self.rt.interrupt.check()?;
            if depth > MAX_VALUE_DEPTH {
                return Err(range_error(format!("Value exceeds the maximum depth of {MAX_VALUE_DEPTH}.")));
            }
            let raw = get(&holder, &key)?;
            if let Value::Object(o) = &raw
                && o.is_promise()
                && !self.boundary
            {
                return Err(invalid_data(
                    "JSON.stringify received an un-awaited Promise; await it first - e.g. `const result = await tools.ns.tool(...)`.",
                ));
            }
            let settled = match &raw {
                Value::Object(o) if o.is_promise() => crate::interp::runner::await_value(self.rt, raw.clone(), None).await?,
                _ => raw,
            };
            let key_value = key.to_value();
            let own = match &settled {
                Value::Object(o) => {
                    let to_json = get(o, &PropKey::str("toJSON"))?;
                    if matches!(&to_json, Value::Object(f) if f.is_callable()) {
                        call_value(self.rt, &to_json, settled.clone(), vec![key_value.clone()], self.site).await?
                    } else {
                        settled
                    }
                }
                _ => settled,
            };
            let value = match self.replacer {
                Some(r) => r(self.rt, vec![key_value, own], Value::Object(holder.clone()), self.site).await?,
                None => own,
            };
            if value.is_undefined() || value.js_typeof() == "function" {
                return Ok(None);
            }
            let o = match &value {
                Value::Number(n) => return Ok(Some(json_number(*n))),
                Value::Null => return Ok(Some(Json::Null)),
                Value::Bool(b) => return Ok(Some(Json::Bool(*b))),
                Value::String(s) => {
                    return Ok(Some(Json::String(if self.boundary { s.to_rust() } else { marked_string(s.units()) })));
                }
                Value::Object(o) => o.clone(),
                _ => return Ok(Some(Json::Object(Map::new()))),
            };
            if self.boundary && o.bytes().is_some() {
                return Err(invalid_data(
                    "A Uint8Array cannot cross to the host; pass text instead, e.g. `new TextDecoder().decode(bytes)` or `bytes.toBase64()`.",
                ));
            }
            if self.boundary
                && let ObjKind::URLSearchParams(p) = o.kind()
            {
                return Ok(Some(Json::String(p.serialize())));
            }
            if let ObjKind::Headers(h) = o.kind() {
                let mut map = Map::new();
                for (k, v) in h.borrow().combined() {
                    map.insert(k, Json::String(v));
                }
                return Ok(Some(Json::Object(map)));
            }
            let target = match o.kind() {
                ObjKind::Set(s) if self.boundary => {
                    let items: Vec<Value> = s.borrow().iter().map(|(k, _)| k.clone()).collect();
                    Obj::array(&self.rt.builtins.array, items)
                }
                _ => o.clone(),
            };
            if !self.stack.insert(target.addr()) {
                return Err(type_error("Converting circular structure to JSON."));
            }
            if target.is_array() {
                let len = target.array_len();
                let mut items = Vec::with_capacity(len);
                for i in 0..len {
                    items.push(self.visit(target.clone(), PropKey::str(&i.to_string()), depth + 1).await?.unwrap_or(Json::Null));
                }
                self.stack.remove(&target.addr());
                return Ok(Some(Json::Array(items)));
            }
            let mut copied: Vec<(String, Json)> = Vec::new();
            let mut put = |name: String, item: Option<Json>| {
                if let Some(item) = item {
                    if let Some(existing) = copied.iter_mut().find(|(k, _)| *k == name) {
                        existing.1 = item;
                    } else {
                        copied.push((name, item));
                    }
                }
            };
            // Errors serialize as { name, message, ...own }.
            if target.is_error() {
                let n = self.visit(target.clone(), PropKey::str("name"), depth + 1).await?;
                put("name".into(), n);
                let m = self.visit(target.clone(), PropKey::str("message"), depth + 1).await?;
                put("message".into(), m);
            }
            for name in keys(&target) {
                if self.boundary && name.eq_str("__proto__") {
                    continue;
                }
                let item = self.visit(target.clone(), PropKey::Str(name.clone()), depth + 1).await?;
                put(if self.boundary { name.to_rust() } else { marked_string(name.units()) }, item);
            }
            self.stack.remove(&target.addr());
            Ok(Some(Json::Object(js_ordered(copied))))
        })
    }
}

/// Orders keys as a JS object would: integer-like keys ascending first, then insertion order.
pub fn js_ordered(entries: Vec<(String, Json)>) -> Map<String, Json> {
    let mut ints: Vec<(u64, String, Json)> = Vec::new();
    let mut rest: Vec<(String, Json)> = Vec::new();
    for (k, v) in entries {
        match crate::object::parse_array_index(&JsStr::from(k.as_str())) {
            Some(i) => ints.push((i as u64, k, v)),
            None => rest.push((k, v)),
        }
    }
    ints.sort_by_key(|(i, _, _)| *i);
    let mut map = Map::new();
    for (_, k, v) in ints {
        map.insert(k, v);
    }
    for (k, v) in rest {
        map.insert(k, v);
    }
    map
}

/// Host JSON as program values: objects and arrays are copied, primitives pass through.
pub fn from_json(rt: &Rt, value: Option<&Json>) -> Value {
    match value {
        None => Value::Undefined,
        Some(v) => from_json_value(rt, v),
    }
}

pub fn from_json_value(rt: &Rt, value: &Json) -> Value {
    match value {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Bool(*b),
        Json::Number(n) => Value::Number(n.as_f64().unwrap_or(f64::NAN)),
        Json::String(s) => Value::str(s),
        Json::Array(items) => Value::Object(Obj::array(&rt.builtins.array, items.iter().map(|i| from_json_value(rt, i)).collect())),
        Json::Object(map) => {
            let copied = Obj::ordinary(&rt.builtins.object);
            for (k, v) in map {
                define_data(&copied, k.as_str(), from_json_value(rt, v));
            }
            Value::Object(copied)
        }
    }
}

/// `JSON.stringify` quoting of a UTF-16 string (well-formed: lone surrogates escape as `\udXXX`).
pub fn quote_units(units: &[u16], out: &mut String) {
    out.push('"');
    let mut i = 0;
    while i < units.len() {
        let c = units[i];
        match c {
            0x22 => out.push_str("\\\""),
            0x5C => out.push_str("\\\\"),
            0x08 => out.push_str("\\b"),
            0x0C => out.push_str("\\f"),
            0x0A => out.push_str("\\n"),
            0x0D => out.push_str("\\r"),
            0x09 => out.push_str("\\t"),
            c if c < 0x20 => out.push_str(&format!("\\u{c:04x}")),
            0xD800..=0xDBFF if i + 1 < units.len() && (0xDC00..=0xDFFF).contains(&units[i + 1]) => {
                let cp = 0x10000 + (((c as u32) - 0xD800) << 10) + ((units[i + 1] as u32) - 0xDC00);
                out.push(char::from_u32(cp).unwrap());
                i += 1;
            }
            0xD800..=0xDFFF => out.push_str(&format!("\\u{c:04x}")),
            c => out.push(char::from_u32(c as u32).unwrap()),
        }
        i += 1;
    }
    out.push('"');
}

const MARK_BASE: u32 = 0x10F800;

/// A UTF-16 string as Rust text for in-program JSON: lone surrogates become plane-16 private-use marker characters
/// (`U+10F800 + (unit - 0xD800)`) that [`quote`] turns back into `\udXXX` escapes, as `JSON.stringify` prints them.
pub fn marked_string(units: &[u16]) -> String {
    let mut out = String::with_capacity(units.len());
    for r in char::decode_utf16(units.iter().copied()) {
        match r {
            Ok(c) => out.push(c),
            Err(e) => out.push(char::from_u32(MARK_BASE + (e.unpaired_surrogate() as u32 - 0xD800)).unwrap()),
        }
    }
    out
}

pub fn quote(s: &str) -> String {
    let mut units: Vec<u16> = Vec::with_capacity(s.len());
    for c in s.chars() {
        let cp = c as u32;
        if cp >= MARK_BASE {
            units.push((0xD800 + (cp - MARK_BASE)) as u16);
        } else {
            let mut buf = [0u16; 2];
            units.extend_from_slice(c.encode_utf16(&mut buf));
        }
    }
    let mut out = String::new();
    quote_units(&units, &mut out);
    out
}

/// `JSON.stringify(value, null, indent)` over host JSON, with JS number formatting.
pub fn stringify(value: &Json, indent: &str) -> String {
    let mut out = String::new();
    write_json(value, indent, "", &mut out);
    out
}

fn write_json(value: &Json, indent: &str, current: &str, out: &mut String) {
    match value {
        Json::Null => out.push_str("null"),
        Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Json::Number(n) => out.push_str(&crate::number::to_string(n.as_f64().unwrap_or(0.0))),
        Json::String(s) => out.push_str(&quote(s)),
        Json::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            let inner = format!("{current}{indent}");
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                if !indent.is_empty() {
                    out.push('\n');
                    out.push_str(&inner);
                }
                write_json(item, indent, &inner, out);
            }
            if !indent.is_empty() {
                out.push('\n');
                out.push_str(current);
            }
            out.push(']');
        }
        Json::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            let inner = format!("{current}{indent}");
            for (i, (k, v)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                if !indent.is_empty() {
                    out.push('\n');
                    out.push_str(&inner);
                }
                out.push_str(&quote(k));
                out.push(':');
                if !indent.is_empty() {
                    out.push(' ');
                }
                write_json(v, indent, &inner, out);
            }
            if !indent.is_empty() {
                out.push('\n');
                out.push_str(current);
            }
            out.push('}');
        }
    }
}
