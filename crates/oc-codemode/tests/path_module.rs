//! Node's `path` module as Bun 1.4.4 gives it (the host module upstream's `path` global wraps), called directly:
//! `tests/fixtures/path/path-calls.json` (8 100 calls of every function of both flavours with generated paths) and
//! `glob-calls.json` (`matchesGlob` on generated patterns), both produced by `tests/recorder/path-fuzz.ts` under
//! Bun 1.4.4 with its working directory at `/usr`.

use oc_codemode::host::HostValue;
use oc_codemode::path::{Flavour, Module};
use serde_json::Value as Json;

fn to_host(value: &Json) -> HostValue {
    match value {
        Json::Null => HostValue::Null,
        Json::Bool(b) => HostValue::Bool(*b),
        Json::Number(n) => HostValue::Number(n.as_f64().unwrap()),
        Json::String(s) => HostValue::String(s.clone()),
        Json::Array(items) => HostValue::Array(items.iter().map(to_host).collect()),
        Json::Object(fields) => HostValue::Object(fields.iter().map(|(k, v)| (k.clone(), to_host(v))).collect()),
    }
}

fn to_json(value: &HostValue) -> Json {
    match value {
        HostValue::Null | HostValue::Undefined => Json::Null,
        HostValue::Bool(b) => Json::Bool(*b),
        HostValue::Number(n) => serde_json::json!(n),
        HostValue::String(s) => Json::String(s.clone()),
        HostValue::Array(items) => Json::Array(items.iter().map(to_json).collect()),
        HostValue::Object(fields) => Json::Object(fields.iter().map(|(k, v)| (k.clone(), to_json(v))).collect()),
        HostValue::Error { name, message, .. } => Json::String(format!("{name}: {message}")),
        other => Json::String(format!("{other:?}")),
    }
}

fn replay(file: &str) {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/path").join(file);
    let cases: Vec<Json> = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let mut failures = Vec::new();
    for case in &cases {
        let flavour = if case[0] == "win32" { Flavour::Win32 } else { Flavour::Posix };
        let module = Module::new(flavour, "/usr");
        let name = case[1].as_str().unwrap();
        let args: Vec<HostValue> = case[2].as_array().unwrap().iter().map(to_host).collect();
        let ours = match module.call(name, &args) {
            Ok(v) => serde_json::json!({ "ok": to_json(&v) }),
            Err(e) => serde_json::json!({ "throw": to_json(&e) }),
        };
        if ours != case[3] {
            failures.push(format!("{} {name}({}): expected {}, got {ours}", case[0], case[2], case[3]));
        }
    }
    eprintln!("{file}: {} calls, {} differ", cases.len(), failures.len());
    assert!(failures.is_empty(), "{}", failures.iter().take(60).cloned().collect::<Vec<_>>().join("\n"));
}

#[test]
fn path_functions_match_bun() {
    replay("path-calls.json");
}

#[test]
fn matches_glob_matches_bun() {
    replay("glob-calls.json");
}
