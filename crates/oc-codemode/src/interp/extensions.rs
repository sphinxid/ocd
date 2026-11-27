//! Host-function globals and value conversion across the extension boundary. Port of
//! `interpreter/extensions.ts`.

use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;

use futures::future::BoxFuture;

use super::coerce::coerce_to_string;
use super::errors::{type_error, ErrorType, JsResult, Thrown};
use super::function::{native, NResult, NativeFn};
use super::globals::create_error_value;
use super::limits::MAX_VALUE_DEPTH;
use super::{describe_value, Rt, Site};
use crate::host::{CallResult, ExtensionInvocation, HostCall, HostFunction, HostKey, HostProperty, HostValue};
use crate::object::{define, define_accessor, define_data, get, has, keys, BytesData, Getter, MapData, Obj, ObjKind, HIDDEN};
use crate::value::{PropKey, Sym, Value};

const UNCROSSED: [&str; 4] = ["stack", "constructor", "toString", "__proto__"];

/// How the arguments of one host call crossed: as host copies, or not yet, because promises in them have not
/// fulfilled; the call is made again once they settle (upstream `HostCalls.call` and `$vmHostWait`).
pub enum Crossed {
    Ready(Vec<HostValue>),
    Waiting(Vec<Obj>),
}

/// A host call's arguments as host copies, each labelled `Argument <n> to <label>` in diagnostics (2.3.1,
/// `hostGlobals`' `toHost` under core's translation, #64487/#64513): a function anywhere is left out, as
/// `JSON.stringify` leaves it out (an object's key is skipped, an array's item is `null`, a Map's or Set's entry is
/// `undefined`, and an argument is `undefined`), and a promise is replaced by its value once fulfilled. A program's
/// `toJSON` is never run. With `back` (an extension function's call, which takes functions, #64472), a function that
/// was inside a host result crosses back as its own host function instead.
pub fn cross_arguments(values: &[Value], label: &str, back: bool) -> JsResult<Crossed> {
    let mut waiting = Vec::new();
    let mut args = Vec::with_capacity(values.len());
    let rt = if back { super::current_rt() } else { None };
    for (i, v) in values.iter().enumerate() {
        let copied = to_host_with(
            v,
            &format!("Argument {} to {label}", i + 1),
            0,
            &mut HashSet::new(),
            &mut waiting,
            rt.as_ref(),
        )?;
        args.push(copied.unwrap_or(HostValue::Undefined));
    }
    Ok(if waiting.is_empty() { Crossed::Ready(args) } else { Crossed::Waiting(waiting) })
}

/// A program value as a host copy; `None` when it is left out (a function). A promise that has not fulfilled goes to
/// `waiting` (its place holds `undefined` until the copy is made again).
pub fn to_host(
    value: &Value,
    label: &str,
    depth: usize,
    seen: &mut HashSet<usize>,
    waiting: &mut Vec<Obj>,
) -> JsResult<Option<HostValue>> {
    to_host_with(value, label, depth, seen, waiting, None)
}

// As `to_host`; with `back`, the run whose functions from host results cross back as themselves.
fn to_host_with(
    value: &Value,
    label: &str,
    depth: usize,
    seen: &mut HashSet<usize>,
    waiting: &mut Vec<Obj>,
    back: Option<&Rt>,
) -> JsResult<Option<HostValue>> {
    if depth > MAX_VALUE_DEPTH {
        return Err(type_error(format!("{label} exceeds the maximum value depth of {MAX_VALUE_DEPTH}.")));
    }
    if let (Some(rt), Value::Object(o)) = (back, value)
        && let Some((_, f)) = rt.hosted.borrow().get(&o.addr())
    {
        return Ok(Some(HostValue::Function(f.clone())));
    }
    let o = match value {
        Value::Undefined => return Ok(Some(HostValue::Undefined)),
        Value::Null => return Ok(Some(HostValue::Null)),
        Value::Bool(b) => return Ok(Some(HostValue::Bool(*b))),
        Value::Number(n) => return Ok(Some(HostValue::Number(*n))),
        Value::String(s) => return Ok(Some(HostValue::String(s.to_rust()))),
        // `typeof` "function": program and host functions, and paths into `tools`.
        Value::Object(o) if o.is_callable() => return Ok(None),
        Value::Tool(t) if !t.path().is_empty() => return Ok(None),
        Value::Object(o) if seen.contains(&o.addr()) => {
            return Err(type_error(format!("{label} contains a circular value.")));
        }
        Value::Object(o) if o.is_promise() => {
            let Some(result) = super::runner::fulfilled_value(o) else {
                waiting.push(o.clone());
                return Ok(Some(HostValue::Undefined));
            };
            seen.insert(o.addr());
            let copied = to_host_with(&result, label, depth + 1, seen, waiting, back);
            seen.remove(&o.addr());
            return copied;
        }
        Value::Object(o) if !o.is_opaque() => o.clone(),
        other => {
            return Err(type_error(format!(
                "{label} contains {}, which cannot be passed to the host.",
                describe_value(other)
            )));
        }
    };
    seen.insert(o.addr());
    let result = (|| -> JsResult<HostValue> {
        let mut next = |item: &Value| to_host_with(item, label, depth + 1, seen, waiting, back);
        Ok(match o.kind() {
            ObjKind::Error(..) => {
                let name = coerce_to_string(&get(&o, &PropKey::str("name"))?).to_rust();
                let message = match get(&o, &PropKey::str("message"))? {
                    Value::Undefined => String::new(),
                    m => coerce_to_string(&m).to_rust(),
                };
                let mut cause = None;
                let mut fields = Vec::new();
                let mut names: Vec<String> = vec!["cause".into()];
                for k in keys(&o) {
                    let k = k.to_rust();
                    if !names.contains(&k) {
                        names.push(k);
                    }
                }
                for key in names {
                    if UNCROSSED.contains(&key.as_str()) || key == "message" || !has(&o, &PropKey::str(&key)) {
                        continue;
                    }
                    let item = match next(&get(&o, &PropKey::str(&key))?) {
                        Ok(Some(v)) => v,
                        Ok(None) | Err(Thrown::Pending(_)) => continue,
                        Err(e) => return Err(e),
                    };
                    if key == "cause" {
                        cause = Some(Box::new(item));
                    } else {
                        fields.push((key, item));
                    }
                }
                HostValue::Error { name, message, cause, fields }
            }
            ObjKind::Array(_) => HostValue::Array(
                o.array_values()
                    .iter()
                    .map(|v| Ok(next(v)?.unwrap_or(HostValue::Null)))
                    .collect::<JsResult<_>>()?,
            ),
            ObjKind::Date(t) => HostValue::Date(t.get()),
            ObjKind::RegExp(r) => HostValue::RegExp { source: r.source(), flags: r.flags.clone() },
            ObjKind::URL(u) => HostValue::Url(u.href()),
            ObjKind::URLSearchParams(p) => HostValue::SearchParams(p.pairs()),
            ObjKind::Headers(h) => HostValue::Headers(h.borrow().list()),
            ObjKind::Bytes(b) if b.node => HostValue::Buffer(b.to_vec()),
            ObjKind::Bytes(b) => HostValue::Bytes(b.to_vec()),
            ObjKind::Map(m) => {
                let entries: Vec<(Value, Value)> = m.borrow().iter().cloned().collect();
                let mut out = Vec::with_capacity(entries.len());
                for (k, v) in &entries {
                    let k = next(k)?.unwrap_or(HostValue::Undefined);
                    out.push((k, next(v)?.unwrap_or(HostValue::Undefined)));
                }
                HostValue::Map(out)
            }
            ObjKind::Set(s) => {
                let items: Vec<Value> = s.borrow().iter().map(|(k, _)| k.clone()).collect();
                HostValue::Set(
                    items.iter().map(|v| Ok(next(v)?.unwrap_or(HostValue::Undefined))).collect::<JsResult<_>>()?,
                )
            }
            _ => {
                let mut fields = Vec::new();
                for key in keys(&o) {
                    if key.eq_str("__proto__") {
                        continue;
                    }
                    let item = get(&o, &PropKey::Str(key.clone()))?;
                    if let Some(item) = next(&item)? {
                        fields.push((key.to_rust(), item));
                    }
                }
                HostValue::Object(fields)
            }
        })
    })();
    seen.remove(&o.addr());
    result.map(Some)
}

/// A host value as program values.
pub fn from_host(rt: &Rt, value: &HostValue, label: &str, depth: usize) -> JsResult<Value> {
    from_host_with(rt, value, label, depth, false)
}

// As `from_host`; `installed` for a value among the globals, whose functions never cross back.
fn from_host_with(rt: &Rt, value: &HostValue, label: &str, depth: usize, installed: bool) -> JsResult<Value> {
    if depth > MAX_VALUE_DEPTH {
        return Err(type_error(format!("{label} exceeds the maximum value depth of {MAX_VALUE_DEPTH}.")));
    }
    let b = &rt.builtins;
    let next = |item: &HostValue, path: &str| from_host_with(rt, item, path, depth + 1, installed);
    Ok(match value {
        HostValue::Undefined => Value::Undefined,
        HostValue::Null => Value::Null,
        HostValue::Bool(v) => Value::Bool(*v),
        HostValue::Number(n) => Value::Number(*n),
        HostValue::String(s) => Value::str(s),
        HostValue::Function(f) => {
            let made = wrap(rt, f.clone(), label, None);
            if !installed && let Value::Object(o) = &made {
                rt.hosted.borrow_mut().insert(o.addr(), (o.clone(), f.clone()));
            }
            made
        }
        HostValue::Unrepresentable(description) => {
            return Err(type_error(format!("{label} produced {description}, which the program cannot hold.")));
        }
        HostValue::Date(t) => Value::Object(Obj::new(Some(b.date.clone()), ObjKind::Date(std::cell::Cell::new(*t)))),
        HostValue::RegExp { source, flags } => {
            Value::Object(crate::stdlib::regexp::construct_regexp_literal(rt, source, flags)?)
        }
        HostValue::Bytes(bytes) => Value::Object(Obj::new(Some(b.uint8_array.clone()), ObjKind::Bytes(BytesData::new(bytes.clone())))),
        HostValue::Buffer(bytes) => Value::Object(Obj::new(Some(b.buffer.clone()), ObjKind::Bytes(BytesData::node(bytes.clone())))),
        HostValue::Error { name, message, cause, fields } => {
            let t = ErrorType::from_name(name).unwrap_or(ErrorType::Error);
            let copied = create_error_value(b.error(t), Some(message));
            if let Some(c) = cause
                && let Ok(v) = next(c, &format!("{label}.cause"))
            {
                define(&copied, "cause", v, HIDDEN);
            }
            for (k, v) in fields {
                if UNCROSSED.contains(&k.as_str()) || matches!(v, HostValue::Function(_)) {
                    continue;
                }
                match next(v, &format!("{label}.{k}")) {
                    Ok(v) => define_data(&copied, k.as_str(), v),
                    Err(Thrown::Pending(_)) => continue,
                    Err(e) => return Err(e),
                }
            }
            Value::Object(copied)
        }
        HostValue::Url(href) => crate::stdlib::url::url_value(rt, href)?,
        HostValue::SearchParams(pairs) => crate::stdlib::url::search_params_value(rt, pairs.clone()),
        HostValue::Headers(list) => crate::stdlib::headers::headers_value(rt, list.clone()),
        HostValue::Map(entries) => {
            let mut data = MapData::default();
            for (k, v) in entries {
                data.set(next(k, label)?, next(v, label)?);
            }
            Value::Object(Obj::new(Some(b.map.clone()), ObjKind::Map(std::cell::RefCell::new(data))))
        }
        HostValue::Set(items) => {
            let mut data = MapData::default();
            for v in items {
                let v = next(v, label)?;
                data.set(v.clone(), v);
            }
            Value::Object(Obj::new(Some(b.set.clone()), ObjKind::Set(std::cell::RefCell::new(data))))
        }
        HostValue::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for (i, item) in items.iter().enumerate() {
                out.push(next(item, &format!("{label}[{i}]"))?);
            }
            Value::Object(Obj::array(&b.array, out))
        }
        HostValue::Object(fields) => {
            let copied = Obj::ordinary(&b.object);
            for (k, v) in fields {
                if k == "__proto__" {
                    continue;
                }
                define_data(&copied, k.as_str(), next(v, &format!("{label}.{k}"))?);
            }
            Value::Object(copied)
        }
        HostValue::Live(props) => {
            let copied = Obj::ordinary(&b.object);
            for (key, property) in props {
                let (key, path): (PropKey, String) = match key {
                    HostKey::String(k) => (PropKey::str(k), format!("{label}.{k}")),
                    HostKey::AsyncIterator => (PropKey::Sym(Sym::AsyncIterator), format!("{label}[Symbol.asyncIterator]")),
                };
                match property {
                    HostProperty::Value(v) => define_data(&copied, key, next(v, &path)?),
                    HostProperty::Getter(read) => {
                        let read = read.clone();
                        let getter: Getter = std::rc::Rc::new(move |_| from_host(&super::rt(), &read(), &path, 0));
                        define_accessor(&copied, key, Some(getter), None);
                    }
                }
            }
            Value::Object(copied)
        }
    })
}

struct ExtensionGuard {
    host: Arc<dyn crate::host::Host>,
    call: Option<ExtensionInvocation>,
}

impl Drop for ExtensionGuard {
    fn drop(&mut self) {
        if let Some(call) = self.call.take() {
            self.host.extension_after(&call, &CallResult::Interrupted);
        }
    }
}

/// A host function as a program function. Arguments cross in (see [`cross_arguments`]); for a global, the
/// `extension.before` hook runs before the host function is called, so a call it denies never reaches the host. The
/// host's result, or what it threw, crosses out so the program catches a copy, and `extension.after` sees how the call
/// ended. Functions inside results are part of a value's API and skip the hooks. A denied call fails the way the global
/// would: an async global rejects its promise, a sync one throws. An argument that can't cross rejects an async call's
/// promise and throws from a sync call; promises in the arguments are awaited first, and the call is then made again
/// (an async call's promise follows the one that call returns; a sync call waits where it is, as `await` would).
pub fn wrap(rt: &Rt, f: Arc<dyn HostFunction>, label: &str, extension: Option<String>) -> Value {
    let label: Rc<str> = Rc::from(label);
    let extension: Option<Rc<str>> = extension.map(|e| Rc::from(e.as_str()));
    let name = f.name().to_string();
    let length = f.length();
    // A constructible host function: `new` calls it as a plain call does (#64472).
    let construct: Option<super::function::NativeConstruct> = if f.constructible() {
        let (f, label, extension) = (f.clone(), label.clone(), extension.clone());
        Some(Rc::new(move |rt, site, values, _| call_host(rt, site, f.clone(), label.clone(), extension.clone(), values)))
    } else {
        None
    };
    Value::Object(native(
        &rt.builtins.function,
        &name,
        length,
        NativeFn {
            call: Rc::new(move |rt, site, _, values| call_host(rt, site, f.clone(), label.clone(), extension.clone(), values)),
            construct,
            callback: true,
        },
    ))
}

fn call_host(
    rt: &Rt,
    site: Site,
    f: Arc<dyn HostFunction>,
    label: Rc<str>,
    extension: Option<Rc<str>>,
    values: Vec<Value>,
) -> NResult {
    let crossed = cross_arguments(&values, &label, true);
    if f.asynchronous() {
        let promise = match rt.runner.new_promise(rt) {
            Ok(p) => p,
            Err(e) => return NResult::err(e),
        };
        let resolving = rt.runner.resolving(&promise, site.node);
        match crossed {
            Err(e) => resolving.reject(rt, e),
            Ok(Crossed::Waiting(waiting)) => {
                let task_rt = rt.clone();
                let task = async move {
                    let rt = task_rt;
                    for p in waiting {
                        if let Err(e) = super::runner::await_value(&rt, Value::Object(p), None).await {
                            return resolving.reject(&rt, e);
                        }
                    }
                    match call_host(&rt, site, f, label, extension, values).resolve().await {
                        Ok(v) => resolving.resolve(&rt, v),
                        Err(e) => resolving.reject(&rt, e),
                    }
                };
                rt.runner.spawn_async(Box::pin(task), &promise);
            }
            Ok(Crossed::Ready(args)) => {
                let hooked = hooks(rt, &label, &extension, &args);
                let work: BoxFuture<'static, Result<HostValue, HostValue>> = Box::pin(async move {
                    hooked.before().await?;
                    let called = f.call(args);
                    hooked.settle(called).await
                });
                settle_later(rt, work, label.to_string(), resolving);
            }
        }
        return NResult::ok(Value::Object(promise));
    }
    let args = match crossed {
        Err(e) => return NResult::err(e),
        Ok(Crossed::Waiting(waiting)) => {
            let rt = rt.clone();
            return NResult::future(async move {
                for p in waiting {
                    super::runner::await_value(&rt, Value::Object(p), None).await?;
                }
                call_host(&rt, site, f, label, extension, values).resolve().await
            });
        }
        Ok(Crossed::Ready(args)) => args,
    };
    let hooked = hooks(rt, &label, &extension, &args);
    if let Err(reason) = hooked.blocking_before(rt) {
        return NResult::err(reason);
    }
    match f.call(args) {
        HostCall::Pending(work) => {
            let work: BoxFuture<'static, Result<HostValue, HostValue>> =
                Box::pin(async move { hooked.settle(HostCall::Pending(work)).await });
            NResult::Ready(pending(rt, work, label.to_string(), site.node))
        }
        // A value now, or host work the job waits for (#64363).
        ready => {
            let settled = rt.runner.block_on(rt, hooked.settle(ready));
            NResult::Ready(match settled {
                Err(e) => Err(e),
                Ok(Ok(v)) => from_host(rt, &v, &label, 0),
                Ok(Err(reason)) => Err(cross(rt, &reason, &label)),
            })
        }
    }
}

fn hooks(rt: &Rt, label: &str, extension: &Option<Rc<str>>, args: &[HostValue]) -> Hooked {
    let invocation = extension.as_ref().map(|extension| ExtensionInvocation {
        id: rt.tools.next_call_id(),
        extension: extension.to_string(),
        name: label.to_string(),
        args: args.to_vec(),
    });
    Hooked { host: rt.tools.host.clone(), invocation, label: label.to_string() }
}

/// What the host threw, as the program catches it.
fn cross(rt: &Rt, reason: &HostValue, label: &str) -> Thrown {
    match from_host(rt, reason, label, 0) {
        Ok(v) => Thrown::Value(v),
        Err(e) => e,
    }
}

/// The hooks around one call (none for functions inside results).
struct Hooked {
    host: Arc<dyn crate::host::Host>,
    invocation: Option<ExtensionInvocation>,
    label: String,
}

impl Hooked {
    async fn before(&self) -> Result<(), HostValue> {
        match &self.invocation {
            Some(call) => self.host.extension_before(call).await,
            None => Ok(()),
        }
    }

    // The `before` hook of a synchronous global holds the current job.
    fn blocking_before(&self, rt: &Rt) -> JsResult<()> {
        if self.invocation.is_none() {
            return Ok(());
        }
        match rt.runner.block_on(rt, self.before())? {
            Ok(()) => Ok(()),
            Err(reason) => Err(cross(rt, &reason, &self.label)),
        }
    }

    // The host's outcome, with `extension.after` observing how the call ended.
    async fn settle(self, called: HostCall) -> Result<HostValue, HostValue> {
        let mut guard = ExtensionGuard { host: self.host.clone(), call: self.invocation };
        let result = match called {
            HostCall::Ready(r) => r,
            HostCall::Pending(work) | HostCall::Block(work) => work.await,
        };
        if let Some(call) = guard.call.take() {
            let observed = match &result {
                Ok(_) => CallResult::Success(None),
                Err(e) => CallResult::Failure(format!("{e:?}")),
            };
            self.host.extension_after(&call, &observed);
        }
        result
    }
}

// An asynchronous host call: the program holds a promise that settles when the host's work does.
fn pending(rt: &Rt, work: BoxFuture<'static, Result<HostValue, HostValue>>, label: String, site: Option<crate::ast::Loc>) -> JsResult<Value> {
    let promise = rt.runner.new_promise(rt)?;
    let resolving = rt.runner.resolving(&promise, site);
    settle_later(rt, work, label, resolving);
    Ok(Value::Object(promise))
}

// Settles `resolving` with the host's work, in a later job, like I/O.
fn settle_later(
    rt: &Rt,
    work: BoxFuture<'static, Result<HostValue, HostValue>>,
    label: String,
    resolving: super::runner::Resolving,
) {
    rt.runner.host(Box::pin(async move {
        let outcome = work.await;
        Box::new(move || {
            let rt = super::rt();
            match outcome {
                Ok(v) => match from_host(&rt, &v, &label, 0) {
                    Ok(value) => resolving.resolve(&rt, value),
                    Err(e) => resolving.reject(&rt, e),
                },
                Err(reason) => resolving.reject(&rt, cross(&rt, &reason, &label)),
            }
        }) as Box<dyn FnOnce()>
    }));
}


/// The global bindings of one run's extensions.
pub fn extension_globals(rt: &Rt) -> JsResult<Vec<(String, Value)>> {
    let mut out = Vec::new();
    for extension in &rt.extensions {
        for (name, value) in extension.values() {
            let value = extension_value(rt, extension.name(), &name, &value)?;
            out.push((name, value));
        }
    }
    Ok(out)
}

/// One extension global as a program value (upstream `extensionValue`): a function fires the extension hooks around
/// its calls, at any depth of the global's plain objects, named by its path (`fs.readFile`); anything else is copied
/// in. Nothing in a global crosses back to the host as itself.
fn extension_value(rt: &Rt, extension: &str, label: &str, value: &HostValue) -> JsResult<Value> {
    match value {
        HostValue::Function(f) => Ok(wrap(rt, f.clone(), label, Some(extension.to_string()))),
        HostValue::Object(fields) => {
            let copied = Obj::ordinary(&rt.builtins.object);
            for (key, item) in fields {
                if key == "__proto__" {
                    continue;
                }
                define_data(&copied, key.as_str(), extension_value(rt, extension, &format!("{label}.{key}"), item)?);
            }
            Ok(Value::Object(copied))
        }
        other => from_host_with(rt, other, label, 0, true),
    }
}

/// Whether a host value can be an extension global: a function or an object.
pub fn is_global_value(value: &HostValue) -> bool {
    match value {
        HostValue::Undefined | HostValue::Null | HostValue::Bool(_) | HostValue::Number(_) | HostValue::String(_) => false,
        HostValue::Unrepresentable(what) => what != "a symbol" && what != "a bigint",
        _ => true,
    }
}
