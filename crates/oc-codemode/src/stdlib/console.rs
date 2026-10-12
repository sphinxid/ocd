//! Captured console: every method appends one formatted line to the logs. Port of `stdlib/console.ts`.

use std::collections::HashSet;

use crate::data::quote_units;
use crate::interp::coerce::{coerce_to_string, obj_to_string};
use crate::interp::function::{method, methods, NResult};
use crate::interp::references::contains_opaque_reference;
use crate::interp::Rt;
use crate::object::{entries, get, Obj, ObjKind};
use crate::value::{PropKey, Value};

const METHODS: [&str; 7] = ["log", "info", "debug", "warn", "error", "dir", "table"];

pub fn console_global(rt: &Rt) -> Value {
    let console = Obj::ordinary(&rt.builtins.object);
    methods(
        &rt.builtins.function,
        &console,
        METHODS
            .iter()
            .map(|&name| {
                method(name, 0, move |rt, _, _, args| {
                    let line = format_console_message(name, &args);
                    rt.logs.borrow_mut().push(line);
                    NResult::ok(Value::Undefined)
                })
            })
            .collect(),
    );
    Value::Object(console)
}

const MAX_CONSOLE_DEPTH: usize = 32;

fn format_console_message(name: &str, args: &[Value]) -> String {
    if name == "dir" {
        return if args.is_empty() { "undefined".into() } else { format_value(&args[0]) };
    }
    if name == "table" {
        return format_console_table(args.first().cloned().unwrap_or_default(), args.get(1).cloned().unwrap_or_default());
    }
    let prefix = match name {
        "warn" => "[warn] ",
        "error" => "[error] ",
        "debug" => "[debug] ",
        _ => "",
    };
    format!("{prefix}{}", args.iter().map(format_value).collect::<Vec<_>>().join(" "))
}

/// One value as `console.log` shows it.
pub fn format_value(value: &Value) -> String {
    match value {
        Value::Undefined => "undefined".into(),
        Value::String(s) => s.to_rust(),
        other => format_console_value(other, &mut HashSet::new(), 0),
    }
}

fn json_string(s: &crate::value::JsStr) -> String {
    let mut out = String::new();
    quote_units(s.units(), &mut out);
    out
}

fn format_console_value(value: &Value, seen: &mut HashSet<usize>, depth: usize) -> String {
    let o = match value {
        Value::Undefined | Value::Null => return "null".into(),
        Value::String(s) => return json_string(s),
        Value::Tool(_) => return "[opaque reference]".into(),
        Value::Object(o) => o.clone(),
        other => return coerce_to_string(other).to_rust(),
    };
    if depth > MAX_CONSOLE_DEPTH {
        return "...".into();
    }
    if !seen.insert(o.addr()) {
        return "[Circular]".into();
    }
    let out = inspect(&o, &mut |v: &Value, seen: &mut HashSet<usize>| format_console_value(v, seen, depth + 1), seen);
    seen.remove(&o.addr());
    out
}

fn inspect(o: &Obj, item: &mut dyn FnMut(&Value, &mut HashSet<usize>) -> String, seen: &mut HashSet<usize>) -> String {
    match o.kind() {
        ObjKind::Array(a) => {
            let items = a.borrow().items.clone();
            let parts: Vec<String> = items.iter().map(|v| v.as_ref().map(|v| item(v, seen)).unwrap_or_default()).collect();
            format!("[{}]", parts.join(","))
        }
        ObjKind::Error(_) => obj_to_string(o).to_rust(),
        ObjKind::Promise(_) => "[Promise (await it to get its value)]".into(),
        ObjKind::Function(_) | ObjKind::Generator(_) | ObjKind::Iterator(_) => "[opaque reference]".into(),
        ObjKind::Map(m) => {
            let entries: Vec<(Value, Value)> = m.borrow().iter().cloned().collect();
            let parts: Vec<String> = entries.iter().map(|(k, v)| format!("[{},{}]", item(k, seen), item(v, seen))).collect();
            format!("Map({}) [{}]", entries.len(), parts.join(","))
        }
        ObjKind::Set(s) => {
            let values: Vec<Value> = s.borrow().iter().map(|(k, _)| k.clone()).collect();
            let parts: Vec<String> = values.iter().map(|v| item(v, seen)).collect();
            format!("Set({}) [{}]", values.len(), parts.join(","))
        }
        ObjKind::WeakMap(_) => "WeakMap { <items unknown> }".into(),
        ObjKind::WeakSet(_) => "WeakSet { <items unknown> }".into(),
        ObjKind::Headers(h) => {
            let mut map = serde_json::Map::new();
            for (k, v) in h.borrow().combined() {
                map.insert(k, serde_json::Value::String(v));
            }
            format!("Headers {}", crate::data::stringify(&serde_json::Value::Object(map), ""))
        }
        ObjKind::Bytes(b) => {
            let bytes = b.to_vec();
            format!("Uint8Array({}) [{}]", bytes.len(), bytes.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(","))
        }
        ObjKind::Date(_) | ObjKind::RegExp(_) | ObjKind::URLSearchParams(_) | ObjKind::URL(_) => obj_to_string(o).to_rust(),
        ObjKind::Ordinary | ObjKind::Arguments => {
            let list = entries(o).unwrap_or_default();
            let parts: Vec<String> = list.iter().map(|(k, v)| format!("{}:{}", json_string(k), item(v, seen))).collect();
            format!("{{{}}}", parts.join(","))
        }
    }
}

fn format_console_table(value: Value, columns_argument: Value) -> String {
    if value.is_undefined() {
        return "undefined".into();
    }
    if contains_opaque_reference(&value) {
        return "[opaque reference]".into();
    }
    let columns: Option<Vec<String>> = match &columns_argument {
        Value::Object(o) if o.is_array() => Some(o.array_values().iter().map(|v| coerce_to_string(v).to_rust()).collect()),
        _ => None,
    };
    let rows = table_rows(&value, columns.as_deref());
    let keys: Vec<String> = match &columns {
        Some(c) => c.clone(),
        None => {
            let mut keys: Vec<String> = Vec::new();
            for (_, values) in &rows {
                for (k, _) in values {
                    if !keys.contains(k) {
                        keys.push(k.clone());
                    }
                }
            }
            keys
        }
    };
    let mut lines = vec![std::iter::once("(index)".to_string()).chain(keys.iter().cloned()).collect::<Vec<_>>().join("\t")];
    for (index, values) in rows {
        let mut cells = vec![index];
        for key in &keys {
            let v = values.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone()).unwrap_or_default();
            cells.push(match &v {
                Value::Undefined => String::new(),
                Value::String(s) => s.to_rust(),
                other => format_console_value(other, &mut HashSet::new(), 0),
            });
        }
        lines.push(cells.join("\t"));
    }
    lines.join("\n")
}

type Row = (String, Vec<(String, Value)>);

fn table_rows(data: &Value, columns: Option<&[String]>) -> Vec<Row> {
    match data {
        Value::Object(o) if o.is_array() => {
            o.array_values().iter().enumerate().map(|(i, item)| (i.to_string(), table_values(item, columns))).collect()
        }
        Value::Object(o) => entries(o)
            .unwrap_or_default()
            .into_iter()
            .map(|(k, v)| (k.to_rust(), table_values(&v, columns)))
            .collect(),
        other => vec![("0".into(), vec![("Value".into(), other.clone())])],
    }
}

fn table_values(value: &Value, columns: Option<&[String]>) -> Vec<(String, Value)> {
    match value {
        Value::Object(o) if !o.is_array() => {
            let mut out: Vec<(String, Value)> = match columns {
                Some(cols) => cols.iter().map(|c| (c.clone(), get(o, &PropKey::str(c)).unwrap_or_default())).collect(),
                None => entries(o).unwrap_or_default().into_iter().map(|(k, v)| (k.to_rust(), v)).collect(),
            };
            // Object.fromEntries semantics: integer-like keys first.
            let (mut ints, rest): (Vec<_>, Vec<_>) =
                out.drain(..).partition(|(k, _)| crate::object::parse_array_index(&crate::value::JsStr::from(k.as_str())).is_some());
            ints.sort_by_key(|(k, _)| k.parse::<u64>().unwrap_or(0));
            let mut dedup: Vec<(String, Value)> = Vec::new();
            for (k, v) in ints.into_iter().chain(rest) {
                if let Some(e) = dedup.iter_mut().find(|(x, _)| *x == k) {
                    e.1 = v;
                } else {
                    dedup.push((k, v));
                }
            }
            dedup
        }
        other => vec![("Value".into(), other.clone())],
    }
}
