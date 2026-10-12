//! `atob`/`btoa`, `crypto`, and `structuredClone`. Port of `stdlib/web.ts`.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use rand::RngCore;

use crate::interp::coerce::coerce_to_string;
use crate::interp::errors::{ErrorType, JsResult, range_error, type_error};
use crate::interp::function::{NResult, arg, fn_, method, methods};
use crate::interp::globals::create_error_value;
use crate::interp::references::is_runtime_reference;
use crate::interp::{Rt, describe_value};
use crate::object::{BytesData, DATA, HIDDEN, MapData, Obj, ObjKind, define, entries, get_own, get_str, has_own};
use crate::stdlib::bytes::{base64_digit, encode_base64};
use crate::value::{JsStr, PropKey, Value};

/// forgiving-base64 decode (HTML): `None` is a failure.
fn forgiving_base64(units: &[u16]) -> Option<Vec<u16>> {
    let mut data: Vec<u16> =
        units.iter().copied().filter(|&c| !matches!(c, 0x09 | 0x0A | 0x0C | 0x0D | 0x20)).collect();
    if data.len().is_multiple_of(4) {
        if data.ends_with(&[b'=' as u16, b'=' as u16]) {
            data.truncate(data.len() - 2);
        } else if data.ends_with(&[b'=' as u16]) {
            data.truncate(data.len() - 1);
        }
    }
    if data.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(data.len() * 3 / 4);
    let mut buffer: u32 = 0;
    let mut bits = 0;
    for c in data {
        buffer = (buffer << 6) | base64_digit(c)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buffer >> bits) & 0xFF) as u16);
        }
    }
    Some(out)
}

// WebIDL DOMString conversion: a missing argument is a TypeError, anything else stringifies. Invalid input is a
// TypeError as well; browsers throw a DOMException named InvalidCharacterError, which CodeMode does not have.
pub fn base64_global(rt: &Rt, name: &str) -> Value {
    let decode = name == "atob";
    let label = name.to_string();
    Value::Object(fn_(&rt.builtins.function, name, 1, move |_, _, _, args| {
        if args.is_empty() {
            return NResult::err(type_error(format!("{label} requires 1 argument (a string)")));
        }
        let input = coerce_to_string(&args[0]);
        let result = if decode {
            forgiving_base64(input.units()).map(JsStr::from_units)
        } else if input.units().iter().all(|&c| c <= 0xFF) {
            let bytes: Vec<u8> = input.units().iter().map(|&c| c as u8).collect();
            Some(JsStr::from(encode_base64(&bytes)))
        } else {
            None
        };
        match result {
            Some(s) => NResult::ok(Value::String(s)),
            None => NResult::err(type_error("The string contains invalid characters.")),
        }
    }))
}

/// Deepest nesting `structuredClone` follows before failing like the host's stack overflow would.
const MAX_CLONE_DEPTH: usize = 10_000;

struct Cloner<'a> {
    rt: &'a Rt,
    memo: HashMap<usize, Obj>,
    depth: Cell<usize>,
}

impl Cloner<'_> {
    fn uncloneable(value: &Value) -> crate::interp::errors::Thrown {
        type_error(format!("DataCloneError: {} could not be cloned.", describe_value(value)))
    }

    fn clone_value(&mut self, value: &Value) -> JsResult<Value> {
        let o = match value {
            Value::Symbol(_) | Value::Tool(_) => return Err(Self::uncloneable(value)),
            Value::Object(o) => o.clone(),
            other => return Ok(other.clone()),
        };
        if let Some(seen) = self.memo.get(&o.addr()) {
            return Ok(Value::Object(seen.clone()));
        }
        if self.depth.get() >= MAX_CLONE_DEPTH {
            return Err(range_error("Maximum call stack size exceeded."));
        }
        self.depth.set(self.depth.get() + 1);
        let result = self.clone_object(&o, value);
        self.depth.set(self.depth.get() - 1);
        result.map(Value::Object)
    }

    fn remember(&mut self, source: &Obj, copy: Obj) -> Obj {
        self.memo.insert(source.addr(), copy.clone());
        copy
    }

    fn clone_object(&mut self, o: &Obj, value: &Value) -> JsResult<Obj> {
        let b = &self.rt.builtins;
        match o.kind() {
            ObjKind::Map(m) => {
                let copy =
                    self.remember(o, Obj::new(Some(b.map.clone()), ObjKind::Map(RefCell::new(MapData::default()))));
                let items: Vec<(Value, Value)> = m.borrow().iter().cloned().collect();
                for (key, item) in items {
                    let (key, item) = (self.clone_value(&key)?, self.clone_value(&item)?);
                    if let ObjKind::Map(target) = copy.kind() {
                        target.borrow_mut().set(key, item);
                    }
                }
                return Ok(copy);
            }
            ObjKind::Set(s) => {
                let copy =
                    self.remember(o, Obj::new(Some(b.set.clone()), ObjKind::Set(RefCell::new(MapData::default()))));
                let items: Vec<Value> = s.borrow().iter().map(|(k, _)| k.clone()).collect();
                for item in items {
                    let item = self.clone_value(&item)?;
                    if let ObjKind::Set(target) = copy.kind() {
                        target.borrow_mut().set(item.clone(), item);
                    }
                }
                return Ok(copy);
            }
            ObjKind::Date(t) => {
                return Ok(self.remember(o, Obj::new(Some(b.date.clone()), ObjKind::Date(Cell::new(t.get())))));
            }
            ObjKind::RegExp(r) => {
                let copy = crate::stdlib::regexp::construct_regexp_literal(self.rt, &r.source(), &r.flags)?;
                return Ok(self.remember(o, copy));
            }
            ObjKind::Bytes(bytes) => {
                let copy = Obj::new(Some(b.uint8_array.clone()), ObjKind::Bytes(BytesData::new(bytes.to_vec())));
                return Ok(self.remember(o, copy));
            }
            ObjKind::Error(_) => {
                let name = get_str(o, "name")?;
                let message = get_str(o, "message")?;
                let error_type = match &name {
                    Value::String(s) => ErrorType::from_name(&s.to_rust()).unwrap_or(ErrorType::Error),
                    _ => ErrorType::Error,
                };
                let message = if message.is_undefined() { None } else { Some(coerce_to_string(&message).to_rust()) };
                let copy = self.remember(o, create_error_value(b.error(error_type), message.as_deref()));
                let stack = get_own(o, &PropKey::str("stack"))?;
                if let Value::String(_) = stack {
                    define(&copy, "stack", stack, HIDDEN);
                }
                if has_own(o, &PropKey::str("cause")) {
                    let cause = self.clone_value(&get_own(o, &PropKey::str("cause"))?)?;
                    define(&copy, "cause", cause, HIDDEN);
                }
                return Ok(copy);
            }
            _ => {}
        }
        if is_runtime_reference(value) {
            return Err(Self::uncloneable(value));
        }
        let copy =
            if o.is_array() { Obj::array_holes(&b.array, vec![None; o.array_len()]) } else { Obj::ordinary(&b.object) };
        let copy = self.remember(o, copy);
        for (key, item) in entries(o)? {
            let item = self.clone_value(&item)?;
            define(&copy, PropKey::Str(key), item, DATA);
        }
        Ok(copy)
    }
}

// HTML structured clone over CodeMode's data kinds. Shared references survive through the memo; prototypes and
// frozen state do not, as in JS. Without a DOMException, DataCloneError is a TypeError carrying that prefix.
pub fn structured_clone_global(rt: &Rt) -> Value {
    Value::Object(fn_(&rt.builtins.function, "structuredClone", 1, |rt, _, _, args| {
        if args.is_empty() {
            return NResult::err(type_error("structuredClone requires 1 argument"));
        }
        let mut cloner = Cloner { rt, memo: HashMap::new(), depth: Cell::new(0) };
        NResult::from(cloner.clone_value(&args[0]))
    }))
}

fn random_uuid() -> String {
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    bytes[6] = (bytes[6] & 0x0F) | 0x40;
    bytes[8] = (bytes[8] & 0x3F) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32])
}

pub fn crypto_global(rt: &Rt) -> Value {
    let object = Obj::ordinary(&rt.builtins.object);
    methods(
        &rt.builtins.function,
        &object,
        vec![
            method("randomUUID", 0, |_, _, _, _| NResult::ok(Value::from(random_uuid()))),
            method("getRandomValues", 1, |_, _, _, args| {
                let target = arg(&args, 0);
                let Some(view) = target.as_object().and_then(|o| o.bytes()) else {
                    return NResult::err(type_error(format!(
                        "crypto.getRandomValues expects a Uint8Array, received {}.",
                        describe_value(&target)
                    )));
                };
                let mut bytes = vec![0u8; view.length];
                rand::rng().fill_bytes(&mut bytes);
                for (i, b) in bytes.into_iter().enumerate() {
                    view.set(i, b);
                }
                NResult::ok(target.clone())
            }),
        ],
    );
    Value::Object(object)
}
