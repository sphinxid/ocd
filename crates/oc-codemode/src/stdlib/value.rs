//! Global coercion functions (`Number`, `parseInt`, ...). Port of `stdlib/value.ts`.

use crate::interp::coerce::{coerce_to_number, coerce_to_string, with_primitives, Hint};
use crate::interp::function::{arg, fn_, NResult};
use crate::interp::{Rt, Site};
use crate::number;
use crate::value::Value;

pub fn coerce(rt: &Rt, site: Site, name: &str, args: Vec<Value>) -> NResult {
    // Native: Number() is 0 and String() is "", unlike their undefined-argument forms.
    if args.is_empty() {
        if name == "Number" {
            return NResult::ok(0.0);
        }
        if name == "String" {
            return NResult::ok("");
        }
    }
    let raw = arg(&args, 0);
    if name == "Boolean" {
        return NResult::ok(raw.truthy());
    }
    if name == "parseInt" {
        return with_primitives(rt, site, vec![Hint::String, Hint::Number], vec![raw, arg(&args, 1)], |p| {
            Ok(Value::Number(number::parse_int(coerce_to_string(&p[0]).units(), coerce_to_number(&p[1]))))
        });
    }
    let hint = if name == "String" || name == "parseFloat" { Hint::String } else { Hint::Number };
    let name = name.to_string();
    with_primitives(rt, site, vec![hint], vec![raw], move |p| {
        let v = &p[0];
        Ok(match name.as_str() {
            "Number" => Value::Number(coerce_to_number(v)),
            "isFinite" => Value::Bool(coerce_to_number(v).is_finite()),
            "isNaN" => Value::Bool(coerce_to_number(v).is_nan()),
            "parseFloat" => Value::Number(number::parse_float(coerce_to_string(v).units())),
            _ => Value::String(coerce_to_string(v)),
        })
    })
}

/// A global coercion function such as `Number` or `parseInt`.
pub fn coercion(rt: &Rt, name: &'static str, length: usize) -> Value {
    Value::Object(fn_(&rt.builtins.function, name, length, move |rt, site, _, args| coerce(rt, site, name, args)))
}
