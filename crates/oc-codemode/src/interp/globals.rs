//! The global table, error constructors and normalization, and `Function`/`Symbol`. Port of
//! `interpreter/globals.ts` and `interpreter/errors.ts`.

use std::rc::Rc;

use super::coerce::{coerce_to_integer, coerce_to_string, to_primitive_string};
use super::errors::*;
use super::eval::call_value;
use super::function::{arg, constants, constructor, fn_, method, methods, native, prototype_from, receiver, NResult, NativeFn};
use super::iteration;
use super::limits::check_array_length;
use super::references::contains_runtime_reference;
use super::{Rt, Site};
use crate::object::{define, get, get_str, has, Obj, ObjKind, HIDDEN};
use crate::stdlib;
use crate::value::{PropKey, Sym, ToolRef, Value};

pub fn create_error_value(prototype: &Obj, message: Option<&str>) -> Obj {
    let value = Obj::new(Some(prototype.clone()), ObjKind::Error(std::cell::RefCell::new(None)));
    if let Some(m) = message {
        define(&value, "message", Value::str(m), HIDDEN);
    }
    value
}

/// Error.prototype.toString as diagnostics show it.
pub fn error_to_string(value: &Obj) -> String {
    let name = get_str(value, "name").unwrap_or_default();
    let message = get_str(value, "message").unwrap_or_default();
    let shown_name = if name.is_undefined() { "Error".to_string() } else { coerce_to_string(&name).to_rust() };
    let shown_message = if message.is_undefined() { String::new() } else { coerce_to_string(&message).to_rust() };
    super::coerce::error_text(&shown_name, &shown_message)
}

pub fn normalize_error(error: &Thrown) -> Diagnostic {
    match error {
        Thrown::Pending(p) => {
            let node = *p.node.borrow();
            Diagnostic {
                kind: p.kind,
                message: format!("{}: {}{}", p.error_type.name(), p.message, format_location(node)),
                location: node.map(source_location),
                suggestions: p.suggestions.clone(),
            }
        }
        Thrown::ToolRuntime(e) => Diagnostic {
            kind: e.kind,
            message: e.message.clone(),
            location: None,
            suggestions: if e.suggestions.is_empty() { None } else { Some(e.suggestions.clone()) },
        },
        Thrown::ToolFailure(m) => Diagnostic::new(DiagnosticKind::ToolFailure, m.to_string()),
        Thrown::Value(value) => {
            if let Value::Object(o) = value
                && let ObjKind::Error(host) = o.kind()
            {
                let host = host.borrow().clone();
                return match host {
                    Some(h) => normalize_error(&Thrown::Pending(h)),
                    None => Diagnostic::new(DiagnosticKind::ExecutionFailure, error_to_string(o)),
                };
            }
            let message = if contains_runtime_reference(value) {
                // Never expose runtime reference internals through thrown values.
                "a non-data value".to_string()
            } else if let Value::String(s) = value {
                s.to_rust()
            } else {
                stdlib::console::format_value(value)
            };
            Diagnostic::new(DiagnosticKind::ExecutionFailure, format!("Uncaught: {message}"))
        }
        Thrown::GeneratorReturn(_) => Diagnostic::new(DiagnosticKind::ExecutionFailure, "Uncaught: generator return"),
        Thrown::Interrupt => Diagnostic::new(DiagnosticKind::ExecutionFailure, "Execution was interrupted."),
        Thrown::ShortCircuit => Diagnostic::new(DiagnosticKind::ExecutionFailure, "Optional chain short-circuited."),
    }
}

pub fn create_aggregate_error_value(rt: &Rt, errors: Vec<Value>, message: &str, proto: Option<&Obj>) -> Obj {
    let proto = proto.cloned().unwrap_or_else(|| rt.builtins.error(ErrorType::AggregateError).clone());
    let value = create_error_value(&proto, Some(message));
    define(&value, "errors", Value::Object(Obj::array(&rt.builtins.array, errors)), HIDDEN);
    value
}

fn construct_error(rt: &Rt, site: Site, error_type: ErrorType, args: Vec<Value>, proto: Obj) -> NResult {
    let rt = rt.clone();
    NResult::future(async move {
        let created = if error_type == ErrorType::AggregateError {
            let message = match arg(&args, 1) {
                Value::Undefined => String::new(),
                m => to_primitive_string(&rt, &m, site).await?.to_rust(),
            };
            let cursor = iteration::iterate(&rt, &arg(&args, 0), None, site).await?;
            let Some(cursor) = cursor else {
                return Err(type_error("new AggregateError(...) expects a synchronous iterable of errors."));
            };
            let mut errors = Vec::new();
            loop {
                let step = cursor.next().await?;
                if step.done {
                    break;
                }
                errors.push(step.value);
            }
            create_aggregate_error_value(&rt, errors, &message, Some(&proto))
        } else {
            let message = match arg(&args, 0) {
                Value::Undefined => None,
                m => Some(to_primitive_string(&rt, &m, site).await?.to_rust()),
            };
            create_error_value(&proto, message.as_deref())
        };
        // ES2022 `new Error(message, { cause })`: installed only when the options object has the property.
        let options = arg(&args, if error_type == ErrorType::AggregateError { 2 } else { 1 });
        if let Value::Object(o) = &options
            && has(o, &PropKey::str("cause"))
        {
            define(&created, "cause", get_str(o, "cause")?, HIDDEN);
        }
        Ok(Value::Object(created))
    })
}

/// An error constructor such as `Error` or `TypeError`; callable with or without `new`, like JS.
pub fn error_global(rt: &Rt, error_type: ErrorType) -> Value {
    let b = &rt.builtins;
    let prototype = b.error(error_type).clone();
    let p2 = prototype.clone();
    let ctor = constructor(
        &b.function,
        &prototype,
        error_type.name(),
        if error_type == ErrorType::AggregateError { 2 } else { 1 },
        NativeFn {
            call: Rc::new(move |rt, site, _, args| construct_error(rt, site, error_type, args, p2.clone())),
            construct: Some(Rc::new(move |rt, site, args, new_target| {
                let proto = prototype_from(&new_target, rt.builtins.error(error_type));
                construct_error(rt, site, error_type, args, proto)
            })),
            callback: true,
        },
    );
    if error_type == ErrorType::Error {
        methods(
            &b.function,
            &prototype,
            vec![method("toString", 0, |_, _, this, _| {
                NResult::Ready(receiver(&this, "Error.prototype.toString", |_| true).map(|o| Value::str(&error_to_string(&o))))
            })],
        );
        methods(
            &b.function,
            &ctor,
            vec![method("isError", 1, |_, _, _, args| {
                NResult::ok(matches!(arg(&args, 0), Value::Object(o) if o.is_error()))
            })],
        );
        return Value::Object(ctor);
    }
    // Derived constructors extend Error itself, so its statics are inherited.
    if let Ok(Value::Object(error_ctor)) = get(b.error(ErrorType::Error), &PropKey::str("constructor")) {
        ctor.set_proto(Some(error_ctor));
    }
    Value::Object(ctor)
}

// CreateListFromArrayLike: `apply` reads `length` and the indexed properties of any object.
fn list_from_array_like(value: &Value) -> JsResult<Vec<Value>> {
    match value {
        Value::Undefined | Value::Null => Ok(Vec::new()),
        Value::Object(o) if o.is_array() => Ok(o.array_values()),
        Value::Object(o) => {
            let length = coerce_to_integer(&get_str(o, "length")?).max(0.0);
            check_array_length(length)?;
            let mut out = Vec::new();
            for i in 0..length as usize {
                out.push(get(o, &PropKey::str(&i.to_string()))?);
            }
            Ok(out)
        }
        _ => Err(type_error("Function.prototype.apply expects an array-like argument list.")),
    }
}

// Function.prototype.constructor exists so `fn.constructor === Function` holds; dynamic code is unsupported.
fn function_global(rt: &Rt) -> Value {
    let b = &rt.builtins;
    let target = |this: &Value, m: &str| receiver(this, &format!("Function.prototype.{m}"), |o| o.is_callable());
    methods(
        &b.function,
        &b.function,
        vec![
            method("call", 1, move |rt, site, this, args| {
                let f = match target(&this, "call") {
                    Ok(f) => f,
                    Err(e) => return NResult::err(e),
                };
                let rt = rt.clone();
                NResult::future(async move {
                    call_value(&rt, &Value::Object(f), arg(&args, 0), args.get(1..).map(|s| s.to_vec()).unwrap_or_default(), site)
                        .await
                })
            }),
            method("apply", 2, move |rt, site, this, args| {
                let f = match target(&this, "apply") {
                    Ok(f) => f,
                    Err(e) => return NResult::err(e),
                };
                let list = match list_from_array_like(&arg(&args, 1)) {
                    Ok(l) => l,
                    Err(e) => return NResult::err(e),
                };
                let rt = rt.clone();
                NResult::future(async move { call_value(&rt, &Value::Object(f), arg(&args, 0), list, site).await })
            }),
            method("bind", 1, move |rt, _, this, args| {
                let f = match target(&this, "bind") {
                    Ok(f) => f,
                    Err(e) => return NResult::err(e),
                };
                let bound: Vec<Value> = args.get(1..).map(|s| s.to_vec()).unwrap_or_default();
                let name = format!("bound {}", coerce_to_string(&get_str(&f, "name").unwrap_or_default()));
                let length = f.func().map(|k| k.length).unwrap_or(0).saturating_sub(bound.len());
                let this_arg = arg(&args, 0);
                let fv = Value::Object(f);
                let bound_fn = native(
                    &rt.builtins.function,
                    &name,
                    length,
                    NativeFn {
                        call: Rc::new(move |rt, site, _, rest| {
                            let (rt, fv, this_arg) = (rt.clone(), fv.clone(), this_arg.clone());
                            let mut all = bound.clone();
                            all.extend(rest);
                            NResult::future(async move { call_value(&rt, &fv, this_arg, all, site).await })
                        }),
                        construct: None,
                        callback: true,
                    },
                );
                NResult::ok(Value::Object(bound_fn))
            }),
        ],
    );
    let reject: super::function::NativeCall =
        Rc::new(|_, _, _, _| NResult::err(type_error("The Function constructor is not supported; write the function inline.")));
    Value::Object(constructor(
        &b.function,
        &b.function,
        "Function",
        1,
        NativeFn {
            call: reject.clone(),
            construct: Some(Rc::new(move |rt, site, args, _| reject(rt, site, Value::Undefined, args))),
            callback: true,
        },
    ))
}

fn symbol_global(rt: &Rt) -> Value {
    let symbol = native(
        &rt.builtins.function,
        "Symbol",
        0,
        NativeFn {
            call: Rc::new(|_, _, _, _| {
                NResult::err(type_error(
                    "Symbol is not callable; only Symbol.asyncIterator and Symbol.iterator are available.",
                ))
            }),
            construct: None,
            callback: false,
        },
    );
    constants(
        &symbol,
        vec![("asyncIterator", Value::Symbol(Sym::AsyncIterator)), ("iterator", Value::Symbol(Sym::Iterator))],
    );
    Value::Object(symbol)
}

/// Names bound in every program before extensions apply, in declaration order.
pub const GLOBAL_NAMES: &[&str] = &[
    "tools",
    "search",
    "undefined",
    "NaN",
    "Infinity",
    "Object",
    "Function",
    "Array",
    "Math",
    "JSON",
    "console",
    "Promise",
    "Symbol",
    "Iterator",
    "Number",
    "String",
    "Boolean",
    "parseInt",
    "parseFloat",
    "isFinite",
    "isNaN",
    "Date",
    "RegExp",
    "Map",
    "Set",
    "WeakMap",
    "WeakSet",
    "URL",
    "URLSearchParams",
    "Headers",
    "Uint8Array",
    "TextEncoder",
    "TextDecoder",
    "encodeURI",
    "encodeURIComponent",
    "decodeURI",
    "decodeURIComponent",
    "atob",
    "btoa",
    "crypto",
    "structuredClone",
    "Error",
    "TypeError",
    "RangeError",
    "SyntaxError",
    "ReferenceError",
    "EvalError",
    "URIError",
    "AggregateError",
];

fn search_global(rt: &Rt) -> Value {
    Value::Object(native(
        &rt.builtins.function,
        "search",
        0,
        NativeFn {
            call: Rc::new(|rt, site, _, args| {
                let rt = rt.clone();
                NResult::future(async move { crate::tool_runtime::run_search(&rt, args, site).await })
            }),
            construct: None,
            callback: false,
        },
    ))
}

/// The immutable global bindings of every program, in declaration order.
pub fn globals(rt: &Rt) -> Vec<(&'static str, Value)> {
    super::generators::generator_globals(rt);
    let mut out = Vec::new();
    for &name in GLOBAL_NAMES {
        let value = match name {
            "tools" => Value::Tool(ToolRef::new(Vec::new())),
            "search" => search_global(rt),
            "undefined" => Value::Undefined,
            "NaN" => Value::Number(f64::NAN),
            "Infinity" => Value::Number(f64::INFINITY),
            "Object" => stdlib::object::object_global(rt),
            "Function" => function_global(rt),
            "Array" => stdlib::array::array_global(rt),
            "Math" => stdlib::math::math_global(rt),
            "JSON" => stdlib::json::json_global(rt),
            "console" => stdlib::console::console_global(rt),
            "Promise" => super::promises::promise_global(rt),
            "Symbol" => symbol_global(rt),
            "Iterator" => stdlib::iterator::iterator_globals(rt),
            "Number" => stdlib::number::number_global(rt),
            "String" => stdlib::string::string_global(rt),
            "Boolean" => stdlib::number::boolean_global(rt),
            "parseInt" => stdlib::value::coercion(rt, "parseInt", 2),
            "parseFloat" => stdlib::value::coercion(rt, "parseFloat", 1),
            "isFinite" => stdlib::value::coercion(rt, "isFinite", 1),
            "isNaN" => stdlib::value::coercion(rt, "isNaN", 1),
            "Date" => stdlib::date::date_global(rt),
            "RegExp" => stdlib::regexp::regexp_global(rt),
            "Map" => stdlib::collections::map_global(rt),
            "Set" => stdlib::collections::set_global(rt),
            "WeakMap" => stdlib::collections::weak_map_global(rt),
            "WeakSet" => stdlib::collections::weak_set_global(rt),
            "URL" => stdlib::url::url_global(rt),
            "URLSearchParams" => stdlib::url::url_search_params_global(rt),
            "Headers" => stdlib::headers::headers_global(rt),
            "Uint8Array" => stdlib::bytes::uint8_array_global(rt),
            "TextEncoder" => stdlib::bytes::text_encoder_global(rt),
            "TextDecoder" => stdlib::bytes::text_decoder_global(rt),
            "encodeURI" | "encodeURIComponent" | "decodeURI" | "decodeURIComponent" => {
                stdlib::url::uri_global(rt, name)
            }
            "atob" | "btoa" => stdlib::web::base64_global(rt, name),
            "crypto" => stdlib::web::crypto_global(rt),
            "structuredClone" => stdlib::web::structured_clone_global(rt),
            other => match ErrorType::from_name(other) {
                Some(t) => error_global(rt, t),
                None => Value::Undefined,
            },
        };
        out.push((name, value));
    }
    out
}

/// Helper for natives: a one-off plain function value.
pub fn function_value(rt: &Rt, name: &str, length: usize, f: impl Fn(&Rt, Site, Value, Vec<Value>) -> NResult + 'static) -> Value {
    Value::Object(fn_(&rt.builtins.function, name, length, f))
}
