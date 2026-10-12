//! Shared support for replaying upstream recordings (see `tests/fixtures/upstream/README.md`).
#![allow(dead_code)]

pub mod ext;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use oc_codemode::host::{CallResult, Host, ToolCallError, ToolInvocation};
use oc_codemode::tool_runtime::{ToolDef, ToolEntry};
use oc_codemode::tool_schema::ToolSchema;
use serde_json::{Map, Value as Json};

pub fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/upstream")
}

pub fn read_jsonl(path: &std::path::Path) -> Vec<Json> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(&sanitize_surrogates(l)).expect("jsonl"))
        .collect()
}

/// Replaces lone-surrogate `\uXXXX` escapes (valid in JS JSON, invalid for serde) with U+FFFD, which is also what
/// the Rust side produces when such strings cross to the host.
pub fn sanitize_surrogates(line: &str) -> String {
    let b = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    let hex_at = |i: usize| -> Option<u16> {
        if i + 6 <= b.len() && b[i] == b'\\' && b[i + 1] == b'u' {
            u16::from_str_radix(std::str::from_utf8(&b[i + 2..i + 6]).ok()?, 16).ok()
        } else {
            None
        }
    };
    while i < b.len() {
        if b[i] == b'\\' && i + 1 < b.len() && b[i + 1] == b'\\' {
            out.push_str("\\\\");
            i += 2;
            continue;
        }
        if let Some(u) = hex_at(i) {
            if (0xD800..0xDC00).contains(&u) {
                if let Some(low) = hex_at(i + 6)
                    && (0xDC00..0xE000).contains(&low)
                {
                    out.push_str(&line[i..i + 12]);
                    i += 12;
                    continue;
                }
                out.push_str("\\ufffd");
                i += 6;
                continue;
            }
            if (0xDC00..0xE000).contains(&u) {
                out.push_str("\\ufffd");
                i += 6;
                continue;
            }
        }
        let ch = line[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Decodes the recorder's markers into plain JSON (`$undefined` → None).
pub fn decode(value: &Json) -> Option<Json> {
    match value {
        Json::Object(m) if m.len() == 1 && m.contains_key("$undefined") => None,
        Json::Object(m) if m.len() == 1 && m.contains_key("$number") => Some(match m["$number"].as_str() {
            Some("-0") => Json::from(0),
            _ => Json::Null,
        }),
        Json::Object(m) => {
            let mut out = Map::new();
            for (k, v) in m {
                if let Some(v) = decode(v) {
                    out.insert(k.clone(), v);
                }
            }
            Some(Json::Object(out))
        }
        Json::Array(a) => Some(Json::Array(a.iter().map(|v| decode(v).unwrap_or(Json::Null)).collect())),
        other => Some(other.clone()),
    }
}

/// JSON equality with numbers compared as f64.
pub fn json_eq(a: &Json, b: &Json) -> bool {
    match (a, b) {
        (Json::Number(x), Json::Number(y)) => x.as_f64() == y.as_f64(),
        (Json::Array(x), Json::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| json_eq(p, q)),
        (Json::Object(x), Json::Object(y)) => {
            x.len() == y.len() && x.iter().zip(y.iter()).all(|((k1, v1), (k2, v2))| k1 == k2 && json_eq(v1, v2))
        }
        _ => a == b,
    }
}

pub fn schema_of(record: &Json) -> ToolSchema {
    match record["kind"].as_str() {
        Some("json") => ToolSchema::Json(record["schema"].clone()),
        _ => {
            let document = match &record["document"] {
                Json::Object(d) => Some((
                    d.get("schema").cloned().unwrap_or(Json::Null),
                    d.get("definitions").and_then(|x| x.as_object().cloned()).unwrap_or_default(),
                )),
                _ => None,
            };
            ToolSchema::Document { document, empty_document: record.get("emptyDocument").cloned() }
        }
    }
}

/// Flattens a recorded trie into declaration-ordered entries.
pub fn tree_entries(tree: &Json) -> Vec<ToolEntry> {
    fn walk(node: &Json, path: &mut Vec<String>, out: &mut Vec<ToolEntry>) {
        if let Some(ns) = node.get("namespace") {
            out.push(ToolEntry::Namespace {
                path: path.join("."),
                description: ns.get("description").and_then(|d| d.as_str()).map(String::from),
            });
        }
        if let Some(tool) = node.get("tool") {
            out.push(ToolEntry::Tool(ToolDef {
                path: path.join("."),
                description: tool["description"].as_str().unwrap_or("").to_string(),
                input: schema_of(&tool["input"]),
                output: if tool["output"].is_null() { None } else { Some(schema_of(&tool["output"])) },
            }));
        }
        if let Some(Json::Array(children)) = node.get("children") {
            for child in children {
                path.push(child[0].as_str().unwrap().to_string());
                walk(&child[1], path, out);
                path.pop();
            }
        }
    }
    let mut out = Vec::new();
    walk(tree, &mut Vec::new(), &mut out);
    out
}

/// A host that replays recorded tool behaviour.
pub struct ReplayHost {
    decodes: Mutex<VecDeque<(String, Json)>>,
    settles: Mutex<Vec<(String, Json, bool)>>,
    pub after: Mutex<Vec<(String, String)>>,
}

impl ReplayHost {
    pub fn new(record: &Json) -> ReplayHost {
        let events = record["events"].as_array().cloned().unwrap_or_default();
        let name_of = |id: &Json| {
            events
                .iter()
                .find(|e| e["type"] == "call" && &e["id"] == id)
                .and_then(|e| e["name"].as_str())
                .unwrap_or("")
                .to_string()
        };
        let mut decodes = VecDeque::new();
        let mut settles = Vec::new();
        for e in &events {
            match e["type"].as_str() {
                Some("decode") => decodes.push_back((name_of(&e["id"]), e.clone())),
                Some("settle") if e["toolRan"] == Json::Bool(true) => settles.push((name_of(&e["id"]), e.clone(), false)),
                _ => {}
            }
        }
        ReplayHost { decodes: Mutex::new(decodes), settles: Mutex::new(settles), after: Mutex::new(Vec::new()) }
    }
}

impl Host for ReplayHost {
    fn decode_input(&self, path: &str, input: Option<Json>) -> Result<Option<Json>, String> {
        let mut decodes = self.decodes.lock().unwrap();
        let Some(i) = decodes.iter().position(|(n, _)| n == path) else { return Ok(input) };
        let (_, e) = decodes.remove(i).unwrap();
        if e["ok"] == Json::Bool(true) {
            return Ok(decode(&e["input"]));
        }
        let message = e["error"].as_str().unwrap_or("");
        let prefix = format!("Invalid input for tool '{path}': ");
        Err(message.strip_prefix(&prefix).unwrap_or(message).to_string())
    }

    fn call(&self, path: &str, input: Option<Json>) -> BoxFuture<'static, Result<Option<Json>, ToolCallError>> {
        let event = {
            let mut settles = self.settles.lock().unwrap();
            let found = settles.iter().position(|(n, _, used)| n == path && !used);
            found.map(|i| {
                settles[i].2 = true;
                settles[i].1.clone()
            })
        };
        let _ = input;
        let path = path.to_string();
        Box::pin(async move {
            let Some(e) = event else { return Err(ToolCallError::Failure(format!("replay: no recorded call for {path}"))) };
            let sync = e["sync"] == Json::Bool(true);
            if !sync {
                tokio::time::sleep(std::time::Duration::from_millis(e["toolMs"].as_u64().unwrap_or(0).min(50) + 1)).await;
            }
            match e["outcome"].as_str() {
                Some("success") => Ok(decode(&e["value"])),
                Some("invalidOutput") => {
                    let message = e["error"].as_str().unwrap_or("");
                    let prefix = format!("Invalid output from tool '{path}': ");
                    Err(ToolCallError::InvalidOutput(message.strip_prefix(&prefix).unwrap_or(message).to_string()))
                }
                Some("interrupted") => futures::future::pending().await,
                _ => Err(ToolCallError::Failure(e["error"].as_str().unwrap_or("").to_string())),
            }
        })
    }

    fn tool_after(&self, call: &ToolInvocation, result: &CallResult) {
        self.after.lock().unwrap().push((call.name.clone(), result.status().to_string()));
    }
}

pub fn limits_of(record: &Json) -> oc_codemode::codemode::ExecutionLimits {
    let l = &record["limits"];
    oc_codemode::codemode::ExecutionLimits {
        timeout_ms: l.get("timeoutMs").and_then(|v| v.as_f64()),
        max_tool_calls: l.get("maxToolCalls").and_then(|v| v.as_f64()),
        max_output_bytes: l.get("maxOutputBytes").and_then(|v| v.as_f64()),
    }
}

pub fn host_arc(record: &Json) -> Arc<ReplayHost> {
    Arc::new(ReplayHost::new(record))
}
