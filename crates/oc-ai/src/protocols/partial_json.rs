//! Lenient partial-JSON parser for advisory `tool-input-delta.input` previews
//! (`protocols/utils/partial-json.ts`, a port of promplate `partial-json` with `Allow.ALL`).

use serde_json::{Map, Number, Value};

/// Best-effort parse; failure yields `{}`.
pub fn parse(input: &str) -> Value {
    if let Ok(v) = serde_json::from_str::<Value>(input) {
        return v;
    }
    let repaired = repair(input);
    if let Ok(v) = serde_json::from_str::<Value>(&repaired) {
        return v;
    }
    let mut parser = Parser { chars: repaired.chars().collect(), pos: 0 };
    parser.skip_ws();
    match parser.value() {
        Some(v) => v,
        None => Value::Object(Map::new()),
    }
}

/// Inside strings, re-escape invalid backslash escapes and raw control characters.
fn repair(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_string = false;
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if !in_string {
            if c == '"' {
                in_string = true;
            }
            out.push(c);
            continue;
        }
        match c {
            '"' => {
                in_string = false;
                out.push(c);
            }
            '\\' => match chars.peek() {
                Some('"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't' | 'u') => {
                    out.push(c);
                    out.push(chars.next().unwrap());
                }
                None => out.push(c),
                Some(_) => out.push_str("\\\\"),
            },
            c if (c as u32) <= 0x1f => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }
    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(c) if c.is_whitespace()) {
            self.pos += 1;
        }
    }
    fn value(&mut self) -> Option<Value> {
        self.skip_ws();
        match self.peek()? {
            '{' => Some(self.object()),
            '[' => Some(self.array()),
            '"' => Some(Value::String(self.string())),
            't' => self.literal("true", Value::Bool(true)),
            'f' => self.literal("false", Value::Bool(false)),
            'n' => self.literal("null", Value::Null),
            'I' => self.literal("Infinity", Value::Null),
            'N' => self.literal("NaN", Value::Null),
            c if c == '-' || c.is_ascii_digit() => self.number(),
            _ => None,
        }
    }
    fn literal(&mut self, word: &str, value: Value) -> Option<Value> {
        let rest: String = self.chars[self.pos..].iter().take(word.len()).collect();
        if word.starts_with(&rest) && !rest.is_empty() {
            self.pos += rest.chars().count();
            Some(value)
        } else {
            None
        }
    }
    fn number(&mut self) -> Option<Value> {
        let start = self.pos;
        while matches!(self.peek(), Some(c) if c.is_ascii_digit() || matches!(c, '-' | '+' | '.' | 'e' | 'E')) {
            self.pos += 1;
        }
        let mut text: String = self.chars[start..self.pos].iter().collect();
        loop {
            if text.is_empty() || text == "-" {
                return None;
            }
            if let Ok(i) = text.parse::<i64>() {
                return Some(Value::Number(Number::from(i)));
            }
            if let Ok(f) = text.parse::<f64>() {
                return Number::from_f64(f).map(Value::Number);
            }
            text.pop();
        }
    }
    fn string(&mut self) -> String {
        // assumes current char is '"'
        self.pos += 1;
        let mut out = String::new();
        while let Some(c) = self.peek() {
            self.pos += 1;
            match c {
                '"' => return out,
                '\\' => {
                    let Some(e) = self.peek() else { return out };
                    self.pos += 1;
                    match e {
                        'n' => out.push('\n'),
                        't' => out.push('\t'),
                        'r' => out.push('\r'),
                        'b' => out.push('\u{8}'),
                        'f' => out.push('\u{c}'),
                        'u' => {
                            let hex: String = self.chars[self.pos..].iter().take(4).collect();
                            if hex.len() < 4 {
                                self.pos = self.chars.len();
                                return out;
                            }
                            self.pos += 4;
                            if let Some(ch) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                                out.push(ch);
                            }
                        }
                        other => out.push(other),
                    }
                }
                c => out.push(c),
            }
        }
        out
    }
    fn object(&mut self) -> Value {
        self.pos += 1;
        let mut map = Map::new();
        loop {
            self.skip_ws();
            match self.peek() {
                None => return Value::Object(map),
                Some('}') => {
                    self.pos += 1;
                    return Value::Object(map);
                }
                Some(',') => {
                    self.pos += 1;
                    continue;
                }
                Some('"') => {
                    let key = self.string();
                    self.skip_ws();
                    if self.peek() != Some(':') {
                        return Value::Object(map);
                    }
                    self.pos += 1;
                    self.skip_ws();
                    match self.value() {
                        Some(v) => {
                            map.insert(key, v);
                        }
                        None => return Value::Object(map),
                    }
                }
                Some(_) => return Value::Object(map),
            }
        }
    }
    fn array(&mut self) -> Value {
        self.pos += 1;
        let mut items = Vec::new();
        loop {
            self.skip_ws();
            match self.peek() {
                None => return Value::Array(items),
                Some(']') => {
                    self.pos += 1;
                    return Value::Array(items);
                }
                Some(',') => {
                    self.pos += 1;
                    continue;
                }
                Some(_) => match self.value() {
                    Some(v) => items.push(v),
                    None => return Value::Array(items),
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn partials() {
        assert_eq!(parse(r#"{"a":1}"#), json!({"a":1}));
        assert_eq!(parse(r#"{"path":"/tm"#), json!({"path":"/tm"}));
        assert_eq!(parse(r#"{"a":[1,2"#), json!({"a":[1,2]}));
        assert_eq!(parse(r#"{"a":tr"#), json!({"a":true}));
        assert_eq!(parse(""), json!({}));
        assert_eq!(parse(r#"{"a":"x\q"}"#), json!({"a":"x\\q"}));
    }
}
