//! What `import` and `require` load: a global, by the Node module name that provides it (upstream
//! `interpreter/modules.ts` and `Interpreter.module`/`namespace`).
//!
//! `fs` loads the `fs` global and `crypto` the `crypto` global; `fs/promises`, `url`, `util` and `buffer` come from
//! globals with other names, and a module that would be empty isn't available. Modules and namespaces are cached per
//! execution, so every import of a module sees the same value.

use super::Rt;
use super::errors::{ErrorType, JsResult, error_at};
use crate::ast::Loc;
use crate::object::{DATA, Obj, ObjKind, define, entries, get};
use crate::value::Value;

/// `node:fs` names the same module as `fs`.
pub fn module_name(specifier: &str) -> &str {
    specifier.strip_prefix("node:").unwrap_or(specifier)
}

/// A global binding's value, or `undefined` when there is none.
fn global(rt: &Rt, name: &str) -> Value {
    let Some(env) = rt.global.borrow().clone() else { return Value::Undefined };
    let vars = env.vars.borrow();
    vars.get(name).map(|b| b.value.clone()).unwrap_or(Value::Undefined)
}

fn pick(rt: &Rt, names: &[&str]) -> Value {
    let found: Vec<(&str, Value)> = names
        .iter()
        .filter_map(|name| Some((*name, global(rt, name))).filter(|(_, v)| !matches!(v, Value::Undefined)))
        .collect();
    if found.is_empty() {
        return Value::Undefined;
    }
    let module = Obj::new(None, ObjKind::Ordinary);
    for (name, value) in found {
        define(&module, name, value, DATA);
    }
    Value::Object(module)
}

/// `resolveModule`: the global of that name, else one of the aliases; `undefined` when there is none.
fn resolve_module(rt: &Rt, specifier: &str) -> JsResult<Value> {
    let name = module_name(specifier);
    let value = global(rt, name);
    // `global(name) ?? aliases[name]?.(global)`
    if !matches!(value, Value::Undefined | Value::Null) {
        return Ok(value);
    }
    Ok(match name {
        "fs/promises" => match global(rt, "fs") {
            Value::Object(fs) => get(&fs, &"promises".into())?,
            _ => Value::Undefined,
        },
        "url" => pick(rt, &["URL", "URLSearchParams"]),
        "util" => pick(rt, &["TextEncoder", "TextDecoder"]),
        "buffer" => pick(rt, &["Buffer", "atob", "btoa"]),
        _ => Value::Undefined,
    })
}

/// A module, as `require` returns it and a default import binds it: a global, by its Node module name.
pub fn module(rt: &Rt, specifier: &str, site: Option<Loc>) -> JsResult<Value> {
    let name = module_name(specifier);
    if let Some(cached) = rt.modules.borrow().get(name) {
        return Ok(cached.clone());
    }
    let module = resolve_module(rt, name)?;
    if matches!(module, Value::Undefined) {
        return Err(error_at(
            ErrorType::Error,
            format!(
                "Cannot import \"{specifier}\": it isn't available in Code Mode, which can't load packages or files. Use the provided globals, or call tools through `tools`."
            ),
            site,
        ));
    }
    rt.modules.borrow_mut().insert(name.to_string(), module.clone());
    Ok(module)
}

/// What `import * as` and `import()` see: the module's own enumerable properties, and the module itself as
/// `default`, the way Node presents a CommonJS module.
pub fn namespace(rt: &Rt, specifier: &str, site: Option<Loc>) -> JsResult<Obj> {
    let module = module(rt, specifier, site)?;
    let name = module_name(specifier);
    if let Some(cached) = rt.namespaces.borrow().get(name) {
        return Ok(cached.clone());
    }
    let namespace = Obj::new(None, ObjKind::Ordinary);
    if let Value::Object(source) = &module {
        for (key, value) in entries(source)? {
            define(&namespace, key, value, DATA);
        }
    }
    define(&namespace, "default", module, DATA);
    rt.namespaces.borrow_mut().insert(name.to_string(), namespace.clone());
    Ok(namespace)
}
