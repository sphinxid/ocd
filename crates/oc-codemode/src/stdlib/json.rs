//! `JSON.parse` / `JSON.stringify`. Port of `stdlib/json.ts`; parse errors follow JavaScriptCore's messages (the
//! upstream host engine).

use std::rc::Rc;

use serde_json::{Map, Value as Json};

use crate::data::{quote, to_json};
use crate::interp::callback::apply_collection_callback;
use crate::interp::coerce::coerce_to_string;
use crate::interp::errors::{syntax_error, JsResult};
use crate::interp::function::{arg, method, methods, NResult};
use crate::interp::limits::check_string_length;
use crate::interp::{Rt, Site};
use crate::object::{define_data, get, keys, record, remove, set, Obj};
use crate::value::{JsStr, PropKey, Value};

pub fn json_global(rt: &Rt) -> Value {
    let json = Obj::ordinary(&rt.builtins.object);
    methods(
        &rt.builtins.function,
        &json,
        vec![
            method("parse", 2, |rt, site, _, args| parse(rt, site, args)),
            method("stringify", 3, |rt, site, _, args| stringify(rt, site, args)),
        ],
    );
    Value::Object(json)
}

/// A parsed JSON value (keys in source order; strings as UTF-16).
#[derive(Clone, Debug)]
pub enum Parsed {
    Null,
    Bool(bool),
    Number(f64),
    String(JsStr),
    Array(Vec<Parsed>),
    Object(Vec<(JsStr, Parsed)>),
}

impl Parsed {
    pub fn to_value(&self, rt: &Rt) -> Value {
        match self {
            Parsed::Null => Value::Null,
            Parsed::Bool(b) => Value::Bool(*b),
            Parsed::Number(n) => Value::Number(*n),
            Parsed::String(s) => Value::String(s.clone()),
            Parsed::Array(items) => {
                Value::Object(Obj::array(&rt.builtins.array, items.iter().map(|i| i.to_value(rt)).collect()))
            }
            Parsed::Object(fields) => {
                let out = Obj::ordinary(&rt.builtins.object);
                for (k, v) in fields {
                    define_data(&out, k.clone(), v.to_value(rt));
                }
                Value::Object(out)
            }
        }
    }
}

struct Parser<'a> {
    s: &'a [u16],
    i: usize,
}

type PResult<T> = Result<T, String>;

impl Parser<'_> {
    fn ws(&mut self) {
        while self.i < self.s.len() && matches!(self.s[self.i], 0x20 | 0x09 | 0x0A | 0x0D) {
            self.i += 1;
        }
    }
    fn peek(&self) -> Option<u16> {
        self.s.get(self.i).copied()
    }
    fn token_error(&self) -> String {
        match self.peek() {
            None => "Unexpected EOF".into(),
            Some(c) => {
                let ch = char::from_u32(c as u32).unwrap_or('\u{FFFD}');
                if ch.is_ascii_alphabetic() || ch == '_' || ch == '$' {
                    let mut j = self.i;
                    let mut word = String::new();
                    while j < self.s.len() {
                        let c = char::from_u32(self.s[j] as u32).unwrap_or(' ');
                        if c.is_ascii_alphanumeric() || c == '_' || c == '$' {
                            word.push(c);
                            j += 1;
                        } else {
                            break;
                        }
                    }
                    format!("Unexpected identifier \"{word}\"")
                } else if ch == '\'' {
                    "Single quotes (') are not allowed in JSON".into()
                } else if matches!(ch, ',' | '}' | ']' | ':' | '.' | '[' | '{') {
                    format!("Unexpected token '{ch}'")
                } else {
                    format!("Unrecognized token '{ch}'")
                }
            }
        }
    }
    fn value(&mut self) -> PResult<Parsed> {
        self.ws();
        let Some(c) = self.peek() else { return Err("Unexpected EOF".into()) };
        match c {
            0x7B => self.object(),
            0x5B => self.array(),
            0x22 => Ok(Parsed::String(self.string()?)),
            0x2D | 0x30..=0x39 => self.number(),
            _ => {
                for (word, value) in [("true", Parsed::Bool(true)), ("false", Parsed::Bool(false)), ("null", Parsed::Null)] {
                    let w: Vec<u16> = word.encode_utf16().collect();
                    if self.s[self.i..].starts_with(&w) {
                        let after = self.s.get(self.i + w.len()).copied();
                        let ident = after.map(|c| (c as u8 as char).is_ascii_alphanumeric() && c < 128).unwrap_or(false);
                        if !ident {
                            self.i += w.len();
                            return Ok(value);
                        }
                    }
                }
                Err(self.token_error())
            }
        }
    }
    fn number(&mut self) -> PResult<Parsed> {
        let start = self.i;
        if self.peek() == Some(0x2D) {
            self.i += 1;
        }
        let digits = |p: &mut Self| {
            let s = p.i;
            while matches!(p.peek(), Some(0x30..=0x39)) {
                p.i += 1;
            }
            p.i - s
        };
        if self.peek() == Some(0x30) {
            self.i += 1;
        } else if digits(self) == 0 {
            return Err("Invalid number".into());
        }
        if self.peek() == Some(0x2E) {
            self.i += 1;
            if digits(self) == 0 {
                return Err("Invalid digits after decimal point".into());
            }
        }
        if matches!(self.peek(), Some(0x65 | 0x45)) {
            let save = self.i;
            self.i += 1;
            if matches!(self.peek(), Some(0x2B | 0x2D)) {
                self.i += 1;
            }
            if digits(self) == 0 {
                self.i = save;
                return Err("Unable to parse JSON string".into());
            }
        }
        let text = String::from_utf16_lossy(&self.s[start..self.i]);
        Ok(Parsed::Number(text.parse::<f64>().unwrap_or(f64::NAN)))
    }
    fn string(&mut self) -> PResult<JsStr> {
        self.i += 1;
        let mut out: Vec<u16> = Vec::new();
        loop {
            let Some(c) = self.peek() else { return Err("Unterminated string".into()) };
            self.i += 1;
            match c {
                0x22 => return Ok(JsStr::from_units(out)),
                0x5C => {
                    let Some(e) = self.peek() else { return Err("Unterminated string".into()) };
                    self.i += 1;
                    match e {
                        0x22 => out.push(0x22),
                        0x5C => out.push(0x5C),
                        0x2F => out.push(0x2F),
                        0x62 => out.push(0x08),
                        0x66 => out.push(0x0C),
                        0x6E => out.push(0x0A),
                        0x72 => out.push(0x0D),
                        0x74 => out.push(0x09),
                        0x75 => {
                            let hex: Vec<u16> = self.s[self.i..(self.i + 4).min(self.s.len())].to_vec();
                            if hex.len() < 4 {
                                return Err("\\u must be followed by 4 hex digits".into());
                            }
                            let text = String::from_utf16_lossy(&hex);
                            match u16::from_str_radix(&text, 16) {
                                Ok(v) if text.chars().all(|c| c.is_ascii_hexdigit()) => {
                                    out.push(v);
                                    self.i += 4;
                                }
                                _ => return Err(format!("\"\\u{text}\" is not a valid unicode escape")),
                            }
                        }
                        other => {
                            return Err(format!(
                                "Invalid escape character {}",
                                char::from_u32(other as u32).unwrap_or('\u{FFFD}')
                            ));
                        }
                    }
                }
                c if c < 0x20 => return Err("Unterminated string".into()),
                c => out.push(c),
            }
        }
    }
    fn array(&mut self) -> PResult<Parsed> {
        self.i += 1;
        let mut items = Vec::new();
        self.ws();
        if self.peek() == Some(0x5D) {
            self.i += 1;
            return Ok(Parsed::Array(items));
        }
        loop {
            items.push(self.value()?);
            self.ws();
            match self.peek() {
                Some(0x2C) => {
                    self.i += 1;
                    self.ws();
                    if self.peek() == Some(0x5D) {
                        return Err("Unexpected comma at the end of array expression".into());
                    }
                }
                Some(0x5D) => {
                    self.i += 1;
                    return Ok(Parsed::Array(items));
                }
                None => return Err("Unexpected EOF".into()),
                _ => return Err("Expected ']'".into()),
            }
        }
    }
    fn object(&mut self) -> PResult<Parsed> {
        self.i += 1;
        let mut out: Vec<(JsStr, Parsed)> = Vec::new();
        self.ws();
        if self.peek() == Some(0x7D) {
            self.i += 1;
            return Ok(Parsed::Object(out));
        }
        if self.peek() != Some(0x22) {
            return Err("Expected '}'".into());
        }
        loop {
            let key = self.string()?;
            self.ws();
            if self.peek() != Some(0x3A) {
                return Err("Expected ':' before value in object property definition".into());
            }
            self.i += 1;
            let value = self.value()?;
            match out.iter_mut().find(|(k, _)| *k == key) {
                Some(e) => e.1 = value,
                None => out.push((key, value)),
            }
            self.ws();
            match self.peek() {
                Some(0x2C) => {
                    self.i += 1;
                    self.ws();
                    if self.peek() != Some(0x22) {
                        return Err("Property name must be a string literal".into());
                    }
                }
                Some(0x7D) => {
                    self.i += 1;
                    return Ok(Parsed::Object(out));
                }
                None => return Err("Unexpected EOF".into()),
                _ => return Err("Expected '}'".into()),
            }
        }
    }
}

/// Parses JSON text with JavaScriptCore's error messages.
pub fn parse_json(text: &JsStr) -> Result<Parsed, String> {
    let mut p = Parser { s: text.units(), i: 0 };
    let value = p.value().map_err(|m| format!("JSON Parse error: {m}"))?;
    p.ws();
    if p.i < p.s.len() {
        return Err("JSON Parse error: Unable to parse JSON string".into());
    }
    Ok(value)
}

fn parse(rt: &Rt, site: Site, args: Vec<Value>) -> NResult {
    let text = coerce_to_string(&arg(&args, 0));
    let parsed = match parse_json(&text) {
        Ok(v) => v.to_value(rt),
        Err(m) => return NResult::err(syntax_error(format!("JSON.parse received invalid JSON: {m}"))),
    };
    let reviver = arg(&args, 1);
    if reviver.js_typeof() != "function" {
        return NResult::ok(parsed);
    }
    let apply = match apply_collection_callback(&reviver, "JSON.parse") {
        Ok(a) => Rc::new(a),
        Err(e) => return NResult::err(e),
    };
    let rt = rt.clone();
    NResult::future(async move {
        let holder = record(&rt.builtins.object, vec![("", parsed)]);
        revive(&rt, &apply, holder, JsStr::from(""), site).await
    })
}

fn revive<'a>(
    rt: &'a Rt,
    apply: &'a Rc<crate::interp::callback::Callback>,
    holder: Obj,
    key: JsStr,
    site: Site,
) -> futures::future::LocalBoxFuture<'a, JsResult<Value>> {
    Box::pin(async move {
        let value = get(&holder, &PropKey::Str(key.clone()))?;
        if let Value::Object(o) = &value {
            for name in keys(o) {
                let revived = revive(rt, apply, o.clone(), name.clone(), site).await?;
                if revived.is_undefined() {
                    remove(o, &PropKey::Str(name));
                } else {
                    set(o, &PropKey::Str(name), revived)?;
                }
            }
        }
        apply(rt, vec![Value::String(key), value], Value::Object(holder), site).await
    })
}

fn indent_of(space: &Value) -> String {
    match space {
        Value::Number(n) => {
            let n = crate::number::to_integer_or_infinity(*n).clamp(0.0, 10.0) as usize;
            " ".repeat(n)
        }
        Value::String(s) => String::from_utf16_lossy(&s.units()[..s.len().min(10)]),
        _ => String::new(),
    }
}

fn filter_properties(value: Json, properties: &[String]) -> Json {
    match value {
        Json::Object(map) => {
            let mut out = Map::new();
            for p in properties {
                if let Some(v) = map.get(p) {
                    out.insert(p.clone(), filter_properties(v.clone(), properties));
                }
            }
            Json::Object(out)
        }
        Json::Array(items) => Json::Array(items.into_iter().map(|v| filter_properties(v, properties)).collect()),
        other => other,
    }
}

fn stringify(rt: &Rt, site: Site, args: Vec<Value>) -> NResult {
    let space = arg(&args, 2);
    let replacer = arg(&args, 1);
    let properties: Option<Vec<String>> = match &replacer {
        Value::Object(o) if o.is_array() => {
            let mut out: Vec<String> = Vec::new();
            for item in o.array_values() {
                let name = match &item {
                    Value::String(s) => s.to_rust(),
                    Value::Number(n) => crate::number::to_string(*n),
                    _ => continue,
                };
                if !out.contains(&name) {
                    out.push(name);
                }
            }
            Some(out)
        }
        _ => None,
    };
    let callback = if replacer.js_typeof() == "function" {
        match apply_collection_callback(&replacer, "JSON.stringify") {
            Ok(c) => Some(c),
            Err(e) => return NResult::err(e),
        }
    } else {
        None
    };
    let rt = rt.clone();
    NResult::future(async move {
        let json = to_json(&rt, arg(&args, 0), callback.as_ref(), site).await?;
        let Some(json) = json else { return Ok(Value::Undefined) };
        let json = match &properties {
            Some(p) => filter_properties(json, p),
            None => json,
        };
        let text = crate::data::stringify(&json, &indent_of(&space));
        let units: Vec<u16> = text.encode_utf16().collect();
        check_string_length(units.len())?;
        Ok(Value::String(JsStr::from_units(units)))
    })
}

#[allow(dead_code)]
fn _q(s: &str) -> String {
    quote(s)
}
