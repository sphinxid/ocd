//! `Headers`. Port of `stdlib/headers.ts`.
//!
//! Upstream wraps the host's (Bun's) `Headers`; this module reproduces its observable behavior: names are
//! validated as HTTP tokens and fold to lowercase, values are trimmed of HTTP whitespace and validated, repeated
//! names combine with `", "` (`"; "` for `cookie`), and `set-cookie` values are kept apart. Iteration is the sorted,
//! combined list followed by each `set-cookie` value, read live by index like Bun's iterator. Validation failures
//! carry Bun's messages.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;

use futures::future::LocalBoxFuture;

use crate::interp::callback::apply_collection_callback;
use crate::interp::coerce::coerce_to_string;
use crate::interp::errors::{JsResult, type_error};
use crate::interp::function::{
    NResult, NativeFn, arg, constructor, method, methods, prototype_from, receiver, requires_new,
};
use crate::interp::iteration::{Cursor, Mode, Step, iterator_object};
use crate::interp::references::is_runtime_reference;
use crate::interp::{Rt, Site};
use crate::object::{HIDDEN, Obj, ObjKind, define, entries, get};
use crate::value::{JsStr, PropKey, Sym, Value};

/// A header list: combined values by lowercase name (kept sorted), plus the `set-cookie` values in order.
#[derive(Default)]
pub struct HeadersData {
    map: BTreeMap<String, String>,
    set_cookies: Vec<String>,
}

impl HeadersData {
    /// `Object.fromEntries(headers)`: the iteration pairs with one `set-cookie` key holding the last cookie.
    pub fn combined(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self.map.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        if let Some(last) = self.set_cookies.last() {
            out.push(("set-cookie".into(), last.clone()));
        }
        out
    }

    /// The iteration pairs: sorted combined headers, then each `set-cookie` value.
    pub fn list(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self.map.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        out.extend(self.set_cookies.iter().map(|c| ("set-cookie".to_string(), c.clone())));
        out
    }

    fn entry_at(&self, index: usize) -> Option<(String, String)> {
        if index < self.map.len() {
            return self.map.iter().nth(index).map(|(k, v)| (k.clone(), v.clone()));
        }
        self.set_cookies.get(index - self.map.len()).map(|c| ("set-cookie".to_string(), c.clone()))
    }

    fn append(&mut self, name: String, value: String) {
        if name == "set-cookie" {
            self.set_cookies.push(value);
            return;
        }
        let separator = if name == "cookie" { "; " } else { ", " };
        match self.map.get_mut(&name) {
            Some(existing) => {
                existing.push_str(separator);
                existing.push_str(&value);
            }
            None => {
                self.map.insert(name, value);
            }
        }
    }

    fn set(&mut self, name: String, value: String) {
        if name == "set-cookie" {
            self.set_cookies = vec![value];
            return;
        }
        self.map.insert(name, value);
    }

    fn delete(&mut self, name: &str) {
        if name == "set-cookie" {
            self.set_cookies.clear();
        } else {
            self.map.remove(name);
        }
    }

    fn get(&self, name: &str) -> Option<String> {
        if name == "set-cookie" {
            return if self.set_cookies.is_empty() { None } else { Some(self.set_cookies.join(", ")) };
        }
        self.map.get(name).cloned()
    }

    fn has(&self, name: &str) -> bool {
        if name == "set-cookie" { !self.set_cookies.is_empty() } else { self.map.contains_key(name) }
    }
}

fn is_token_char(c: u16) -> bool {
    c < 128 && {
        let b = c as u8;
        b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
    }
}

fn is_http_whitespace(c: u16) -> bool {
    matches!(c, 0x09 | 0x0A | 0x0D | 0x20)
}

/// A validated header name, lowercased.
fn header_name(name: &JsStr, _method: &str) -> JsResult<String> {
    let units = name.units();
    if units.is_empty() || !units.iter().all(|&c| is_token_char(c)) {
        return Err(type_error(format!("Invalid header name: '{}'", name.to_rust())));
    }
    Ok(name.to_rust().to_ascii_lowercase())
}

/// A normalized, validated header value (trimmed of HTTP whitespace; no NUL, CR, LF, or code unit above 0xFF).
fn header_value(name: &JsStr, value: &JsStr) -> JsResult<String> {
    let units = value.units();
    let start = units.iter().position(|&c| !is_http_whitespace(c)).unwrap_or(units.len());
    let end = units.iter().rposition(|&c| !is_http_whitespace(c)).map(|i| i + 1).unwrap_or(start);
    let trimmed = &units[start..end.max(start)];
    let text = String::from_utf16_lossy(trimmed);
    if trimmed.iter().any(|&c| c == 0 || c == 0x0A || c == 0x0D || c > 0xFF) {
        return Err(type_error(format!("Header '{}' has invalid value", name.to_rust())));
    }
    Ok(text)
}

fn append_checked(data: &mut HeadersData, name: &JsStr, value: &JsStr) -> JsResult<()> {
    let lower = header_name(name, "append")?;
    let value = header_value(name, value)?;
    data.append(lower, value);
    Ok(())
}

fn headers_obj(proto: &Obj, data: HeadersData) -> Obj {
    Obj::new(Some(proto.clone()), ObjKind::Headers(RefCell::new(data)))
}

/// A Headers value for host pairs (extensions).
pub fn headers_value(rt: &Rt, list: Vec<(String, String)>) -> Value {
    let mut data = HeadersData::default();
    for (name, value) in list {
        let (name, value) = (JsStr::from(name), JsStr::from(value));
        if append_checked(&mut data, &name, &value).is_err() {
            continue;
        }
    }
    Value::Object(headers_obj(&rt.builtins.headers, data))
}

async fn construct_headers(rt: Rt, init: Value, proto: Obj, site: Site) -> JsResult<Value> {
    if let Value::Object(_) = &init
        && let Some(pairs) = crate::stdlib::url::read_pairs(&rt, &init, "new Headers(...)", site).await?
    {
        let mut data = HeadersData::default();
        for (name, value) in &pairs {
            append_checked(&mut data, name, value)?;
        }
        return Ok(Value::Object(headers_obj(&proto, data)));
    }
    let o = match &init {
        Value::Object(o) if !is_runtime_reference(&init) => o.clone(),
        _ => {
            return Err(type_error(
                "new Headers(...) expects a record of names to values, iterable [name, value] pairs, or Headers.",
            ));
        }
    };
    let mut data = HeadersData::default();
    for (name, value) in entries(&o)? {
        append_checked(&mut data, &name, &coerce_to_string(&value))?;
    }
    Ok(Value::Object(headers_obj(&proto, data)))
}

fn headers_receiver(this: &Value, name: &str) -> JsResult<Obj> {
    receiver(this, &format!("Headers.prototype.{name}"), |o| matches!(o.kind(), ObjKind::Headers(_)))
}

fn data_of(o: &Obj) -> &RefCell<HeadersData> {
    match o.kind() {
        ObjKind::Headers(h) => h,
        _ => unreachable!("receiver checked"),
    }
}

fn require_args(name: &str, args: &[Value], count: usize) -> JsResult<()> {
    if args.len() < count {
        return Err(type_error(format!(
            "Headers.{name} requires {count} argument{}.",
            if count == 1 { "" } else { "s" }
        )));
    }
    Ok(())
}

fn text_arg(args: &[Value], index: usize) -> JsStr {
    coerce_to_string(&arg(args, index))
}

/// Bun's live Headers iterator: an index into the current sorted list, finishing for good once exhausted.
struct HeadersCursor {
    target: RefCell<Option<Obj>>,
    index: Cell<usize>,
    mode: Mode,
    array_proto: Obj,
}

impl Cursor for HeadersCursor {
    fn next(&self) -> LocalBoxFuture<'static, JsResult<Step>> {
        let value = (|| {
            let target = self.target.borrow().clone()?;
            let i = self.index.get();
            let entry = data_of(&target).borrow().entry_at(i);
            let Some((name, value)) = entry else {
                *self.target.borrow_mut() = None;
                return None;
            };
            self.index.set(i + 1);
            Some(match self.mode {
                Mode::Keys => Value::from(name),
                Mode::Values => Value::from(value),
                Mode::Entries => {
                    Value::Object(Obj::array(&self.array_proto, vec![Value::from(name), Value::from(value)]))
                }
            })
        })();
        let step = match value {
            Some(v) => Step::value(v),
            None => Step::done(),
        };
        Box::pin(std::future::ready(Ok(step)))
    }
    fn close(&self) -> LocalBoxFuture<'static, JsResult<()>> {
        Box::pin(std::future::ready(Ok(())))
    }
    fn teardown(&self) -> Box<dyn std::any::Any> {
        Box::new(self.target.borrow_mut().take())
    }
}

pub fn headers_cursor(rt: &Rt, o: Obj, mode: Mode) -> Rc<dyn Cursor> {
    Rc::new(HeadersCursor {
        target: RefCell::new(Some(o)),
        index: Cell::new(0),
        mode,
        array_proto: rt.builtins.array.clone(),
    })
}

pub fn headers_global(rt: &Rt) -> Value {
    let proto = rt.builtins.headers.clone();
    let fp = rt.builtins.function.clone();
    let construct_proto = proto.clone();
    let ctor = constructor(
        &fp,
        &proto,
        "Headers",
        0,
        NativeFn {
            call: requires_new("Headers"),
            construct: Some(Rc::new(move |rt, site, args, new_target| {
                let into = prototype_from(&new_target, &construct_proto);
                let init = arg(&args, 0);
                if init.is_undefined() {
                    return NResult::ok(Value::Object(headers_obj(&into, HeadersData::default())));
                }
                NResult::future(construct_headers(rt.clone(), init, into, site))
            })),
            callback: true,
        },
    );
    methods(
        &fp,
        &proto,
        vec![
            method("append", 2, |_, _, this, args| {
                NResult::from((|| {
                    require_args("append", &args, 2)?;
                    let target = headers_receiver(&this, "append")?;
                    let (name, value) = (text_arg(&args, 0), text_arg(&args, 1));
                    append_checked(&mut data_of(&target).borrow_mut(), &name, &value)?;
                    Ok(Value::Undefined)
                })())
            }),
            method("delete", 1, |_, _, this, args| {
                NResult::from((|| {
                    require_args("delete", &args, 1)?;
                    let target = headers_receiver(&this, "delete")?;
                    let name = header_name(&text_arg(&args, 0), "delete")?;
                    data_of(&target).borrow_mut().delete(&name);
                    Ok(Value::Undefined)
                })())
            }),
            method("get", 1, |_, _, this, args| {
                NResult::from((|| {
                    require_args("get", &args, 1)?;
                    let target = headers_receiver(&this, "get")?;
                    let name = header_name(&text_arg(&args, 0), "get")?;
                    Ok(data_of(&target).borrow().get(&name).map(Value::from).unwrap_or(Value::Null))
                })())
            }),
            method("getSetCookie", 0, |rt, _, this, _| {
                NResult::from(headers_receiver(&this, "getSetCookie").map(|target| {
                    let cookies: Vec<Value> =
                        data_of(&target).borrow().set_cookies.iter().map(|c| Value::from(c.clone())).collect();
                    Value::Object(Obj::array(&rt.builtins.array, cookies))
                }))
            }),
            method("has", 1, |_, _, this, args| {
                NResult::from((|| {
                    require_args("has", &args, 1)?;
                    let target = headers_receiver(&this, "has")?;
                    let name = header_name(&text_arg(&args, 0), "has")?;
                    Ok(Value::Bool(data_of(&target).borrow().has(&name)))
                })())
            }),
            method("set", 2, |_, _, this, args| {
                NResult::from((|| {
                    require_args("set", &args, 2)?;
                    let target = headers_receiver(&this, "set")?;
                    let (name, value) = (text_arg(&args, 0), text_arg(&args, 1));
                    let lower = header_name(&name, "set")?;
                    let value = header_value(&name, &value)?;
                    data_of(&target).borrow_mut().set(lower, value);
                    Ok(Value::Undefined)
                })())
            }),
            method("keys", 0, |rt, _, this, _| {
                NResult::from(
                    headers_receiver(&this, "keys").map(|o| iterator_object(rt, headers_cursor(rt, o, Mode::Keys))),
                )
            }),
            method("values", 0, |rt, _, this, _| {
                NResult::from(
                    headers_receiver(&this, "values").map(|o| iterator_object(rt, headers_cursor(rt, o, Mode::Values))),
                )
            }),
            method("entries", 0, |rt, _, this, _| {
                NResult::from(
                    headers_receiver(&this, "entries")
                        .map(|o| iterator_object(rt, headers_cursor(rt, o, Mode::Entries))),
                )
            }),
            method("forEach", 1, |rt, site, this, args| {
                let prepared = (|| {
                    require_args("forEach", &args, 1)?;
                    let target = headers_receiver(&this, "forEach")?;
                    let apply = apply_collection_callback(&arg(&args, 0), "Headers.forEach")?;
                    Ok((target, apply))
                })();
                let (target, apply) = match prepared {
                    Ok(v) => v,
                    Err(e) => return NResult::err(e),
                };
                let rt = rt.clone();
                let this_arg = arg(&args, 1);
                NResult::future(async move {
                    let snapshot = data_of(&target).borrow().list();
                    for (name, value) in snapshot {
                        let args = vec![Value::from(value), Value::from(name), Value::Object(target.clone())];
                        apply(&rt, args, this_arg.clone(), site).await?;
                    }
                    Ok(Value::Undefined)
                })
            }),
        ],
    );
    let entries_fn = get(&proto, &PropKey::str("entries")).unwrap_or_default();
    define(&proto, PropKey::Sym(Sym::Iterator), entries_fn, HIDDEN);
    Value::Object(ctor)
}
