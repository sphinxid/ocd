//! Coercions. The `coerce_*` functions never run program code (built-in forms only); `to_primitive` and friends
//! call a data object's own `valueOf`/`toString` like JS ToPrimitive. Port of parts of `objects.ts` and
//! `callback.ts`.

use super::errors::{type_error, JsResult};
use super::{Rt, Site};
use crate::number;
use crate::object::{get, get_str, Obj, ObjKind};
use crate::value::{JsStr, PropKey, Value};

/// ToString without consulting program-defined methods.
pub fn coerce_to_string(value: &Value) -> JsStr {
    match value {
        Value::Undefined => JsStr::from("undefined"),
        Value::Null => JsStr::from("null"),
        Value::Bool(b) => JsStr::from(if *b { "true" } else { "false" }),
        Value::Number(n) => JsStr::from(number::to_string(*n)),
        Value::String(s) => s.clone(),
        Value::Symbol(s) => JsStr::from(s.description()),
        Value::Object(o) => obj_to_string(o),
        Value::Tool(_) => JsStr::from("[object Object]"),
    }
}

pub fn to_rust_string(value: &Value) -> String {
    coerce_to_string(value).to_rust()
}

/// An object's built-in string form (`Obj.toString` overrides upstream).
pub fn obj_to_string(o: &Obj) -> JsStr {
    match o.kind() {
        ObjKind::Array(a) => {
            let items: Vec<Option<Value>> = a.borrow().items.clone();
            let mut out: Vec<u16> = Vec::new();
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',' as u16);
                }
                match item {
                    None | Some(Value::Undefined) | Some(Value::Null) => {}
                    Some(v) => out.extend_from_slice(coerce_to_string(v).units()),
                }
            }
            JsStr::from_units(out)
        }
        ObjKind::Error(_) => {
            let name = get_str(o, "name").unwrap_or(Value::Undefined);
            let message = get_str(o, "message").unwrap_or(Value::Undefined);
            let shown_name = match &name {
                Value::String(s) => s.to_rust(),
                _ => "Error".into(),
            };
            let shown_message = match &message {
                Value::String(s) => s.to_rust(),
                _ => String::new(),
            };
            JsStr::from(error_text(&shown_name, &shown_message))
        }
        ObjKind::Date(t) => JsStr::from(crate::stdlib::date::to_display(t.get())),
        ObjKind::RegExp(r) => JsStr::from(r.display()),
        ObjKind::URLSearchParams(p) => JsStr::from(p.serialize()),
        ObjKind::URL(u) => JsStr::from(u.href()),
        ObjKind::Bytes(b) => {
            let s: Vec<String> = b.to_vec().iter().map(|x| x.to_string()).collect();
            JsStr::from(s.join(","))
        }
        _ => JsStr::from(format!("[object {}]", o.tag())),
    }
}

/// Error.prototype.toString: "name: message", or just one when the other is empty.
pub fn error_text(name: &str, message: &str) -> String {
    if message.is_empty() {
        return name.to_string();
    }
    if name.is_empty() {
        return message.to_string();
    }
    format!("{name}: {message}")
}

/// ToNumber without consulting program-defined methods; tool references are not numbers.
pub fn coerce_to_number(value: &Value) -> f64 {
    match value {
        Value::Undefined => f64::NAN,
        Value::Null => 0.0,
        Value::Bool(b) => {
            if *b {
                1.0
            } else {
                0.0
            }
        }
        Value::Number(n) => *n,
        Value::String(s) => number::string_to_number(s.units()),
        Value::Symbol(_) => f64::NAN,
        Value::Object(o) => match o.kind() {
            ObjKind::Date(t) => t.get(),
            _ => number::string_to_number(obj_to_string(o).units()),
        },
        Value::Tool(_) => f64::NAN,
    }
}

/// ToIntegerOrInfinity: NaN is 0, fractions truncate.
pub fn coerce_to_integer(value: &Value) -> f64 {
    number::to_integer_or_infinity(coerce_to_number(value))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Hint {
    Number,
    String,
    Default,
}

fn is_primitive(v: &Value) -> bool {
    !matches!(v, Value::Object(_) | Value::Tool(_))
}

/// ToPrimitive: calls `valueOf`/`toString` in hint order and returns the first primitive result. Dates treat the
/// default hint as "string". Opaque values (functions, promises, generators, tool references) pass through.
pub async fn to_primitive(rt: &Rt, value: &Value, hint: Hint, site: Site) -> JsResult<Value> {
    let Value::Object(o) = value else { return Ok(value.clone()) };
    if o.is_opaque() {
        return Ok(value.clone());
    }
    let as_string = hint == Hint::String || (hint == Hint::Default && matches!(o.kind(), ObjKind::Date(_)));
    let order = if as_string { ["toString", "valueOf"] } else { ["valueOf", "toString"] };
    for name in order {
        let callable = get(o, &PropKey::str(name))?;
        if !matches!(&callable, Value::Object(c) if c.is_callable()) {
            continue;
        }
        let result = super::eval::call_value(rt, &callable, value.clone(), Vec::new(), site).await?;
        if is_primitive(&result) {
            return Ok(result);
        }
    }
    Err(type_error("Cannot convert object to primitive value."))
}

pub async fn to_primitive_string(rt: &Rt, value: &Value, site: Site) -> JsResult<JsStr> {
    Ok(coerce_to_string(&to_primitive(rt, value, Hint::String, site).await?))
}

pub async fn to_primitive_number(rt: &Rt, value: &Value, site: Site) -> JsResult<f64> {
    Ok(coerce_to_number(&to_primitive(rt, value, Hint::Number, site).await?))
}

/// Whether any argument needs program-defined conversion.
pub fn needs_conversion(values: &[Value]) -> bool {
    values.iter().any(|v| matches!(v, Value::Object(_)))
}

/// ToPrimitive over `values` in order, one hint per position.
pub async fn primitives(rt: &Rt, hints: &[Hint], values: Vec<Value>, site: Site) -> JsResult<Vec<Value>> {
    let mut out = Vec::with_capacity(values.len());
    for (i, v) in values.into_iter().enumerate() {
        let hint = hints.get(i).copied().unwrap_or(*hints.last().unwrap_or(&Hint::Default));
        out.push(to_primitive(rt, &v, hint, site).await?);
    }
    Ok(out)
}

/// Runs `body` on the arguments after ToPrimitive; primitive arguments skip the async path entirely.
pub fn with_primitives(
    rt: &Rt,
    site: Site,
    hints: Vec<Hint>,
    values: Vec<Value>,
    body: impl FnOnce(Vec<Value>) -> JsResult<Value> + 'static,
) -> super::function::NResult {
    if !needs_conversion(&values) {
        return super::function::NResult::Ready(body(values));
    }
    let rt = rt.clone();
    super::function::NResult::future(async move {
        let converted = primitives(&rt, &hints, values, site).await?;
        body(converted)
    })
}
