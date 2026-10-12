//! Callback admission and invocation shared by every built-in that calls back into the program.
//! Port of the callback half of `interpreter/callback.ts`.

use futures::future::LocalBoxFuture;

use super::errors::{type_error, JsResult};
use super::eval::call_value;
use super::{Rt, Site};
use crate::object::get;
use crate::value::{PropKey, Value};

/// The single acceptance list for callbacks: collections, sort, string replacers, `Array.from` mappers, and
/// promise reactions all admit exactly these callables.
pub fn is_supported_callback(value: &Value) -> bool {
    match value {
        Value::Object(o) if o.is_callable() => !matches!(o.native_fn(), Some(n) if !n.callback),
        _ => false,
    }
}

pub type Callback = Box<dyn Fn(&Rt, Vec<Value>, Value, Site) -> LocalBoxFuture<'static, JsResult<Value>>>;

pub fn apply_collection_callback(callback: &Value, name: &str) -> JsResult<Callback> {
    if !is_supported_callback(callback) {
        if callback.js_typeof() == "function" {
            return Err(type_error(format!(
                "{name} cannot use this callable as a callback; wrap it in an arrow function, e.g. (value) => tools.ns.tool(value)."
            )));
        }
        return Err(type_error(format!("{name} expects a function callback.")));
    }
    let callback = callback.clone();
    Ok(Box::new(move |rt, args, this, site| {
        let (rt, callback) = (rt.clone(), callback.clone());
        Box::pin(async move { call_value(&rt, &callback, this, args, site).await })
    }))
}

/// Invoke(value, name): calls the method the value would find through its prototype.
pub async fn invoke(rt: &Rt, value: &Value, name: &str, label: &str, site: Site) -> JsResult<Value> {
    let target = match value {
        Value::Object(o) => o.clone(),
        other => match rt.builtins.primitive_prototype(other) {
            Some(p) => p.clone(),
            None => return Err(type_error(format!("{label} called on null or undefined."))),
        },
    };
    let method = get(&target, &PropKey::str(name))?;
    call_value(rt, &method, value.clone(), Vec::new(), site).await
}
