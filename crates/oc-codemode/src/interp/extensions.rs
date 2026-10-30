//! Host-function globals and value conversion across the extension boundary. Port of
//! `interpreter/extensions.ts`.

use std::collections::HashSet;
use std::sync::Arc;

use futures::future::BoxFuture;

use super::coerce::coerce_to_string;
use super::errors::{type_error, ErrorType, JsResult, Thrown};
use super::function::{native, NResult, NativeFn};
use super::globals::create_error_value;
use super::limits::MAX_VALUE_DEPTH;
use super::{describe_value, Rt};
use crate::host::{CallResult, ExtensionInvocation, HostCall, HostFunction, HostValue};
use crate::object::{define, define_data, get, has, keys, BytesData, MapData, Obj, ObjKind, HIDDEN};
use crate::value::{PropKey, Value};

const UNCROSSED: [&str; 4] = ["stack", "constructor", "toString", "__proto__"];

/// A program value as a host copy.
pub fn to_host(value: &Value, label: &str, depth: usize, seen: &mut HashSet<usize>) -> JsResult<HostValue> {
    if depth > MAX_VALUE_DEPTH {
        return Err(type_error(format!("{label} exceeds the maximum value depth of {MAX_VALUE_DEPTH}.")));
    }
    let o = match value {
        Value::Undefined => return Ok(HostValue::Undefined),
        Value::Null => return Ok(HostValue::Null),
        Value::Bool(b) => return Ok(HostValue::Bool(*b)),
        Value::Number(n) => return Ok(HostValue::Number(*n)),
        Value::String(s) => return Ok(HostValue::String(s.to_rust())),
        Value::Object(o) if !o.is_opaque() => o.clone(),
        other => {
            return Err(type_error(format!(
                "{label} contains {}, which cannot be passed to an extension.",
                describe_value(other)
            )));
        }
    };
    if !seen.insert(o.addr()) {
        return Err(type_error(format!("{label} contains a circular value.")));
    }
    let result = (|| -> JsResult<HostValue> {
        let mut next = |item: &Value| to_host(item, label, depth + 1, &mut seen.clone());
        Ok(match o.kind() {
            ObjKind::Error(_) => {
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
                    if UNCROSSED.contains(&key.as_str()) || !has(&o, &PropKey::str(&key)) {
                        continue;
                    }
                    let item = match next(&get(&o, &PropKey::str(&key))?) {
                        Ok(v) => v,
                        Err(Thrown::Pending(_)) => continue,
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
            ObjKind::Array(_) => HostValue::Array(o.array_values().iter().map(&mut next).collect::<JsResult<_>>()?),
            ObjKind::Date(t) => HostValue::Date(t.get()),
            ObjKind::RegExp(r) => HostValue::RegExp { source: r.source(), flags: r.flags.clone() },
            ObjKind::URL(u) => HostValue::Url(u.href()),
            ObjKind::URLSearchParams(p) => HostValue::SearchParams(p.pairs()),
            ObjKind::Headers(h) => HostValue::Headers(h.borrow().list()),
            ObjKind::Bytes(b) => HostValue::Bytes(b.to_vec()),
            ObjKind::Map(m) => {
                let entries: Vec<(Value, Value)> = m.borrow().iter().cloned().collect();
                HostValue::Map(entries.iter().map(|(k, v)| Ok((next(k)?, next(v)?))).collect::<JsResult<_>>()?)
            }
            ObjKind::Set(s) => {
                let items: Vec<Value> = s.borrow().iter().map(|(k, _)| k.clone()).collect();
                HostValue::Set(items.iter().map(&mut next).collect::<JsResult<_>>()?)
            }
            ObjKind::WeakMap(_) | ObjKind::WeakSet(_) => HostValue::Object(Vec::new()),
            _ => {
                let mut fields = Vec::new();
                for key in keys(&o) {
                    if key.eq_str("__proto__") {
                        continue;
                    }
                    let item = get(&o, &PropKey::Str(key.clone()))?;
                    fields.push((key.to_rust(), next(&item)?));
                }
                HostValue::Object(fields)
            }
        })
    })();
    seen.remove(&o.addr());
    result
}

/// A host value as program values.
pub fn from_host(rt: &Rt, value: &HostValue, label: &str, depth: usize) -> JsResult<Value> {
    if depth > MAX_VALUE_DEPTH {
        return Err(type_error(format!("{label} exceeds the maximum value depth of {MAX_VALUE_DEPTH}.")));
    }
    let b = &rt.builtins;
    let next = |item: &HostValue, path: &str| from_host(rt, item, path, depth + 1);
    Ok(match value {
        HostValue::Undefined => Value::Undefined,
        HostValue::Null => Value::Null,
        HostValue::Bool(v) => Value::Bool(*v),
        HostValue::Number(n) => Value::Number(*n),
        HostValue::String(s) => Value::str(s),
        HostValue::Function(f) => wrap(rt, f.clone(), label, None),
        HostValue::Unrepresentable(description) => {
            return Err(type_error(format!("{label} produced {description}, which the program cannot hold.")));
        }
        HostValue::Date(t) => Value::Object(Obj::new(Some(b.date.clone()), ObjKind::Date(std::cell::Cell::new(*t)))),
        HostValue::RegExp { source, flags } => {
            Value::Object(crate::stdlib::regexp::construct_regexp_literal(rt, source, flags)?)
        }
        HostValue::Bytes(bytes) => Value::Object(Obj::new(Some(b.uint8_array.clone()), ObjKind::Bytes(BytesData::new(bytes.clone())))),
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

/// A host function as a program function. Arguments cross in; for a global, the `extension.before` hook runs before
/// the host function is called, so a call it denies never reaches the host. The host's result, or what it threw,
/// crosses out so the program catches a copy, and `extension.after` sees how the call ended. Functions inside
/// results are part of a value's API and skip the hooks. A denied call fails the way the global would: an async
/// global rejects its promise, a sync one throws.
pub fn wrap(rt: &Rt, f: Arc<dyn HostFunction>, label: &str, extension: Option<String>) -> Value {
    let label = label.to_string();
    let name = f.name().to_string();
    let length = f.length();
    Value::Object(native(
        &rt.builtins.function,
        &name,
        length,
        NativeFn {
            call: std::rc::Rc::new(move |rt, site, _, values| {
                let mut args = Vec::with_capacity(values.len());
                for (i, v) in values.iter().enumerate() {
                    match to_host(v, &format!("Argument {} to {label}", i + 1), 0, &mut HashSet::new()) {
                        Ok(h) => args.push(h),
                        Err(e) => return NResult::err(e),
                    }
                }
                let host = rt.tools.host.clone();
                let invocation = extension.as_ref().map(|extension| ExtensionInvocation {
                    id: rt.tools.next_call_id(),
                    extension: extension.clone(),
                    name: label.clone(),
                    args: args.clone(),
                });
                let hooked = Hooked { host, invocation, label: label.clone() };
                if f.asynchronous() {
                    let f = f.clone();
                    let work: BoxFuture<'static, Result<HostValue, HostValue>> = Box::pin(async move {
                        hooked.before().await?;
                        let called = f.call(args);
                        hooked.settle(called).await
                    });
                    return NResult::Ready(pending(rt, work, label.clone(), site.node));
                }
                if let Err(reason) = hooked.blocking_before(rt) {
                    return NResult::err(reason);
                }
                match f.call(args) {
                    HostCall::Pending(work) => {
                        let work: BoxFuture<'static, Result<HostValue, HostValue>> =
                            Box::pin(async move { hooked.settle(HostCall::Pending(work)).await });
                        NResult::Ready(pending(rt, work, label.clone(), site.node))
                    }
                    ready => {
                        let settled = rt.runner.block_on(rt, hooked.settle(ready));
                        NResult::Ready(match settled {
                            Err(e) => Err(e),
                            Ok(Ok(v)) => from_host(rt, &v, &label, 0),
                            Ok(Err(reason)) => Err(cross(rt, &reason, &label)),
                        })
                    }
                }
            }),
            construct: None,
            callback: true,
        },
    ))
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
            HostCall::Pending(work) => work.await,
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
    Ok(Value::Object(promise))
}


/// The global bindings of one run's extensions.
pub fn extension_globals(rt: &Rt) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    for extension in &rt.extensions {
        for (name, f) in extension.globals() {
            let value = wrap(rt, f, &name, Some(extension.name().to_string()));
            out.push((name, value));
        }
    }
    out
}
