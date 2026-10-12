//! `Number` and `Boolean`: constants, predicates, parsers, and the formatting methods. Port of `stdlib/number.ts`.

use std::rc::Rc;

use crate::interp::coerce::{coerce_to_number, with_primitives, Hint};
use crate::interp::errors::{host_error, range_error, type_error, ErrorType, JsResult};
use crate::interp::function::{arg, constants, constructor, method, methods, Method, NResult, NativeFn};
use crate::interp::Rt;
use crate::number;
use crate::stdlib::value::coerce;
use crate::value::Value;

/// ToNumber of an already-primitive value, throwing the host's `TypeError` for symbols as JavaScript does.
pub fn to_number(value: &Value) -> JsResult<f64> {
    if let Value::Symbol(_) = value {
        return Err(host_error(ErrorType::TypeError, "Cannot convert a symbol to a number"));
    }
    Ok(coerce_to_number(value))
}

/// Number.isInteger without coercion.
fn is_integer(value: &Value) -> bool {
    matches!(value, Value::Number(n) if n.is_finite() && n.trunc() == *n)
}

fn this_number(this: &Value, name: &str) -> JsResult<f64> {
    match this {
        Value::Number(n) => Ok(*n),
        _ => Err(type_error(format!("Number.prototype.{name} requires that 'this' be a Number."))),
    }
}

fn range(message: &str) -> crate::interp::errors::Thrown {
    host_error(ErrorType::RangeError, message)
}

fn to_fixed(value: f64, digits: Option<f64>) -> JsResult<String> {
    let f = number::to_integer_or_infinity(digits.unwrap_or(0.0));
    // JavaScriptCore checks the range before the receiver's finiteness: NaN.toFixed(200) throws.
    if !(0.0..=100.0).contains(&f) {
        return Err(range("toFixed() argument must be between 0 and 100"));
    }
    if !value.is_finite() || value.abs() >= 1e21 {
        return Ok(number::to_string(value));
    }
    Ok(number::to_fixed(value, f as usize))
}

fn to_exponential(value: f64, digits: Option<f64>) -> JsResult<String> {
    let f = number::to_integer_or_infinity(digits.unwrap_or(0.0));
    if !value.is_finite() {
        return Ok(number::to_string(value));
    }
    if !(0.0..=100.0).contains(&f) {
        return Err(range("toExponential() argument must be between 0 and 100"));
    }
    Ok(number::to_exponential(value, digits.map(|_| f as usize)))
}

fn to_precision(value: f64, digits: Option<f64>) -> JsResult<String> {
    let Some(digits) = digits else { return Ok(number::to_string(value)) };
    let p = number::to_integer_or_infinity(digits);
    if !value.is_finite() {
        return Ok(number::to_string(value));
    }
    if !(1.0..=100.0).contains(&p) {
        return Err(range("toPrecision() argument must be between 1 and 100"));
    }
    Ok(number::to_precision(value, p as usize))
}

fn to_string_radix(value: f64, radix: Option<f64>) -> JsResult<String> {
    let Some(radix) = radix else { return Ok(number::to_string(value)) };
    if radix < 2.0 || radix > 36.0 {
        return Err(range_error("Number.toString radix must be between 2 and 36."));
    }
    // What passes CodeMode's own check reaches the host, which truncates (NaN becomes 0 and throws).
    let r = number::to_integer_or_infinity(radix);
    if !(2.0..=36.0).contains(&r) {
        return Err(range("toString() radix argument must be between 2 and 36"));
    }
    if r == 10.0 {
        return Ok(number::to_string(value));
    }
    Ok(number::to_radix_string(value, r as u32))
}

/// A formatting method: the receiver is checked first, then the one argument converts through ToPrimitive with the
/// number hint.
fn formatting(name: &'static str, op: fn(f64, Option<f64>) -> JsResult<String>) -> Method {
    method(name, 1, move |rt, site, this, args| {
        let value = match this_number(&this, name) {
            Ok(v) => v,
            Err(e) => return NResult::err(e),
        };
        with_primitives(rt, site, vec![Hint::Number], vec![arg(&args, 0)], move |p| {
            let digits = match &p[0] {
                Value::Undefined => None,
                other => Some(to_number(other)?),
            };
            Ok(Value::from(op(value, digits)?))
        })
    })
}

pub fn number_global(rt: &Rt) -> Value {
    let b = &rt.builtins;
    let ctor = constructor(
        &b.function,
        &b.number,
        "Number",
        1,
        NativeFn {
            call: Rc::new(|rt, site, _, args| coerce(rt, site, "Number", args)),
            construct: None,
            callback: true,
        },
    );
    constants(
        &ctor,
        vec![
            ("MAX_SAFE_INTEGER", Value::Number(9007199254740991.0)),
            ("MIN_SAFE_INTEGER", Value::Number(-9007199254740991.0)),
            ("MAX_VALUE", Value::Number(f64::MAX)),
            ("MIN_VALUE", Value::Number(5e-324)),
            ("EPSILON", Value::Number(f64::EPSILON)),
            ("NaN", Value::Number(f64::NAN)),
            ("POSITIVE_INFINITY", Value::Number(f64::INFINITY)),
            ("NEGATIVE_INFINITY", Value::Number(f64::NEG_INFINITY)),
        ],
    );
    methods(
        &b.function,
        &ctor,
        vec![
            method("isInteger", 1, |_, _, _, args| NResult::ok(is_integer(&arg(&args, 0)))),
            method("isFinite", 1, |_, _, _, args| {
                NResult::ok(matches!(arg(&args, 0), Value::Number(n) if n.is_finite()))
            }),
            method("isNaN", 1, |_, _, _, args| NResult::ok(matches!(arg(&args, 0), Value::Number(n) if n.is_nan()))),
            method("isSafeInteger", 1, |_, _, _, args| {
                let v = arg(&args, 0);
                NResult::ok(is_integer(&v) && matches!(v, Value::Number(n) if n.abs() <= 9007199254740991.0))
            }),
            method("parseInt", 2, |rt, site, _, args| coerce(rt, site, "parseInt", args)),
            method("parseFloat", 1, |rt, site, _, args| coerce(rt, site, "parseFloat", args)),
        ],
    );
    methods(
        &b.function,
        &b.number,
        vec![
            formatting("toFixed", to_fixed),
            method("toLocaleString", 0, |_, _, this, _| {
                NResult::Ready(this_number(&this, "toLocaleString").map(|n| Value::from(number::to_locale_string(n))))
            }),
            formatting("toExponential", to_exponential),
            formatting("toPrecision", to_precision),
            formatting("toString", to_string_radix),
            method("valueOf", 0, |_, _, this, _| NResult::Ready(this_number(&this, "valueOf").map(Value::Number))),
        ],
    );
    Value::Object(ctor)
}

pub fn boolean_global(rt: &Rt) -> Value {
    let b = &rt.builtins;
    let ctor = constructor(
        &b.function,
        &b.boolean,
        "Boolean",
        1,
        NativeFn {
            call: Rc::new(|rt, site, _, args| coerce(rt, site, "Boolean", args)),
            construct: None,
            callback: true,
        },
    );
    let this_bool = |this: &Value, name: &str| -> JsResult<bool> {
        match this {
            Value::Bool(b) => Ok(*b),
            _ => Err(type_error(format!("Boolean.prototype.{name} requires that 'this' be a Boolean."))),
        }
    };
    methods(
        &b.function,
        &b.boolean,
        vec![
            method("toString", 0, move |_, _, this, _| {
                NResult::Ready(this_bool(&this, "toString").map(|b| Value::str(if b { "true" } else { "false" })))
            }),
            method("valueOf", 0, move |_, _, this, _| NResult::Ready(this_bool(&this, "valueOf").map(Value::Bool))),
        ],
    );
    Value::Object(ctor)
}
