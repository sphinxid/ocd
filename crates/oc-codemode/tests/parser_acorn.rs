//! Compares the acorn port against acorn 8.15.0 itself.
//!
//! Expected results come from `tests/fixtures/parser/gen.js` (run with node; see its header):
//! - `snippets.json`: hand-written sources with acorn's full normalized ESTree or error, compared exactly;
//! - `test262.json`: every `.js` file of upstream codemode's vendored test262 subset, as an FNV-1a 64 hash of the
//!   canonical normalized ESTree or the error. The test262 files are not committed; the test reads them from
//!   `$OC_TEST262_DIR` (or the default checkout path below) and skips when they are absent.

use oc_codemode::parser::{ParseError, parse, to_estree};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

const FALLBACK_FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/parser");
const STACK: usize = 256 << 20;

fn fixtures_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("OC_PARSER_FIXTURES") {
        return PathBuf::from(dir);
    }
    let local = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/parser");
    if local.join("snippets.json").exists() { local } else { PathBuf::from(FALLBACK_FIXTURES) }
}

fn error_json(e: &ParseError) -> Value {
    json!({ "message": e.message, "pos": e.pos, "line": e.line, "column": e.column })
}

/// The parse result in the fixture's shape: `{"ast": ...}` or `{"error": {...}}`, or `{"panic": ...}`.
fn run(src: &str) -> Value {
    let src = src.to_owned();
    let result = std::panic::catch_unwind(move || match parse(&src) {
        Ok(program) => json!({ "ast": to_estree(&program) }),
        Err(e) => json!({ "error": error_json(&e) }),
    });
    match result {
        Ok(v) => v,
        Err(p) => {
            let msg = p
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default();
            json!({ "panic": msg })
        }
    }
}

/// Canonical JSON (sorted keys, minimal escaping) — mirrors `canonical` in gen.js.
fn canonical(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => canonical_str(s, out),
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonical(x, out);
            }
            out.push(']');
        }
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonical_str(k, out);
                out.push(':');
                canonical(&m[k], out);
            }
            out.push('}');
        }
    }
}

fn canonical_str(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn fnv1a64(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// Path and short description of the first difference between two JSON values.
fn first_diff(expected: &Value, actual: &Value, path: &str) -> Option<String> {
    match (expected, actual) {
        (Value::Object(a), Value::Object(b)) => {
            for (k, va) in a {
                match b.get(k) {
                    None => return Some(format!("{path}.{k}: missing (expected {})", short(va))),
                    Some(vb) => {
                        if let Some(d) = first_diff(va, vb, &format!("{path}.{k}")) {
                            return Some(d);
                        }
                    }
                }
            }
            b.keys().find(|k| !a.contains_key(*k)).map(|k| format!("{path}.{k}: unexpected {}", short(&b[k])))
        }
        (Value::Array(a), Value::Array(b)) => {
            for (i, (va, vb)) in a.iter().zip(b).enumerate() {
                if let Some(d) = first_diff(va, vb, &format!("{path}[{i}]")) {
                    return Some(d);
                }
            }
            if a.len() != b.len() {
                return Some(format!("{path}: length {} vs {}", a.len(), b.len()));
            }
            None
        }
        _ if expected == actual => None,
        _ => Some(format!("{path}: expected {} got {}", short(expected), short(actual))),
    }
}

fn short(v: &Value) -> String {
    let s = v.to_string();
    if s.chars().count() > 300 { s.chars().take(300).collect::<String>() + "..." } else { s }
}

fn on_big_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new().stack_size(STACK).spawn(f).unwrap().join().unwrap()
}

fn report(kind: &str, total: usize, failures: &[String]) {
    for f in failures.iter().take(30) {
        eprintln!("{f}\n");
    }
    if !failures.is_empty() {
        panic!("{kind}: {} of {total} entries differ from acorn", failures.len());
    }
    eprintln!("{kind}: all {total} entries match acorn");
}

#[test]
fn snippets_match_acorn() {
    let path = fixtures_dir().join("snippets.json");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    let entries: Vec<Value> = serde_json::from_str(&text).unwrap();
    let total = entries.len();
    let failures = on_big_stack(move || {
        let mut failures = Vec::new();
        for entry in &entries {
            let src = entry["src"].as_str().unwrap();
            let expected = &entry["expected"];
            let actual = run(src);
            if &actual != expected {
                let diff = first_diff(expected, &actual, "$").unwrap_or_default();
                failures.push(format!("source: {src:?}\n  diff: {diff}"));
            }
        }
        failures
    });
    report("snippets", total, &failures);
}

fn collect_js(dir: &Path, root: &Path, out: &mut Vec<String>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap()).collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let p = e.path();
        let ty = e.file_type().unwrap();
        if ty.is_dir() {
            collect_js(&p, root, out);
        } else if ty.is_file() && p.extension().is_some_and(|x| x == "js") {
            let rel = p.strip_prefix(root).unwrap();
            out.push(rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/"));
        }
    }
}

fn check_test262_file(root: &Path, rel: &str, expected: &Value) -> Option<String> {
    let src = match std::fs::read_to_string(root.join(rel)) {
        Ok(s) => s,
        Err(e) => return Some(format!("{rel}: cannot read: {e}")),
    };
    let actual = run(&src);
    match expected {
        Value::String(hash) => {
            if actual.get("ast").is_none() {
                return Some(format!("{rel}: acorn accepts, got {}", short(&actual)));
            }
            let mut c = String::new();
            canonical(&actual, &mut c);
            let h = fnv1a64(c.as_bytes());
            if &h != hash { Some(format!("{rel}: AST differs from acorn (hash {h} vs {hash})")) } else { None }
        }
        Value::Object(_) => {
            let want = json!({ "error": expected });
            if actual != want {
                Some(format!("{rel}: expected error {} got {}", short(expected), short(&actual)))
            } else {
                None
            }
        }
        _ => Some(format!("{rel}: bad fixture entry {}", short(expected))),
    }
}

#[test]
fn test262_matches_acorn() {
    let root = PathBuf::from(std::env::var("OC_TEST262_DIR").unwrap_or_else(|_| {
        let scratch = std::env::var("OCD_SCRATCH").unwrap_or_default();
        format!("{scratch}/src/opencode/packages/codemode/test/test262")
    }));
    if !root.join("language").is_dir() && !root.join("built-ins").is_dir() {
        eprintln!("test262 files not found at {} (set OC_TEST262_DIR or OCD_SCRATCH); skipping", root.display());
        return;
    }
    let path = fixtures_dir().join("test262.json");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    let fixture: Map<String, Value> = serde_json::from_str(&text).unwrap();
    let mut files = Vec::new();
    collect_js(&root, &root, &mut files);
    let mut failures: Vec<String> = files
        .iter()
        .filter(|f| !fixture.contains_key(*f))
        .map(|f| format!("{f}: not in fixture (regenerate with gen.js)"))
        .collect();
    let on_disk: std::collections::HashSet<&String> = files.iter().collect();
    failures.extend(
        fixture.keys().filter(|k| !on_disk.contains(k)).map(|k| format!("{k}: in fixture but missing on disk")),
    );

    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(16);
    let jobs: Vec<(String, Value)> =
        files.iter().filter_map(|f| fixture.get(f).map(|v| (f.clone(), v.clone()))).collect();
    let total = jobs.len();
    let chunk = jobs.len().div_ceil(threads).max(1);
    let root = std::sync::Arc::new(root);
    let handles: Vec<_> = jobs
        .chunks(chunk)
        .map(|c| {
            let c = c.to_vec();
            let root = root.clone();
            std::thread::Builder::new()
                .stack_size(STACK)
                .spawn(move || c.iter().filter_map(|(f, v)| check_test262_file(&root, f, v)).collect::<Vec<_>>())
                .unwrap()
        })
        .collect();
    for h in handles {
        failures.extend(h.join().unwrap());
    }
    report("test262", total, &failures);
}
