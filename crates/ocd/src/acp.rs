//! `opencode acp` — Agent Client Protocol over stdio (`cli/src/acp/*.ts`, spec 01 §14).
//!
//! Newline-delimited JSON-RPC 2.0 on stdin/stdout, mapped onto the HTTP API of a private
//! standalone server. Implemented: initialize/authenticate, session new/load/list/delete/resume/
//! close/fork/set_config_option/set_mode/prompt, `session/cancel` and `$/cancel_request`,
//! catalog + config options, the event fold (text, reasoning, tools, retries, compaction, child
//! sessions), permission requests, form elicitation, usage updates, history replay, additional
//! directories, MCP registration and the JSON-RPC error mapping.
//! File diffs on permission requests and completed edit/patch calls (`acp/permission.ts`).
//! Not implemented: background following of child sessions after the root turn ends.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use base64::Engine;
use oc_util::Global;
use parking_lot::Mutex;
use serde_json::{Map, Value, json};
use tokio::sync::{mpsc, oneshot, watch};

use oc_tools::jsdiff;
use oc_tools::patch::Hunk;

use crate::cli::parse::Input;
use crate::client::{Client, ClientError, EventStream, Req};
use crate::connection::Standalone;

const MAX_MESSAGE_BYTES: usize = 32 * 1024 * 1024;
const AUTH_METHOD_ID: &str = "opencode-login";
const CHILD_UPDATE_METHOD: &str = "opencode/session/child_update";
const DIRECTORIES_KEY: &str = "opencode.acp.additionalDirectories";
const UNSHOWN_QUESTION: &str = "The question couldn't be shown to the user in this client. Continue without an answer: make reasonable assumptions and state them, or ask the user in your reply if you can't proceed.";

// ---------------------------------------------------------------------------------------------
// errors (`acp/error.ts`)

#[derive(Debug, Clone)]
pub struct AcpError {
    pub code: i64,
    pub message: String,
    pub data: Value,
}

impl AcpError {
    fn invalid_params(detail: impl Into<String>, data: Value) -> Self {
        AcpError { code: -32602, message: format!("Invalid params: {}", detail.into()), data }
    }
    fn internal(detail: impl Into<String>, data: Value) -> Self {
        AcpError { code: -32603, message: format!("Internal error: {}", detail.into()), data }
    }
    fn session_not_found(id: &str) -> Self {
        Self::invalid_params(format!("session not found: {id}"), json!({"sessionId": id}))
    }
    fn invalid_request(message: &str, field: Option<&str>) -> Self {
        Self::invalid_params(message, field.map(|f| json!({"field": f})).unwrap_or(json!({})))
    }
    fn server_unavailable() -> Self {
        Self::internal("OpenCode server is unavailable", json!({"errorName": "ServerUnavailable"}))
    }
    fn defect() -> Self {
        tracing::warn!("ACP request failed");
        Self::internal("Internal service failure", json!({"errorName": "Error"}))
    }
    fn service_failure(safe: &str, service: Option<&str>, name: Option<&str>) -> Self {
        let mut data = Map::new();
        if let Some(s) = service {
            data.insert("service".into(), json!(s));
        }
        if let Some(n) = name {
            data.insert("errorName".into(), json!(n));
        }
        Self::internal(safe, Value::Object(data))
    }
    fn auth_required() -> Self {
        AcpError { code: -32000, message: "Authentication required: provider authentication required".into(), data: json!({}) }
    }
    fn method_not_found(method: &str) -> Self {
        AcpError { code: -32601, message: format!("\"Method not found\": {method}"), data: json!({"method": method}) }
    }
    fn invalid_config(id: &str) -> Self {
        Self::invalid_params(format!("unknown config option: {id}"), json!({"configId": id}))
    }
    fn invalid_model(provider: &str, model: &str) -> Self {
        Self::invalid_params(format!("model not found: {model}"), json!({"providerId": provider, "modelId": model}))
    }
    fn invalid_effort(effort: &str) -> Self {
        Self::invalid_params(format!("effort not found: {effort}"), json!({"effort": effort}))
    }
    fn invalid_mode(mode: &str) -> Self {
        Self::invalid_params(format!("mode not found: {mode}"), json!({"mode": mode}))
    }
    fn reloadable(&self) -> bool {
        self.code == -32602
            && (self.message.contains("model not found") || self.message.contains("effort not found") || self.message.contains("mode not found"))
    }
}

/// `ACPClient.classify`
fn classify(e: ClientError) -> AcpError {
    if e.is_transport() || matches!(&e, ClientError::Client { reason: "MalformedResponse", .. }) {
        return AcpError::server_unavailable();
    }
    let body = e.body().cloned().unwrap_or(Value::Null);
    match e.tag() {
        Some("SessionNotFoundError") => AcpError::session_not_found(body.get("sessionID").and_then(Value::as_str).unwrap_or("")),
        Some("InvalidRequestError") => {
            AcpError::invalid_request(body.get("message").and_then(Value::as_str).unwrap_or(""), body.get("field").and_then(Value::as_str))
        }
        Some("InvalidCursorError") => AcpError::invalid_request(body.get("message").and_then(Value::as_str).unwrap_or(""), Some("cursor")),
        _ => AcpError::defect(),
    }
}

type R<T> = Result<T, AcpError>;

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

// ---------------------------------------------------------------------------------------------
// connection

struct Conn {
    out: Mutex<std::io::Stdout>,
    next_id: AtomicI64,
    pending: Mutex<HashMap<i64, oneshot::Sender<Result<Value, Value>>>>,
}

impl Conn {
    fn send(&self, v: &Value) {
        let mut out = self.out.lock();
        let _ = out.write_all(format!("{v}\n").as_bytes());
        let _ = out.flush();
    }
    fn notify(&self, method: &str, params: Value) {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }
    fn update(&self, session: &str, update: Value) {
        self.notify("session/update", json!({"sessionId": session, "update": update}));
    }
    fn request(&self, method: &str, params: Value) -> (i64, oneshot::Receiver<Result<Value, Value>>) {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().insert(id, tx);
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        (id, rx)
    }
    fn cancel_request(&self, id: i64) {
        self.pending.lock().remove(&id);
        self.notify("$/cancel_request", json!({"requestId": id}));
    }
    fn respond(&self, id: &Value, result: R<Value>) {
        match result {
            Ok(v) => self.send(&json!({"jsonrpc": "2.0", "id": id, "result": v})),
            Err(e) => self.send(&json!({"jsonrpc": "2.0", "id": id, "error": {"code": e.code, "message": e.message, "data": e.data}})),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// paths & directories (`acp/directories.ts`)

fn fs_resolve(p: &str) -> String {
    let path = std::path::Path::new(p);
    let abs = if path.is_absolute() { path.to_path_buf() } else { std::env::current_dir().unwrap_or_default().join(path) };
    let abs = crate::run::tool_path(&abs.display().to_string(), "/");
    let abs = format!("/{}", abs.trim_start_matches('/'));
    std::fs::canonicalize(&abs).map(|c| c.display().to_string()).unwrap_or(abs)
}

fn absolutize(p: &str, cwd: &str) -> String {
    if std::path::Path::new(p).is_absolute() {
        return p.to_string();
    }
    let joined = std::path::Path::new(cwd).join(p);
    format!("/{}", crate::run::tool_path(&joined.display().to_string(), "/").trim_start_matches('/'))
}

fn parse_cwd(cwd: &Value) -> R<String> {
    let c = cwd.as_str().unwrap_or("");
    if c.is_empty() || !std::path::Path::new(c).is_absolute() {
        return Err(AcpError::invalid_request(&format!("cwd must be an absolute path: {c}"), Some("cwd")));
    }
    Ok(c.to_string())
}

fn parse_directories(cwd: &Value, dirs: Option<&Value>) -> R<(String, Vec<String>)> {
    let cwd = parse_cwd(cwd)?;
    let mut out: Vec<String> = vec![];
    for d in dirs.and_then(Value::as_array).cloned().unwrap_or_default() {
        let d = d.as_str().unwrap_or("").to_string();
        if d.is_empty() || !std::path::Path::new(&d).is_absolute() || d.contains('*') || d.contains('?') {
            return Err(AcpError::invalid_params(
                format!("additional directory must be an absolute path without glob characters: {d}"),
                json!({"additionalDirectory": d}),
            ));
        }
        let n = absolutize(&d, "/");
        if !out.contains(&n) {
            out.push(n);
        }
    }
    let root = fs_resolve(&cwd);
    out.retain(|d| fs_resolve(d) != root);
    Ok((cwd, out))
}

fn directory_rules(dirs: &[String]) -> Vec<Value> {
    let mut rules = vec![];
    for d in dirs {
        let mut paths = vec![d.clone()];
        let real = fs_resolve(d);
        if !paths.contains(&real) {
            paths.push(real);
        }
        for p in paths {
            rules.push(json!({"action": "external_directory", "resource": std::path::Path::new(&p).join("*").display().to_string(), "effect": "allow"}));
        }
    }
    rules
}

fn listed_directories(session: &Value) -> Vec<String> {
    session
        .pointer(&format!("/metadata/{}", DIRECTORIES_KEY.replace('/', "~1")))
        .and_then(Value::as_array)
        .filter(|a| a.iter().all(Value::is_string))
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------------------------
// catalog & config options (`acp/catalog.ts`, `acp/config-option.ts`)

#[derive(Clone, Debug, PartialEq)]
struct Catalog {
    models: Vec<Value>,
    default_model: Value,
    modes: Vec<Value>,
    default_mode: String,
    commands: Vec<Value>,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Selection {
    model: Option<Value>,
    mode: Option<String>,
}

fn variant_ids(model: &Value) -> Vec<String> {
    model.get("variants").and_then(Value::as_array).map(|a| a.iter().filter_map(|v| v.get("id").and_then(Value::as_str).map(String::from)).collect()).unwrap_or_default()
}

fn find_model<'a>(catalog: &'a Catalog, provider: &str, id: &str) -> Option<&'a Value> {
    catalog.models.iter().find(|m| s(m, "providerID") == provider && s(m, "id") == id)
}

fn select_variant(v: Option<&str>, variants: &[String]) -> String {
    match v {
        None | Some("") | Some("default") => "default".into(),
        Some(v) if variants.iter().any(|x| x == v) => v.into(),
        _ if variants.iter().any(|x| x == "default") => "default".into(),
        _ => variants.first().cloned().unwrap_or_else(|| "default".into()),
    }
}

fn effort_name(v: &str) -> String {
    v.split(['_', '-'])
        .map(|p| {
            let mut c = p.chars();
            c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn config_options(catalog: &Catalog, sel: &Selection) -> Value {
    let model = sel.model.clone().unwrap_or_else(|| catalog.default_model.clone());
    let mut models = catalog.models.clone();
    models.sort_by(|a, b| s(a, "providerID").cmp(s(b, "providerID")).then(crate::term::locale_cmp(s(a, "name"), s(b, "name"))));
    let mut options = vec![json!({
        "id": "model", "name": "Model", "category": "model", "type": "select",
        "currentValue": format!("{}/{}", s(&model, "providerID"), s(&model, "id")),
        "options": models.iter().map(|m| json!({"value": format!("{}/{}", s(m, "providerID"), s(m, "id")), "name": format!("{}/{}", s(m, "providerID"), s(m, "name"))})).collect::<Vec<_>>(),
    })];
    if let Some(current) = find_model(catalog, s(&model, "providerID"), s(&model, "id")) {
        let variants = variant_ids(current);
        if !variants.is_empty() {
            let mut values = variants.clone();
            if !values.iter().any(|v| v == "default") {
                values.push("default".into());
            }
            options.push(json!({
                "id": "effort", "name": "Effort", "description": "Available effort levels for this model", "category": "thought_level", "type": "select",
                "currentValue": select_variant(model.get("variant").and_then(Value::as_str), &variants),
                "options": values.iter().map(|v| json!({"value": v, "name": effort_name(v)})).collect::<Vec<_>>(),
            }));
        }
    }
    options.push(json!({
        "id": "mode", "name": "Session Mode", "category": "mode", "type": "select",
        "currentValue": sel.mode.clone().unwrap_or_else(|| catalog.default_mode.clone()),
        "options": catalog.modes.iter().map(|m| {
            let mut o = json!({"value": m.get("id"), "name": m.get("name")});
            if let Some(d) = m.get("description").and_then(Value::as_str).filter(|d| !d.is_empty()) { o["description"] = json!(d); }
            o
        }).collect::<Vec<_>>(),
    }));
    Value::Array(options)
}

fn available_commands(catalog: &Catalog) -> Value {
    let mut list: Vec<Value> = catalog.commands.iter().map(|c| json!({"name": c.get("name"), "description": c.get("description").and_then(Value::as_str).unwrap_or("")})).collect();
    list.push(json!({"name": "compact", "description": "Compact the session"}));
    Value::Array(list)
}

enum Change {
    Model(Value),
    Mode(String),
}

fn resolve_change(catalog: &Catalog, sel: &Selection, config: &str, value: &str) -> R<Change> {
    let current = sel.model.clone().unwrap_or_else(|| catalog.default_model.clone());
    match config {
        "model" => {
            let advertised = |p: &str, i: &str| find_model(catalog, p, i).is_some();
            let (provider, id, variant) = if let Some((p, i)) = value.split_once('/').filter(|(p, i)| advertised(p, i)) {
                (p.to_string(), i.to_string(), None)
            } else if let Some((prefix, suffix)) = value.rsplit_once('/').and_then(|(pre, suf)| {
                let (p, i) = pre.split_once('/')?;
                find_model(catalog, p, i).filter(|m| variant_ids(m).iter().any(|v| v == suf)).map(|_| (pre.to_string(), suf.to_string()))
            }) {
                let (p, i) = prefix.split_once('/').unwrap();
                (p.to_string(), i.to_string(), Some(suffix))
            } else {
                match value.split_once('/') {
                    Some((p, i)) => (p.to_string(), i.to_string(), None),
                    None => (value.to_string(), String::new(), None),
                }
            };
            let model = find_model(catalog, &provider, &id).ok_or_else(|| AcpError::invalid_model(&provider, value))?;
            let variants = variant_ids(model);
            if let Some(v) = &variant
                && !variants.contains(v)
            {
                return Err(AcpError::invalid_effort(v));
            }
            let variant = variant.or_else(|| {
                let cv = current.get("variant").and_then(Value::as_str)?;
                (s(&current, "providerID") == provider && s(&current, "id") == id && (cv == "default" || variants.iter().any(|x| x == cv))).then(|| cv.to_string())
            });
            let mut m = json!({"providerID": provider, "id": id});
            if let Some(v) = variant {
                m["variant"] = json!(v);
            }
            Ok(Change::Model(m))
        }
        "effort" => {
            let model = find_model(catalog, s(&current, "providerID"), s(&current, "id")).ok_or_else(|| AcpError::invalid_effort(value))?;
            if value != "default" && !variant_ids(model).iter().any(|v| v == value) {
                return Err(AcpError::invalid_effort(value));
            }
            let mut m = json!({"providerID": current.get("providerID"), "id": current.get("id")});
            m["variant"] = json!(value);
            Ok(Change::Model(m))
        }
        "mode" => {
            if !catalog.modes.iter().any(|m| s(m, "id") == value) {
                return Err(AcpError::invalid_mode(value));
            }
            Ok(Change::Mode(value.into()))
        }
        other => Err(AcpError::invalid_config(other)),
    }
}

// ---------------------------------------------------------------------------------------------
// tools (`acp/tool.ts`)

fn canonical(name: &str) -> String {
    let n = name.to_lowercase();
    crate::run::canonical_tool_name(&n).to_string()
}

fn tool_kind(name: &str) -> &'static str {
    match canonical(name).as_str() {
        "shell" => "execute",
        "webfetch" => "fetch",
        "edit" | "patch" | "write" => "edit",
        "grep" | "glob" | "context" | "context7_resolve_library_id" | "context7_get_library_docs" => "search",
        "read" => "read",
        "subagent" => "think",
        _ => "other",
    }
}

/// `patchHunks(input)`: the parsed `patchText`, or nothing when it doesn't parse.
fn patch_hunks(input: &Value) -> Vec<Hunk> {
    let Some(text) = input.get("patchText").and_then(Value::as_str).filter(|t| !t.is_empty()) else { return vec![] };
    oc_tools::patch::parse(text).unwrap_or_default()
}

fn locations(name: &str, input: &Value, cwd: &str) -> Value {
    let str_of = |k: &str| input.get(k).and_then(Value::as_str).map(String::from);
    let values: Vec<String> = match canonical(name).as_str() {
        "shell" => vec![str_of("workdir").or_else(|| str_of("cwd")).unwrap_or_else(|| cwd.to_string())],
        "read" | "edit" | "write" => str_of("path").or_else(|| str_of("filePath")).into_iter().collect(),
        "patch" => patch_hunks(input)
            .into_iter()
            .flat_map(|h| match h {
                Hunk::Update { path, move_path, .. } => std::iter::once(path).chain(move_path).collect::<Vec<_>>(),
                h => vec![h.path().to_string()],
            })
            .collect(),
        "external_directory" => str_of("filepath").into_iter().collect(),
        "grep" | "glob" | "context" | "context7_resolve_library_id" | "context7_get_library_docs" => str_of("path").into_iter().collect(),
        _ => vec![],
    };
    let mut seen = vec![];
    for v in values.into_iter().filter(|v| !v.is_empty()) {
        let a = absolutize(&v, cwd);
        if !seen.contains(&a) {
            seen.push(a);
        }
    }
    Value::Array(seen.into_iter().map(|p| json!({"path": p})).collect())
}

fn tool_title(name: &str, input: &Value, fallback: Option<&str>) -> String {
    if canonical(name) == "shell" {
        return input
            .get("command")
            .and_then(Value::as_str)
            .or_else(|| input.get("cmd").and_then(Value::as_str))
            .or(fallback)
            .unwrap_or(name)
            .to_string();
    }
    fallback.filter(|f| !f.is_empty()).unwrap_or(name).to_string()
}

fn raw_input(name: &str, input: &Value, cwd: &str) -> Value {
    if canonical(name) != "shell" || input.get("cwd").is_some_and(|v| !v.is_null() && v != "") || input.get("workdir").is_some_and(|v| !v.is_null() && v != "") {
        return input.clone();
    }
    let mut o = input.as_object().cloned().unwrap_or_default();
    o.insert("cwd".into(), json!(cwd));
    Value::Object(o)
}

fn pending_tool_call(id: &str, name: &str, input: &Value, title: Option<&str>, cwd: &str) -> Value {
    json!({"toolCallId": id, "name": name, "title": tool_title(name, input, title), "kind": tool_kind(name), "status": "pending", "locations": locations(name, input, cwd), "rawInput": raw_input(name, input, cwd)})
}

fn running_update(id: &str, name: &str, input: &Value, cwd: &str) -> Value {
    json!({"toolCallId": id, "status": "in_progress", "kind": tool_kind(name), "title": tool_title(name, input, None), "locations": locations(name, input, cwd), "rawInput": raw_input(name, input, cwd)})
}

fn tool_content(content: &Value) -> Vec<Value> {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"^data:([^;,]+)(?:;[^,]*)*;base64,(.*)$").unwrap());
    content
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|p| {
            if s(p, "type") == "text" {
                return Some(json!({"type": "content", "content": {"type": "text", "text": s(p, "text")}}));
            }
            let c = RE.captures(s(p, "uri"))?;
            let mime = c.get(1)?.as_str();
            let data = c.get(2)?.as_str();
            mime.starts_with("image/").then(|| json!({"type": "content", "content": {"type": "image", "mimeType": mime, "data": data}}))
        })
        .collect()
}

fn completed_update(id: &str, name: &str, input: &Value, content: &Value, metadata: Option<&Value>, cwd: &str) -> Value {
    let normalized = tool_content(content);
    let first_text = content.as_array().and_then(|a| a.iter().find(|p| s(p, "type") == "text")).map(|p| s(p, "text").to_string());
    let read = if canonical(name) == "read" { first_text.as_deref().and_then(crate::run::read_display_text) } else { None };
    let is_image = |p: &Value| p.pointer("/content/type").and_then(Value::as_str) == Some("image");
    let images: Vec<Value> = normalized.iter().filter(|p| is_image(p)).cloned().collect();
    let mut all: Vec<Value> = match read {
        Some(r) => vec![json!({"type": "content", "content": {"type": "text", "text": r}})],
        None => normalized.iter().filter(|p| !is_image(p)).cloned().collect(),
    };
    all.extend(images);
    let mut raw = Map::new();
    if let Some(m) = metadata {
        raw.insert("metadata".into(), m.clone());
    }
    json!({"toolCallId": id, "status": "completed", "locations": locations(name, input, cwd), "content": all, "rawOutput": raw})
}

fn error_update(id: &str, name: &str, input: &Value, content: &Value, metadata: Option<&Value>, error: &str, cwd: &str) -> Value {
    let mut c = tool_content(content);
    c.push(json!({"type": "content", "content": {"type": "text", "text": error}}));
    let mut raw = Map::new();
    if let Some(m) = metadata {
        raw.insert("metadata".into(), m.clone());
    }
    raw.insert("error".into(), json!(error));
    json!({"toolCallId": id, "status": "failed", "kind": tool_kind(name), "title": tool_title(name, input, None), "locations": locations(name, input, cwd), "rawInput": raw_input(name, input, cwd), "content": c, "rawOutput": raw})
}

// ---------------------------------------------------------------------------------------------
// file diffs (`acp/permission.ts`: `withCompletedDiffs`, `permissionPreviews`)

/// A `{type: "diff", path, oldText, newText}` tool-call content item.
fn diff_item(path: &str, old_text: Option<&str>, new_text: &str) -> Value {
    json!({"type": "diff", "path": path, "oldText": old_text, "newText": new_text})
}

struct FileInfo {
    file: String,
    patch: String,
    status: String,
}

/// `Schema.decodeUnknownOption(Schema.Array(FileDiff.Info))(metadata?.files)`: every entry must
/// decode, or there are none.
fn decode_files(metadata: Option<&Value>) -> Vec<FileInfo> {
    let count = |v: Option<&Value>| v.and_then(Value::as_f64).is_some_and(|n| n.is_finite() && n >= 0.0 && n.fract() == 0.0);
    let Some(files) = metadata.and_then(|m| m.get("files")).and_then(Value::as_array) else { return vec![] };
    let mut out = Vec::with_capacity(files.len());
    for f in files {
        let (Some(file), Some(patch), Some(status)) = (f.get("file").and_then(Value::as_str), f.get("patch").and_then(Value::as_str), f.get("status").and_then(Value::as_str)) else {
            return vec![];
        };
        if !count(f.get("additions")) || !count(f.get("deletions")) || !["added", "deleted", "modified"].contains(&status) {
            return vec![];
        }
        out.push(FileInfo { file: file.into(), patch: patch.into(), status: status.into() });
    }
    out
}

/// `Bun.file(path).text()`.
fn read_text(path: &str) -> Option<String> {
    std::fs::read(path).ok().map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// `withCompletedDiffs(update, source, cwd)`: appends a diff per changed file in the completed
/// tool's `metadata.files`, rebuilding the old text by reversing the recorded patch against the
/// file as it is now.
fn with_completed_diffs(mut update: Value, tool_name: &str, input: &Value, metadata: Option<&Value>, cwd: &str) -> Value {
    if s(&update, "sessionUpdate") != "tool_call_update" {
        return update;
    }
    let hunks = if canonical(tool_name) == "patch" { patch_hunks(input) } else { vec![] };
    let diffs: Vec<Value> = decode_files(metadata).iter().filter_map(|f| completed_diff(f, &hunks, cwd)).collect();
    if diffs.is_empty() {
        return update;
    }
    match update.get_mut("content").and_then(Value::as_array_mut) {
        Some(content) => content.extend(diffs),
        None => update["content"] = Value::Array(diffs),
    }
    update
}

fn completed_diff(file: &FileInfo, hunks: &[Hunk], cwd: &str) -> Option<Value> {
    let path = absolutize(&file.file, cwd);
    let new_text = if file.status == "deleted" { String::new() } else { read_text(&path)? };
    if file.status == "added" {
        return Some(diff_item(&path, None, &new_text));
    }
    let recorded = jsdiff::parse_patch(&file.patch).ok()?.into_iter().next()?;
    if let Some(old_text) = jsdiff::apply_patch(&new_text, &jsdiff::reverse_patch(&recorded)) {
        return Some(diff_item(&path, Some(&old_text), &new_text));
    }
    // Core trims indentation from patch-tool diffs, so rebuild those from its hunks and keep them if the ranges agree.
    let hunk = hunks.iter().find(|h| matches!(h, Hunk::Update { path: p, move_path, .. } if absolutize(move_path.as_deref().unwrap_or(p), cwd) == path));
    let Some(Hunk::Update { path: hunk_path, chunks, .. }) = hunk else { return None };
    if chunks.iter().any(|c| c.old_lines.is_empty() || c.new_lines.is_empty()) {
        return None;
    }
    let swapped: Vec<_> = chunks.iter().map(|c| oc_tools::patch::UpdateFileChunk { old_lines: c.new_lines.clone(), new_lines: c.old_lines.clone(), ..c.clone() }).collect();
    let rebuilt = oc_tools::patch::derive(hunk_path, &swapped, &new_text).ok()?.content;
    let ranges = |h: &[jsdiff::PatchHunk]| h.iter().map(|h| (h.old_start, h.old_lines, h.new_start, h.new_lines)).collect::<Vec<_>>();
    (ranges(&jsdiff::structured_patch(&rebuilt, &new_text)) == ranges(&recorded)).then(|| diff_item(&path, Some(&rebuilt), &new_text))
}

/// `permissionPreviews(toolName, input, metadata, cwd)`: the diffs a permission request shows.
/// Any failure drops every preview, like upstream's `orElseSucceed`.
fn permission_previews(tool_name: &str, input: &Value, metadata: Option<&Value>, cwd: &str) -> Vec<Value> {
    let previews = if canonical(tool_name) == "patch" {
        patch_previews(input, cwd)
    } else {
        decode_files(metadata)
            .iter()
            .map(|f| {
                let path = absolutize(&f.file, cwd);
                let old_text = if f.status == "added" { None } else { Some(read_text(&path)?) };
                let new_text = jsdiff::apply_patch_str(old_text.as_deref().unwrap_or(""), &f.patch).ok()?;
                Some(new_text.map(|n| diff_item(&path, old_text.as_deref(), &n)))
            })
            .collect::<Option<Vec<_>>>()
            .map(|v| v.into_iter().flatten().collect())
    };
    previews.unwrap_or_default()
}

// Core trims the patch tool's diffs for display, which breaks `applyPatch`, so its previews come from its own hunks.
fn patch_previews(input: &Value, cwd: &str) -> Option<Vec<Value>> {
    patch_hunks(input)
        .iter()
        .map(|hunk| {
            let path = absolutize(hunk.path(), cwd);
            match hunk {
                Hunk::Add { contents, .. } => {
                    let new_text = if contents.ends_with('\n') || contents.is_empty() { contents.clone() } else { format!("{contents}\n") };
                    Some(diff_item(&path, None, &new_text))
                }
                Hunk::Delete { .. } => Some(diff_item(&path, Some(&read_text(&path)?), "")),
                Hunk::Update { path: rel, move_path, chunks } => {
                    let old_text = read_text(&path)?;
                    let derived = oc_tools::patch::derive(rel, chunks, &old_text).ok()?;
                    let target = move_path.as_deref().map(|m| absolutize(m, cwd)).unwrap_or(path);
                    Some(diff_item(&target, Some(&old_text), &derived.content))
                }
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// prompt content (`acp/content.ts`, `acp/prompt.ts`)

fn filename_from_uri(uri: &str) -> Option<String> {
    if uri.is_empty() || uri.starts_with("data:") {
        return None;
    }
    let base = |p: &str| p.rsplit('/').next().filter(|b| !b.is_empty()).map(String::from);
    match url::Url::parse(uri) {
        Ok(u) => {
            if let Some((_, p)) = u.query_pairs().find(|(k, _)| k == "path") {
                return base(&p);
            }
            base(&percent_encoding::percent_decode_str(u.path()).decode_utf8_lossy())
        }
        Err(_) => base(uri),
    }
}

/// Converted ACP prompt part: `(text, synthetic, ignored)` or a file `(url, filename, mime)`.
#[derive(Debug, Clone)]
enum Part {
    Text { text: String, synthetic: bool, ignored: bool },
    File { url: String, filename: String, #[allow(dead_code)] mime: String },
}

fn resource_link_to_part(uri: &str, name: Option<&str>, mime: Option<&str>) -> Part {
    let mime = mime.unwrap_or("text/plain").to_string();
    if uri.starts_with("file://") {
        let filename = name.filter(|n| !n.is_empty()).map(String::from).or_else(|| filename_from_uri(uri)).unwrap_or_else(|| "file".into());
        return Part::File { url: uri.into(), filename, mime };
    }
    if uri.starts_with("zed://")
        && let Ok(u) = url::Url::parse(uri)
        && let Some((_, path)) = u.query_pairs().find(|(k, _)| k == "path")
    {
        let file_url = url::Url::from_file_path(path.as_ref()).map(|u| u.to_string()).unwrap_or_else(|_| format!("file://{path}"));
        let filename = name.filter(|n| !n.is_empty()).map(String::from).or_else(|| path.rsplit('/').next().map(String::from)).unwrap_or_else(|| "file".into());
        return Part::File { url: file_url, filename, mime };
    }
    Part::Text { text: match name { Some(n) if !n.is_empty() => format!("[{n}]({uri})"), _ => uri.to_string() }, synthetic: false, ignored: false }
}

fn prompt_parts(blocks: &[Value]) -> Vec<Part> {
    let mut parts = vec![];
    for b in blocks {
        match s(b, "type") {
            "text" => {
                let audience: Vec<&str> = b.pointer("/annotations/audience").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
                parts.push(Part::Text {
                    text: s(b, "text").into(),
                    synthetic: audience == ["assistant"],
                    ignored: audience == ["user"],
                });
            }
            "image" => {
                let data = s(b, "data");
                let uri = s(b, "uri");
                let mime = s(b, "mimeType");
                let url = if !data.is_empty() { format!("data:{mime};base64,{data}") } else { uri.to_string() };
                if url.is_empty() {
                    continue;
                }
                let filename = filename_from_uri(uri).unwrap_or_else(|| "image".into());
                if url.starts_with("data:") {
                    parts.push(Part::File { url, filename, mime: mime.into() });
                } else {
                    parts.push(resource_link_to_part(&url, Some(&filename), Some(mime)));
                }
            }
            "resource_link" => parts.push(resource_link_to_part(s(b, "uri"), b.get("name").and_then(Value::as_str), b.get("mimeType").and_then(Value::as_str))),
            "resource" => {
                let r = b.get("resource").cloned().unwrap_or(Value::Null);
                let uri = s(&r, "uri");
                if let Some(text) = r.get("text").and_then(Value::as_str) {
                    let label = match url::Url::parse(uri) {
                        Ok(u) if u.scheme() == "file" => {
                            let path = u.to_file_path().map(|p| p.display().to_string()).unwrap_or_else(|_| percent_encoding::percent_decode_str(u.path()).decode_utf8_lossy().into_owned());
                            let line = u.fragment().and_then(|f| f.strip_prefix('L')).and_then(|l| {
                                let digits: String = l.chars().take_while(char::is_ascii_digit).collect();
                                (!digits.is_empty()).then_some(digits)
                            });
                            format!("{path}{}", line.map(|l| format!(":{l}")).unwrap_or_default())
                        }
                        _ => uri.to_string(),
                    };
                    parts.push(Part::Text { text: format!("[{label}]\n{text}"), synthetic: false, ignored: false });
                } else if let (Some(blob), Some(mime)) = (r.get("blob").and_then(Value::as_str), r.get("mimeType").and_then(Value::as_str)) {
                    let url = if uri.starts_with("data:") { uri.to_string() } else { format!("data:{mime};base64,{blob}") };
                    parts.push(Part::File { url, filename: filename_from_uri(uri).unwrap_or_else(|| "file".into()), mime: mime.into() });
                }
            }
            _ => {}
        }
    }
    parts
}

struct Prepared {
    text: String,
    files: Vec<Value>,
    synthetic: Vec<String>,
    slash: Option<(String, String)>,
    start_compaction: bool,
    start_id: String,
    command: Option<Value>,
}

fn prepare(catalog: &Catalog, blocks: &[Value]) -> R<Prepared> {
    if blocks.iter().any(|b| s(b, "type") == "image" && s(b, "data").is_empty() && s(b, "uri").is_empty()) {
        return Err(AcpError::invalid_request("image content has no data or uri", Some("prompt")));
    }
    let mut parts = prompt_parts(blocks);
    for p in parts.iter_mut() {
        if let Part::File { url, filename, .. } = p
            && url.starts_with("file://")
        {
            let path = url::Url::parse(url).ok().and_then(|u| u.to_file_path().ok());
            let readable = path.as_ref().is_some_and(|p| p.is_dir() || std::fs::File::open(p).is_ok());
            if !readable {
                *p = Part::Text { text: format!("[{filename}]({url})"), synthetic: false, ignored: false };
            }
        }
    }
    let visible: Vec<&Part> = parts.iter().filter(|p| !matches!(p, Part::Text { synthetic: true, .. } | Part::Text { ignored: true, .. })).collect();
    let text = visible.iter().filter_map(|p| if let Part::Text { text, .. } = p { Some(text.clone()) } else { None }).collect::<Vec<_>>().join("\n");
    let files = visible.iter().filter_map(|p| if let Part::File { url, filename, .. } = p { Some(json!({"uri": url, "name": filename})) } else { None }).collect();
    let synthetic = parts.iter().filter_map(|p| if let Part::Text { text, synthetic: true, .. } = p { Some(text.clone()) } else { None }).collect();
    let trimmed = text.trim();
    let slash = trimmed.strip_prefix('/').and_then(|rest| {
        let mut tokens = rest.split_whitespace();
        let name = tokens.next()?.to_string();
        Some((name, tokens.collect::<Vec<_>>().join(" ").trim().to_string()))
    });
    let start_compaction = slash.as_ref().is_some_and(|(n, _)| n == "compact");
    let command = slash.as_ref().and_then(|(n, _)| catalog.commands.iter().find(|c| s(c, "name") == n).cloned());
    Ok(Prepared { text, files, synthetic, slash, start_compaction, start_id: oc_schema::id::message(), command })
}

/// `partsToContentChunks` for replayed user files.
fn file_chunk(url: &str, filename: Option<&str>, mime: &str) -> Option<Value> {
    if url.starts_with("file://") {
        return Some(json!({"type": "resource_link", "uri": url, "name": filename.unwrap_or("file"), "mimeType": mime}));
    }
    let re = regex::Regex::new(r"^data:([^;]+);base64,(.*)$").unwrap();
    let c = re.captures(url)?;
    let (m, data) = (c.get(1)?.as_str(), c.get(2)?.as_str());
    let file_url = |n: &str| url::Url::from_file_path(std::env::current_dir().unwrap_or_default().join(n)).map(|u| u.to_string()).unwrap_or_default();
    if m.starts_with("image/") {
        return Some(json!({"type": "image", "mimeType": m, "data": data, "uri": file_url(filename.unwrap_or("image"))}));
    }
    if m.starts_with("text/") || m == "application/json" {
        let text = base64::engine::general_purpose::STANDARD.decode(data).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
        return Some(json!({"type": "resource", "resource": {"uri": file_url(filename.unwrap_or("file")), "mimeType": m, "text": text}}));
    }
    Some(json!({"type": "resource", "resource": {"uri": file_url(filename.unwrap_or("file")), "mimeType": m, "blob": data}}))
}

// ---------------------------------------------------------------------------------------------
// capabilities

#[derive(Clone, Copy, Default)]
struct Caps {
    child_updates: bool,
    form_elicitation: bool,
    compaction: bool,
}

fn parse_caps(c: Option<&Value>) -> Caps {
    let Some(c) = c else { return Caps::default() };
    let present = |p: &str| c.pointer(p).is_some_and(|v| !v.is_null());
    Caps {
        child_updates: c.pointer("/_meta/opencode~1child-session-updates") == Some(&Value::Bool(true)),
        form_elicitation: present("/elicitation/form"),
        compaction: present("/session/compaction"),
    }
}

// ---------------------------------------------------------------------------------------------
// agent

struct Attached {
    cwd: String,
    selection: Selection,
}

struct TurnCtl {
    cancel: watch::Sender<bool>,
    done: watch::Receiver<bool>,
}

struct Agent {
    conn: Arc<Conn>,
    client: Client,
    caps: Mutex<Caps>,
    catalogs: tokio::sync::Mutex<HashMap<String, Catalog>>,
    sessions: Mutex<HashMap<String, Attached>>,
    mcp: Mutex<HashMap<String, HashMap<String, Value>>>,
    turns: Mutex<HashMap<String, Arc<TurnCtl>>>,
    prompt_requests: Mutex<HashMap<String, String>>,
}

impl Agent {
    async fn read_catalog(&self, cwd: &str) -> R<Catalog> {
        let dir = cwd.to_string();
        let c = &self.client;
        let (models, default, agents, commands) = tokio::join!(
            c.call("model.list", Req::new().location(&dir)),
            c.call("model.default", Req::new().location(&dir)),
            c.call("agent.list", Req::new().location(&dir)),
            c.call("command.list", Req::new().location(&dir)),
        );
        let load = |r: Result<Value, ClientError>| -> R<Vec<Value>> {
            match r {
                Ok(v) => Ok(v.get("data").and_then(Value::as_array).cloned().unwrap_or_default()),
                Err(e) if e.is_transport() => Err(AcpError::server_unavailable()),
                Err(_) => Err(AcpError::defect()),
            }
        };
        let models: Vec<Value> = load(models)?.into_iter().filter(|m| m.get("enabled").and_then(Value::as_bool) != Some(false)).collect();
        let default = match default {
            Ok(v) => v.get("data").cloned().filter(|d| !d.is_null()),
            Err(e) if e.is_transport() => return Err(AcpError::server_unavailable()),
            Err(_) => return Err(AcpError::defect()),
        };
        let not_ready = |what: &str| AcpError::service_failure(if what == "models" { "No models are available" } else { "No primary agents are available" }, None, Some("CatalogNotReady"));
        let chosen = match default {
            Some(d) => find_model(&Catalog { models: models.clone(), default_model: Value::Null, modes: vec![], default_mode: String::new(), commands: vec![] }, s(&d, "providerID"), s(&d, "id")).cloned().ok_or_else(|| not_ready("models"))?,
            None => models.first().cloned().ok_or_else(|| not_ready("models"))?,
        };
        let mut default_model = json!({"providerID": chosen.get("providerID"), "id": chosen.get("id")});
        if variant_ids(&chosen).iter().any(|v| v == "default") {
            default_model["variant"] = json!("default");
        }
        let agents: Vec<Value> = load(agents)?.into_iter().filter(|a| s(a, "mode") != "subagent" && a.get("hidden").and_then(Value::as_bool) != Some(true)).collect();
        let first = agents.first().ok_or_else(|| not_ready("agents"))?;
        let default_mode = s(first, "id").to_string();
        let modes = agents.iter().map(|a| json!({"id": a.get("id"), "name": a.get("name"), "description": a.get("description")})).collect();
        let commands = load(commands)?.into_iter().filter(|c| s(c, "name") != "compact").collect();
        Ok(Catalog { models, default_model, modes, default_mode, commands })
    }

    async fn load_catalog(&self, cwd: &str) -> R<Catalog> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            match self.client.call("plugin.list", Req::new().location(cwd)).await {
                Ok(v) if v.get("data").and_then(Value::as_array).is_some_and(|a| !a.is_empty()) => break,
                _ => tokio::time::sleep(Duration::from_millis(25)).await,
            }
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            match self.read_catalog(cwd).await {
                Err(e) if e.data.get("errorName") == Some(&json!("CatalogNotReady")) && tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(25)).await
                }
                other => return other,
            }
        }
    }

    async fn catalog(&self, cwd: &str) -> R<Catalog> {
        let key = fs_resolve(cwd);
        let mut cache = self.catalogs.lock().await;
        if let Some(c) = cache.get(&key) {
            return Ok(c.clone());
        }
        let c = self.load_catalog(cwd).await?;
        cache.insert(key, c.clone());
        Ok(c)
    }

    async fn reload(&self, cwd: &str) -> R<Catalog> {
        let c = self.load_catalog(cwd).await?;
        self.catalogs.lock().await.insert(fs_resolve(cwd), c.clone());
        Ok(c)
    }

    async fn get_session(&self, id: &str, cwd: &str) -> R<Value> {
        if !id.starts_with("ses") {
            return Err(AcpError::invalid_request("Invalid session ID", Some("sessionID")));
        }
        let session = self.client.call("session.get", Req::new().path("sessionID", id)).await.map_err(classify)?.get("data").cloned().unwrap_or(Value::Null);
        if fs_resolve(cwd) != fs_resolve(s(session.get("location").unwrap_or(&Value::Null), "directory")) {
            return Err(AcpError::invalid_params(format!("session {id} does not belong to cwd: {cwd}"), json!({"sessionId": id, "cwd": cwd})));
        }
        Ok(session)
    }

    async fn activate(&self, session: &Value, dirs: &[String]) -> R<()> {
        let previous = listed_directories(session);
        let owned = directory_rules(&previous);
        let current: Vec<Value> = session.get("permissions").and_then(Value::as_array).cloned().unwrap_or_default();
        let mut next = directory_rules(dirs);
        next.extend(current.iter().filter(|r| !owned.contains(r)).cloned());
        let mut body = Map::new();
        if next != current {
            body.insert("permissions".into(), Value::Array(next));
        }
        if previous != dirs {
            let mut meta = session.get("metadata").and_then(Value::as_object).cloned().unwrap_or_default();
            meta.remove(DIRECTORIES_KEY);
            if !dirs.is_empty() {
                meta.insert(DIRECTORIES_KEY.into(), json!(dirs));
            }
            body.insert("metadata".into(), Value::Object(meta));
        }
        if body.is_empty() {
            return Ok(());
        }
        self.client.call("session.update", Req::new().path("sessionID", s(session, "id")).body(Value::Object(body))).await.map_err(classify)?;
        Ok(())
    }

    fn check_mcp(servers: Option<&Value>) -> R<Vec<Value>> {
        let list = servers.and_then(Value::as_array).cloned().unwrap_or_default();
        if list.iter().any(|s| s.get("type").is_some_and(|t| t != "http")) {
            return Err(AcpError::invalid_request("Only stdio and HTTP MCP servers are supported", Some("mcpServers")));
        }
        Ok(list)
    }

    async fn register_mcp(&self, id: &str, cwd: &str, servers: &[Value]) -> R<()> {
        for server in servers {
            let name = s(server, "name").to_string();
            let pairs = |k: &str, kk: &str| -> Map<String, Value> {
                server.get(k).and_then(Value::as_array).map(|a| a.iter().map(|e| (s(e, "name").to_string(), e.get(kk).cloned().unwrap_or(Value::Null))).collect()).unwrap_or_default()
            };
            let config = if server.get("type").is_some() {
                json!({"type": "remote", "url": server.get("url"), "headers": pairs("headers", "value"), "oauth": false})
            } else {
                let mut cmd = vec![server.get("command").cloned().unwrap_or(Value::Null)];
                cmd.extend(server.get("args").and_then(Value::as_array).cloned().unwrap_or_default());
                json!({"type": "local", "command": cmd, "environment": pairs("env", "value")})
            };
            if self.mcp.lock().get(id).and_then(|m| m.get(&name)) == Some(&config) {
                continue;
            }
            self.mcp.lock().entry(id.to_string()).or_default().insert(name.clone(), config.clone());
            if let Err(e) = self.client.call("experimental.mcp.add", Req::new().path("server", &name).location(cwd).body(json!({"config": config}))).await {
                if let Some(m) = self.mcp.lock().get_mut(id) {
                    m.remove(&name);
                }
                return Err(classify(e));
            }
        }
        Ok(())
    }

    async fn attach(&self, session: &Value, cwd: &str, servers: &[Value]) -> R<(String, Value, Catalog)> {
        let catalog = self.catalog(cwd).await?;
        let id = s(session, "id").to_string();
        let selection = Selection {
            model: session.get("model").filter(|m| !m.is_null()).cloned(),
            mode: session.get("agent").and_then(Value::as_str).map(String::from),
        };
        let options = config_options(&catalog, &selection);
        self.sessions.lock().insert(id.clone(), Attached { cwd: cwd.to_string(), selection });
        if let Err(e) = self.register_mcp(&id, cwd, servers).await {
            self.sessions.lock().remove(&id);
            return Err(e);
        }
        Ok((id, options, catalog))
    }

    fn after_attach(&self, id: &str, catalog: &Catalog) {
        self.conn.update(id, json!({"sessionUpdate": "available_commands_update", "availableCommands": available_commands(catalog)}));
    }

    fn detach(&self, id: &str) {
        self.sessions.lock().remove(id);
        self.mcp.lock().remove(id);
    }

    fn selection(&self, id: &str) -> R<(String, Selection)> {
        self.sessions.lock().get(id).map(|a| (a.cwd.clone(), a.selection.clone())).ok_or_else(|| AcpError::session_not_found(id))
    }

    async fn select(&self, id: &str, config: &str, value: &str) -> R<()> {
        let attempt = async |catalog: &Catalog| -> R<Change> {
            let (_, sel) = self.selection(id)?;
            resolve_change(catalog, &sel, config, value)
        };
        let (cwd, _) = self.selection(id)?;
        let change = match attempt(&self.catalog(&cwd).await?).await {
            Err(e) if e.reloadable() => attempt(&self.reload(&cwd).await?).await?,
            other => other?,
        };
        match change {
            Change::Model(m) => {
                if let Some(a) = self.sessions.lock().get_mut(id) {
                    a.selection.model = Some(m.clone());
                }
                self.client.call("session.switchModel", Req::new().path("sessionID", id).body(json!({"model": m}))).await.map_err(classify)?;
            }
            Change::Mode(mode) => {
                if let Some(a) = self.sessions.lock().get_mut(id) {
                    a.selection.mode = Some(mode.clone());
                }
                self.client.call("session.switchAgent", Req::new().path("sessionID", id).body(json!({"agent": mode}))).await.map_err(classify)?;
            }
        }
        Ok(())
    }

    async fn current_options(&self, id: &str) -> R<Value> {
        let (cwd, sel) = self.selection(id)?;
        Ok(config_options(&self.catalog(&cwd).await?, &sel))
    }

    async fn cancel_turn(&self, id: &str, wait: bool) -> bool {
        let ctl = self.turns.lock().get(id).cloned();
        let Some(ctl) = ctl else { return false };
        let _ = ctl.cancel.send(true);
        if wait {
            let mut done = ctl.done.clone();
            while !*done.borrow() {
                if done.changed().await.is_err() {
                    break;
                }
            }
        }
        true
    }

    // -----------------------------------------------------------------------------------------
    // methods

    async fn handle(self: &Arc<Self>, method: &str, params: Value, request_id: &Value) -> R<Value> {
        match method {
            "initialize" => {
                let caps = params.get("clientCapabilities");
                *self.caps.lock() = parse_caps(caps);
                let mut auth = json!({"description": "Run `ocd auth login` in the terminal", "name": "Login with ocd", "id": AUTH_METHOD_ID});
                if caps.and_then(|c| c.pointer("/auth/terminal")).is_some_and(|t| t.as_bool().unwrap_or(!t.is_null())) {
                    auth["type"] = json!("terminal");
                    auth["args"] = json!(["--login"]);
                }
                if caps.and_then(|c| c.pointer("/_meta/terminal-auth")) == Some(&Value::Bool(true)) {
                    auth["_meta"] = json!({"terminal-auth": {"command": crate::cli::spec::CLI_NAME, "args": ["auth", "login"], "label": "OpenCode Login"}});
                }
                Ok(json!({
                    "protocolVersion": 1,
                    "agentCapabilities": {
                        "loadSession": true,
                        "mcpCapabilities": {"http": true, "sse": false},
                        "promptCapabilities": {"embeddedContext": true, "image": true},
                        "sessionCapabilities": {"additionalDirectories": {}, "close": {}, "delete": {}, "fork": {}, "list": {}, "resume": {}},
                        "_meta": {"opencode/child-session-updates": true},
                    },
                    "authMethods": [auth],
                    "agentInfo": {"name": "OpenCode", "version": oc_util::version::VERSION},
                }))
            }
            "authenticate" => {
                let m = s(&params, "methodId");
                if m != AUTH_METHOD_ID {
                    return Err(AcpError::invalid_params(format!("unknown auth method: {m}"), json!({"methodId": m})));
                }
                Ok(json!({}))
            }
            "session/new" => {
                let (cwd, dirs) = parse_directories(params.get("cwd").unwrap_or(&Value::Null), params.get("additionalDirectories"))?;
                let servers = Self::check_mcp(params.get("mcpServers"))?;
                self.catalog(&cwd).await?;
                let mut body = json!({"location": {"directory": cwd}});
                if !dirs.is_empty() {
                    body["permissions"] = Value::Array(directory_rules(&dirs));
                    body["metadata"] = json!({DIRECTORIES_KEY: dirs});
                }
                let created = self.client.call("session.create", Req::new().body(body)).await.map_err(classify)?.get("data").cloned().unwrap_or(Value::Null);
                let (id, options, catalog) = self.attach(&created, &cwd, &servers).await?;
                self.pending_after(request_id, id.clone(), catalog);
                Ok(json!({"sessionId": id, "configOptions": options}))
            }
            "session/load" | "session/resume" => {
                let (cwd, dirs) = parse_directories(params.get("cwd").unwrap_or(&Value::Null), params.get("additionalDirectories"))?;
                let servers = Self::check_mcp(params.get("mcpServers"))?;
                let session = self.get_session(s(&params, "sessionId"), &cwd).await?;
                self.activate(&session, &dirs).await?;
                let dir = s(session.get("location").unwrap_or(&Value::Null), "directory").to_string();
                let (id, options, catalog) = self.attach(&session, &dir, &servers).await?;
                if method == "session/load" {
                    if let Err(e) = self.replay(&id, &dir).await {
                        self.detach(&id);
                        return Err(e);
                    }
                    self.pending_after(request_id, id.clone(), catalog);
                    return Ok(json!({"configOptions": self.current_options(&id).await.unwrap_or(options)}));
                }
                self.pending_after(request_id, id, catalog);
                Ok(json!({"configOptions": options}))
            }
            "session/fork" => {
                let (cwd, dirs) = parse_directories(params.get("cwd").unwrap_or(&Value::Null), params.get("additionalDirectories"))?;
                let servers = Self::check_mcp(params.get("mcpServers"))?;
                let session = self.get_session(s(&params, "sessionId"), &cwd).await?;
                let forked = self.client.call("session.fork", Req::new().path("sessionID", s(&session, "id"))).await.map_err(classify)?.get("data").cloned().unwrap_or(Value::Null);
                self.activate(&forked, &dirs).await?;
                let dir = s(forked.get("location").unwrap_or(&Value::Null), "directory").to_string();
                let (id, options, catalog) = self.attach(&forked, &dir, &servers).await?;
                self.pending_after(request_id, id.clone(), catalog);
                Ok(json!({"sessionId": id, "configOptions": options}))
            }
            "session/list" => {
                let mut req = Req::new();
                if let Some(cwd) = params.get("cwd").filter(|c| !c.is_null()) {
                    req = req.query("directory", json!(parse_cwd(cwd)?));
                }
                req = req.query("order", json!("desc")).query("limit", json!(100));
                if let Some(c) = params.get("cursor").and_then(Value::as_str).filter(|c| !c.is_empty()) {
                    req = req.query("cursor", json!(c));
                }
                let page = self.client.call("session.list", req).await.map_err(classify)?;
                let iso = |ms: i64| chrono::DateTime::from_timestamp_millis(ms).map(|d| d.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()).unwrap_or_default();
                let sessions: Vec<Value> = page
                    .get("data")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .map(|session| {
                        let mut o = Map::new();
                        o.insert("sessionId".into(), session["id"].clone());
                        o.insert("cwd".into(), session.pointer("/location/directory").cloned().unwrap_or(Value::Null));
                        let dirs = listed_directories(session);
                        if !dirs.is_empty() {
                            o.insert("additionalDirectories".into(), json!(dirs));
                        }
                        let title = session.get("title").and_then(Value::as_str).map(String::from).unwrap_or_else(|| {
                            format!(
                                "{} session - {}",
                                if session.get("parentID").is_some_and(|p| !p.is_null()) { "Child" } else { "New" },
                                iso(session.pointer("/time/created").and_then(Value::as_i64).unwrap_or(0))
                            )
                        });
                        o.insert("title".into(), json!(title));
                        o.insert("updatedAt".into(), json!(iso(session.pointer("/time/updated").and_then(Value::as_i64).unwrap_or(0))));
                        Value::Object(o)
                    })
                    .collect();
                let mut out = json!({"sessions": sessions});
                if let Some(n) = page.pointer("/cursor/next").and_then(Value::as_str) {
                    out["nextCursor"] = json!(n);
                }
                Ok(out)
            }
            "session/delete" => {
                let id = s(&params, "sessionId").to_string();
                self.cancel_turn(&id, true).await;
                if id.starts_with("ses") {
                    match self.client.call("session.remove", Req::new().path("sessionID", &id)).await {
                        Ok(_) => {}
                        Err(e) if matches!(e.tag(), Some("SessionNotFoundError") | Some("InvalidRequestError")) => {}
                        Err(e) => return Err(classify(e)),
                    }
                }
                self.detach(&id);
                Ok(json!({}))
            }
            "session/close" => {
                let id = s(&params, "sessionId").to_string();
                if !self.cancel_turn(&id, true).await && id.starts_with("ses") {
                    match self.client.call("session.interrupt", Req::new().path("sessionID", &id)).await {
                        Ok(_) => {}
                        Err(e) if matches!(e.tag(), Some("SessionNotFoundError") | Some("InvalidRequestError")) => {}
                        Err(e) => return Err(classify(e)),
                    }
                }
                self.detach(&id);
                Ok(json!({}))
            }
            "session/set_config_option" => {
                let id = s(&params, "sessionId").to_string();
                self.selection(&id)?;
                let config = s(&params, "configId").to_string();
                let Some(value) = params.get("value").and_then(Value::as_str) else { return Err(AcpError::invalid_config(&config)) };
                self.select(&id, &config, value).await?;
                Ok(json!({"configOptions": self.current_options(&id).await?}))
            }
            "session/set_mode" => {
                let id = s(&params, "sessionId").to_string();
                self.selection(&id)?;
                self.select(&id, "mode", s(&params, "modeId")).await?;
                Ok(json!({}))
            }
            "session/prompt" => {
                let id = s(&params, "sessionId").to_string();
                self.prompt_requests.lock().insert(request_id.to_string(), id.clone());
                let r = self.clone().prompt(&id, params.get("prompt").and_then(Value::as_array).cloned().unwrap_or_default()).await;
                self.prompt_requests.lock().remove(&request_id.to_string());
                r
            }
            other => Err(AcpError::method_not_found(other)),
        }
    }

    /// Updates sent after the response of new/load/resume/fork has been written.
    fn pending_after(&self, request_id: &Value, id: String, catalog: Catalog) {
        AFTER.lock().insert(request_id.to_string(), (id, catalog));
    }

    async fn replay(&self, id: &str, cwd: &str) -> R<()> {
        let caps = *self.caps.lock();
        let mut cursor: Option<String> = None;
        loop {
            let mut req = Req::new().path("sessionID", id).query("limit", json!(200));
            req = match &cursor {
                Some(c) => req.query("cursor", json!(c)),
                None => req.query("order", json!("asc")),
            };
            let page = self.client.call("session.message.list", req).await.map_err(classify)?;
            for m in page.get("data").and_then(Value::as_array).cloned().unwrap_or_default() {
                for update in replay_message(&m, cwd, caps) {
                    self.conn.update(id, update);
                }
            }
            match page.pointer("/cursor/next").and_then(Value::as_str) {
                Some(n) => cursor = Some(n.into()),
                None => return Ok(()),
            }
        }
    }
}

static AFTER: std::sync::LazyLock<Mutex<HashMap<String, (String, Catalog)>>> = std::sync::LazyLock::new(Default::default);

fn compaction_update(status: &str, message_id: &str, reason: &Value, summary: Option<&str>, error: Option<&Value>, standard: bool) -> Value {
    if !standard {
        let mut marker = json!({"status": status, "messageId": message_id, "reason": reason});
        if status == "failed" {
            marker["error"] = error.cloned().unwrap_or(Value::Null);
        }
        return json!({"sessionUpdate": "session_info_update", "_meta": {"opencode/compaction": marker}});
    }
    match status {
        "started" => json!({"sessionUpdate": "compaction_update", "compactionId": message_id, "status": "in_progress"}),
        "completed" => {
            let summary = summary.filter(|s| !s.is_empty()).map(|s| json!([{"type": "text", "text": s}])).unwrap_or(Value::Null);
            json!({"sessionUpdate": "compaction_update", "compactionId": message_id, "status": "completed", "summary": summary})
        }
        _ => {
            let e = error.cloned().unwrap_or(Value::Null);
            let ty = s(&e, "type");
            if ty == "aborted" || ty == "compaction.interrupted" {
                return json!({"sessionUpdate": "compaction_update", "compactionId": message_id, "status": "cancelled", "summary": null});
            }
            let msg = s(&e, "message");
            let display = if msg.is_empty() || (ty == "compaction.failed" && msg.contains('\n')) { "Compaction failed" } else { msg };
            json!({"sessionUpdate": "compaction_update", "compactionId": message_id, "status": "failed", "summary": null, "error": display})
        }
    }
}

/// `ACPReplay` for one projected message.
fn replay_message(m: &Value, cwd: &str, caps: Caps) -> Vec<Value> {
    let id = s(m, "id").to_string();
    let mut out = vec![];
    match s(m, "type") {
        "user" => {
            out.push(json!({"sessionUpdate": "user_message_chunk", "messageId": id, "content": {"type": "text", "text": s(m, "text")}}));
            for f in m.get("files").and_then(Value::as_array).cloned().unwrap_or_default() {
                let url = if f.pointer("/source/type").and_then(Value::as_str) == Some("uri") {
                    f.pointer("/source/uri").and_then(Value::as_str).unwrap_or("").to_string()
                } else {
                    format!("data:{};base64,{}", s(&f, "mime"), f.pointer("/source/data").and_then(Value::as_str).or_else(|| f.get("data").and_then(Value::as_str)).unwrap_or(""))
                };
                if let Some(chunk) = file_chunk(&url, f.get("name").and_then(Value::as_str), s(&f, "mime")) {
                    out.push(json!({"sessionUpdate": "user_message_chunk", "messageId": id, "content": chunk}));
                }
            }
        }
        "compaction" => {
            let status = s(m, "status");
            if status != "running" {
                let st = if status == "failed" { "failed" } else { "completed" };
                out.push(compaction_update(st, &id, m.get("reason").unwrap_or(&Value::Null), m.get("summary").and_then(Value::as_str), m.get("error"), caps.compaction));
            }
        }
        "assistant" => {
            let mut reasoning = 0;
            for item in m.get("content").and_then(Value::as_array).cloned().unwrap_or_default() {
                match s(&item, "type") {
                    "text" => out.push(json!({"sessionUpdate": "agent_message_chunk", "messageId": id, "content": {"type": "text", "text": s(&item, "text")}})),
                    "reasoning" => {
                        out.push(json!({"sessionUpdate": "agent_thought_chunk", "messageId": format!("{id}:reasoning:{reasoning}"), "content": {"type": "text", "text": s(&item, "text")}}));
                        reasoning += 1;
                    }
                    "tool" => {
                        let tid = s(&item, "id");
                        let name = s(&item, "name");
                        let state = item.get("state").cloned().unwrap_or(Value::Null);
                        let status = s(&state, "status");
                        let input = if status == "streaming" { json!({}) } else { state.get("input").cloned().unwrap_or(json!({})) };
                        let mut call = pending_tool_call(tid, name, &input, None, cwd);
                        call["sessionUpdate"] = json!("tool_call");
                        out.push(reorder_update(call));
                        let update = match status {
                            "completed" => Some(completed_update(tid, name, &input, state.get("content").unwrap_or(&json!([])), state.get("metadata"), cwd)),
                            "running" => Some(running_update(tid, name, &input, cwd)),
                            "error" => Some(error_update(tid, name, &input, state.get("content").unwrap_or(&json!([])), state.get("metadata"), state.pointer("/error/message").and_then(Value::as_str).unwrap_or(""), cwd)),
                            _ => None,
                        };
                        if let Some(mut u) = update {
                            u["sessionUpdate"] = json!("tool_call_update");
                            let u = reorder_update(u);
                            out.push(if status == "completed" { with_completed_diffs(u, name, &input, state.get("metadata"), cwd) } else { u });
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    out
}

/// `{sessionUpdate, ...rest}` key order.
fn reorder_update(v: Value) -> Value {
    let Value::Object(mut o) = v else { return v };
    let mut out = Map::new();
    if let Some(su) = o.remove("sessionUpdate") {
        out.insert("sessionUpdate".into(), su);
    }
    out.extend(o);
    Value::Object(out)
}

fn with_update(kind: &str, body: Value) -> Value {
    let mut o = Map::new();
    o.insert("sessionUpdate".into(), json!(kind));
    if let Value::Object(b) = body {
        o.extend(b);
    }
    Value::Object(o)
}

// ---------------------------------------------------------------------------------------------
// turn engine (`acp/turn.ts`, `acp/translate.ts`)

#[derive(Clone)]
struct Child {
    id: String,
    parent: String,
    depth: i64,
    title: Option<String>,
}

#[derive(Clone)]
struct Tracked {
    session: String,
    id: String,
    name: String,
    input: Value,
    metadata: Value,
}

#[derive(Default)]
struct TurnState {
    started: bool,
    tools: HashMap<String, Tracked>,
    retries: HashMap<String, Value>,
    compactions: HashMap<String, (String, Value)>,
    children: HashMap<String, Child>,
    asks: HashSet<String>,
    finish: Option<String>,
    usage_turn: Option<[i64; 5]>,
    usage_last: Option<[i64; 5]>,
    step_error: Option<Value>,
    execution_error: Option<Value>,
}

enum Out {
    Update(Value),
    Child(Value),
    Permission { event: Value, tool: Option<Tracked>, child: Option<Child> },
    Form { form: Value, child: Option<Child> },
    Settled(String),
}

fn tokens(t: &Value) -> [i64; 5] {
    let n = |p: &str| t.pointer(p).and_then(Value::as_f64).unwrap_or(0.0) as i64;
    [n("/input"), n("/output"), n("/reasoning"), n("/cache/read"), n("/cache/write")]
}

struct Ctx {
    session: String,
    cwd: String,
    start: String,
    caps: Caps,
}

fn child_meta(c: &Child) -> Value {
    let mut m = json!({"id": c.id, "parentID": c.parent, "depth": c.depth});
    if let Some(t) = c.title.as_ref().filter(|t| !t.is_empty()) {
        m["title"] = json!(t);
    }
    json!({"opencode/child-session": m})
}

fn child_update(ctx: &Ctx, c: &Child, event: Value) -> Value {
    let mut o = Map::new();
    o.insert("rootSessionId".into(), json!(ctx.session));
    o.insert("childSessionId".into(), json!(c.id));
    o.insert("parentSessionId".into(), json!(c.parent));
    o.insert("depth".into(), json!(c.depth));
    if let Some(t) = c.title.as_ref().filter(|t| !t.is_empty()) {
        o.insert("title".into(), json!(t));
    }
    if let Value::Object(e) = event {
        o.extend(e);
    }
    Value::Object(o)
}

fn route(ctx: &Ctx, child: Option<&Child>, update: Value) -> Vec<Out> {
    let Some(c) = child else { return vec![Out::Update(update)] };
    let mut p = update.as_object().cloned().unwrap_or_default();
    let mut meta = p.get("_meta").and_then(Value::as_object).cloned().unwrap_or_default();
    if let Value::Object(m) = child_meta(c) {
        meta.extend(m);
    }
    p.insert("_meta".into(), Value::Object(meta));
    let kind = p.get("sessionUpdate").and_then(Value::as_str).unwrap_or("").to_string();
    if kind == "tool_call" || kind == "tool_call_update" {
        if let Some(id) = p.get("toolCallId").and_then(Value::as_str) {
            let v = format!("{}:{id}", c.id);
            p.insert("toolCallId".into(), json!(v));
        }
        if let (Some(title), Some(ct)) = (p.get("title").and_then(Value::as_str).filter(|t| !t.is_empty()), c.title.as_ref().filter(|t| !t.is_empty())) {
            let v = format!("{ct}: {title}");
            p.insert("title".into(), json!(v));
        }
    }
    if ctx.caps.child_updates {
        return vec![Out::Child(child_update(ctx, c, json!({"type": "update", "update": Value::Object(p)})))];
    }
    vec![Out::Update(Value::Object(p))]
}

fn child_status(ctx: &Ctx, c: &Child, status: &str, error: Option<&Value>) -> Vec<Out> {
    if !ctx.caps.child_updates {
        return vec![];
    }
    let mut e = json!({"type": "status", "status": status});
    if let Some(err) = error {
        e["error"] = err.clone();
    }
    vec![Out::Child(child_update(ctx, c, e))]
}

fn event_session(event: &Value) -> Option<String> {
    let data = event.get("data")?;
    if let Some(sid) = data.get("sessionID").and_then(Value::as_str) {
        return Some(sid.into());
    }
    if s(event, "type") == "form.created" {
        return data.pointer("/form/sessionID").and_then(Value::as_str).map(String::from);
    }
    None
}

/// `fold(state, event)` → outputs and an optional terminal.
fn fold(st: &mut TurnState, event: &Value, ctx: &Ctx) -> (Vec<Out>, Option<&'static str>) {
    let ty = s(event, "type");
    let data = event.get("data").cloned().unwrap_or(Value::Null);
    if ty == "session.created" {
        let Some(parent) = data.get("parentID").and_then(Value::as_str) else { return (vec![], None) };
        let parent_child = st.children.get(parent).cloned();
        if parent_child.is_none() && parent != ctx.session {
            return (vec![], None);
        }
        let c = Child {
            id: s(&data, "sessionID").into(),
            parent: parent.into(),
            depth: parent_child.map(|p| p.depth + 1).unwrap_or(1),
            title: data.get("title").and_then(Value::as_str).map(String::from),
        };
        st.children.insert(c.id.clone(), c.clone());
        return (child_status(ctx, &c, "created", None), None);
    }
    let sid = event_session(event);
    let child = sid.as_ref().and_then(|s| st.children.get(s)).cloned();
    if ty == "permission.asked" && (sid.as_deref() == Some(ctx.session.as_str()) || child.is_some()) {
        st.asks.insert(s(&data, "id").into());
        let tool = data.pointer("/source/id").and_then(Value::as_str).and_then(|t| st.tools.get(&format!("{}:{t}", s(&data, "sessionID")))).cloned();
        return (vec![Out::Permission { event: event.clone(), tool, child }], None);
    }
    if ty == "form.created" && (sid.as_deref() == Some(ctx.session.as_str()) || child.is_some()) {
        let form = data.get("form").cloned().unwrap_or(Value::Null);
        st.asks.insert(s(&form, "id").into());
        return (vec![Out::Form { form, child }], None);
    }
    let settled = match ty {
        "permission.replied" => data.get("requestID").and_then(Value::as_str),
        "form.replied" | "form.cancelled" => data.get("id").and_then(Value::as_str),
        _ => None,
    };
    if let Some(id) = settled
        && st.asks.remove(id)
    {
        return (vec![Out::Settled(id.into())], None);
    }
    let Some(sid) = sid else { return (vec![], None) };
    if sid != ctx.session && child.is_none() {
        return (vec![], None);
    }
    if ty == "session.inbox.delivered" && s(&data, "inboxID") == ctx.start {
        st.started = true;
        return (vec![], None);
    }
    if !st.started {
        return (vec![], None);
    }
    if let Some(c) = &child {
        return match ty {
            "session.execution.started" => (child_status(ctx, c, "running", None), None),
            "session.execution.succeeded" => (child_status(ctx, c, "completed", None), None),
            "session.execution.interrupted" => (child_status(ctx, c, "interrupted", None), None),
            "session.execution.failed" => (child_status(ctx, c, "failed", data.get("error")), None),
            _ => (session_event(st, ty, &data, event, ctx, Some(c)), None),
        };
    }
    match ty {
        "session.step.started" => {
            st.step_error = None;
            (session_event(st, ty, &data, event, ctx, None), None)
        }
        "session.step.ended" => {
            record(st, data.get("tokens"));
            st.finish = data.get("finish").and_then(Value::as_str).map(String::from);
            (vec![], None)
        }
        "session.step.failed" => {
            if data.get("tokens").is_some_and(|t| !t.is_null()) {
                record(st, data.get("tokens"));
            }
            st.step_error = data.get("error").cloned();
            (vec![], None)
        }
        "session.execution.succeeded" => (vec![], Some("succeeded")),
        "session.execution.interrupted" => (vec![], Some("interrupted")),
        "session.execution.failed" => {
            st.execution_error = data.get("error").cloned();
            (vec![], Some("failed"))
        }
        _ => (session_event(st, ty, &data, event, ctx, None), None),
    }
}

fn record(st: &mut TurnState, t: Option<&Value>) {
    let t = tokens(t.unwrap_or(&Value::Null));
    st.usage_turn = Some(match st.usage_turn {
        Some(u) => [u[0] + t[0], u[1] + t[1], u[2] + t[2], u[3] + t[3], u[4] + t[4]],
        None => t,
    });
    st.usage_last = Some(t);
}

fn session_event(st: &mut TurnState, ty: &str, data: &Value, event: &Value, ctx: &Ctx, child: Option<&Child>) -> Vec<Out> {
    let sid = child.map(|c| c.id.clone()).unwrap_or_else(|| ctx.session.clone());
    let send = |u: Value| route(ctx, child, u);
    let standard = ctx.caps.compaction && (child.is_none() || ctx.caps.child_updates);
    let tool_key = |id: &str| format!("{}:{id}", s(data, "sessionID"));
    match ty {
        "session.step.started" => {
            if st.retries.remove(&sid).is_none() {
                return vec![];
            }
            send(json!({"sessionUpdate": "session_info_update", "_meta": {"opencode/retry": null}}))
        }
        "session.retry.scheduled" => {
            let at = data.get("at").and_then(Value::as_i64).unwrap_or(0);
            let retry = json!({
                "attempt": data.get("attempt"),
                "nextRetryAt": chrono::DateTime::from_timestamp_millis(at).map(|d| d.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()).unwrap_or_default(),
                "error": data.get("error"),
            });
            st.retries.insert(sid, retry.clone());
            send(json!({"sessionUpdate": "session_info_update", "_meta": {"opencode/retry": retry}}))
        }
        "session.compaction.started" | "session.compaction.ended" | "session.compaction.failed" => {
            let reason = data.get("reason").cloned().unwrap_or(Value::Null);
            let open_id = || data.get("inputID").and_then(Value::as_str).map(String::from).unwrap_or_else(|| oc_schema::id::message_from_event(s(event, "id")));
            match ty {
                "session.compaction.started" => {
                    let id = open_id();
                    st.compactions.insert(sid, (id.clone(), reason.clone()));
                    send(compaction_update("started", &id, &reason, None, None, standard))
                }
                "session.compaction.ended" => match st.compactions.remove(&sid) {
                    None => vec![],
                    Some((id, _)) => send(compaction_update("completed", &id, &reason, data.get("text").and_then(Value::as_str), None, standard)),
                },
                _ => {
                    let current = st.compactions.remove(&sid);
                    let (id, opened) = match current {
                        Some((id, _)) => (id, true),
                        None => (open_id(), false),
                    };
                    let mut out = vec![];
                    if !opened && standard {
                        out.extend(send(compaction_update("started", &id, &reason, None, None, standard)));
                    }
                    out.extend(send(compaction_update("failed", &id, &reason, None, data.get("error"), standard)));
                    out
                }
            }
        }
        "session.compaction.delta" => match st.compactions.get(&sid) {
            Some((id, _)) if standard => send(json!({"sessionUpdate": "compaction_summary_chunk", "compactionId": id, "content": {"type": "text", "text": s(data, "text")}})),
            _ => vec![],
        },
        "session.text.delta" => send(json!({"sessionUpdate": "agent_message_chunk", "messageId": s(data, "assistantMessageID"), "content": {"type": "text", "text": s(data, "delta")}})),
        "session.reasoning.delta" => send(json!({
            "sessionUpdate": "agent_thought_chunk",
            "messageId": format!("{}:reasoning:{}", s(data, "assistantMessageID"), data.get("ordinal").cloned().unwrap_or(Value::Null)),
            "content": {"type": "text", "text": s(data, "delta")},
        })),
        "session.tool.input.started" => {
            let id = s(data, "id");
            st.tools.insert(tool_key(id), Tracked { session: s(data, "sessionID").into(), id: id.into(), name: s(data, "name").into(), input: json!({}), metadata: json!({}) });
            send(with_update("tool_call", pending_tool_call(id, s(data, "name"), &json!({}), None, &ctx.cwd)))
        }
        "session.tool.called" => {
            let id = s(data, "id");
            let k = tool_key(id);
            let mut t = st.tools.get(&k).cloned().unwrap_or(Tracked { session: s(data, "sessionID").into(), id: id.into(), name: "tool".into(), input: json!({}), metadata: json!({}) });
            t.input = data.get("input").cloned().unwrap_or(json!({}));
            st.tools.insert(k, t.clone());
            send(with_update("tool_call_update", running_update(id, &t.name, &t.input, &ctx.cwd)))
        }
        "session.tool.progress" => {
            let id = s(data, "id");
            let k = tool_key(id);
            let Some(t) = st.tools.get_mut(&k) else { return vec![] };
            t.metadata = data.get("metadata").cloned().unwrap_or(json!({}));
            let t = t.clone();
            send(with_update("tool_call_update", running_update(id, &t.name, &t.input, &ctx.cwd)))
        }
        "session.tool.success" => {
            let id = s(data, "id");
            let t = st.tools.remove(&tool_key(id)).unwrap_or(Tracked { session: sid.clone(), id: id.into(), name: "tool".into(), input: json!({}), metadata: json!({}) });
            let update = with_update("tool_call_update", completed_update(id, &t.name, &t.input, data.get("content").unwrap_or(&json!([])), data.get("metadata"), &ctx.cwd));
            send(with_completed_diffs(update, &t.name, &t.input, data.get("metadata"), &ctx.cwd))
        }
        "session.tool.failed" => {
            let id = s(data, "id");
            let t = st.tools.remove(&tool_key(id)).unwrap_or(Tracked { session: sid.clone(), id: id.into(), name: "tool".into(), input: json!({}), metadata: json!({}) });
            let metadata = data.get("metadata").filter(|m| !m.is_null()).cloned().unwrap_or(t.metadata.clone());
            let content = data.get("content").filter(|c| !c.is_null()).cloned().unwrap_or(json!([]));
            send(with_update(
                "tool_call_update",
                error_update(id, &t.name, &t.input, &content, Some(&metadata), data.pointer("/error/message").and_then(Value::as_str).unwrap_or(""), &ctx.cwd),
            ))
        }
        _ => vec![],
    }
}

fn abandon(st: &mut TurnState, ctx: &Ctx) -> Vec<Out> {
    let mut out = vec![];
    for t in std::mem::take(&mut st.tools).into_values() {
        let child = st.children.get(&t.session).cloned();
        out.extend(route(ctx, child.as_ref(), with_update("tool_call_update", error_update(&t.id, &t.name, &t.input, &json!([]), Some(&t.metadata), "Cancelled", &ctx.cwd))));
    }
    if let Some((id, reason)) = st.compactions.remove(&ctx.session) {
        out.extend(route(ctx, None, compaction_update("failed", &id, &reason, None, Some(&json!({"type": "aborted", "message": "Compaction cancelled"})), ctx.caps.compaction)));
    }
    out
}

/// One queued ask (permission or form) with its fallback.
enum Ask {
    Permission { event: Value, tool: Option<Tracked>, child: Option<Child> },
    Form { form: Value, child: Option<Child>, schema: Value },
    Barrier(oneshot::Sender<()>),
}

struct AskShared {
    settled: Mutex<HashSet<String>>,
    notify: tokio::sync::Notify,
}

impl Agent {
    async fn prompt(self: Arc<Self>, id: &str, blocks: Vec<Value>) -> R<Value> {
        let (cwd, sel) = self.selection(id)?;
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let (done_tx, done_rx) = watch::channel(false);
        {
            let mut turns = self.turns.lock();
            if turns.contains_key(id) {
                return Err(AcpError::service_failure(&format!("Session already has an active ACP prompt: {id}"), Some("session"), None));
            }
            turns.insert(id.to_string(), Arc::new(TurnCtl { cancel: cancel_tx, done: done_rx }));
        }
        let result = self.clone().run_turn(id, &cwd, &sel, blocks, cancel_rx).await;
        self.turns.lock().remove(id);
        let _ = done_tx.send(true);
        result
    }

    async fn run_turn(self: Arc<Self>, id: &str, cwd: &str, sel: &Selection, blocks: Vec<Value>, mut cancelled: watch::Receiver<bool>) -> R<Value> {
        let catalog = self.catalog(cwd).await?;
        let prepared = prepare(&catalog, &blocks)?;
        let caps = *self.caps.lock();
        let cancelled_result = || Ok(json!({"stopReason": "cancelled", "_meta": {}}));
        if *cancelled.borrow() {
            return cancelled_result();
        }
        let mut stream: EventStream = self.client.subscribe().await.map_err(|_| AcpError::server_unavailable())?;
        match stream.next().await {
            Some(Ok(e)) if s(&e, "type") == "server.connected" => {}
            Some(Ok(e)) => {
                tracing::warn!(kind = s(&e, "type"), "expected server.connected");
                return Err(AcpError::defect());
            }
            _ => return Err(AcpError::server_unavailable()),
        }
        if *cancelled.borrow() {
            return cancelled_result();
        }
        let ctx = Ctx { session: id.to_string(), cwd: cwd.to_string(), start: prepared.start_id.clone(), caps };
        // ask worker
        let shared = Arc::new(AskShared { settled: Mutex::new(HashSet::new()), notify: tokio::sync::Notify::new() });
        let (ask_tx, ask_rx) = mpsc::unbounded_channel::<Ask>();
        let worker = tokio::spawn(self.clone().ask_worker(id.to_string(), cwd.to_string(), ask_rx, shared.clone(), cancelled.clone(), caps));
        // submit
        let c = &self.client;
        if !prepared.synthetic.is_empty() {
            c.call("session.synthetic", Req::new().path("sessionID", id).body(json!({"text": prepared.synthetic.join("\n\n"), "description": "ACP embedded context", "delivery": "steer", "resume": false})))
                .await
                .map_err(classify)?;
        }
        if prepared.start_compaction {
            c.call("session.compact", Req::new().path("sessionID", id).body(json!({"id": prepared.start_id}))).await.map_err(classify)?;
        } else if let Some(cmd) = &prepared.command {
            let args = prepared.slash.as_ref().map(|(_, a)| a.clone()).unwrap_or_default();
            c.call("session.command", Req::new().path("sessionID", id).body(json!({"name": cmd.get("name"), "text": args, "files": prepared.files, "delivery": "steer"})))
                .await
                .map_err(classify)?;
            drop(ask_tx);
            let _ = worker.await;
            return Ok(json!({"stopReason": "end_turn", "_meta": {}}));
        } else {
            c.call("session.prompt", Req::new().path("sessionID", id).body(json!({"id": prepared.start_id, "text": prepared.text, "files": prepared.files, "delivery": "steer"})))
                .await
                .map_err(classify)?;
        }
        let mut st = TurnState::default();
        let emit = |outs: Vec<Out>, st_asks: &AskShared| {
            for o in outs {
                match o {
                    Out::Update(u) => self.conn.update(id, u),
                    Out::Child(u) => self.conn.notify(CHILD_UPDATE_METHOD, u),
                    Out::Permission { event, tool, child } => {
                        let _ = ask_tx.send(Ask::Permission { event, tool, child });
                    }
                    Out::Form { form, child } => match form_schema(&form, caps) {
                        Some(schema) => {
                            let _ = ask_tx.send(Ask::Form { form, child, schema });
                        }
                        None => {
                            let client = self.client.clone();
                            tokio::spawn(async move { cancel_form(&client, &form, form.pointer("/metadata/kind").and_then(Value::as_str) == Some("question")).await });
                        }
                    },
                    Out::Settled(sid) => {
                        st_asks.settled.lock().insert(sid);
                        st_asks.notify.notify_waiters();
                    }
                }
            }
        };
        let mut interrupted = false;
        let terminal: &str = loop {
            tokio::select! {
                next = stream.next() => match next {
                    Some(Ok(event)) => {
                        let (outs, term) = fold(&mut st, &event, &ctx);
                        emit(outs, &shared);
                        if let Some(t) = term { break t; }
                    }
                    _ => return Err(AcpError::server_unavailable()),
                },
                _ = cancelled.changed() => {
                    interrupted = true;
                    if let Err(e) = self.client.call("session.interrupt", Req::new().path("sessionID", id)).await {
                        tracing::warn!(cause = %e, "ACP server interrupt failed");
                    }
                    if !st.started { break "interrupted"; }
                    let drained = tokio::time::timeout(Duration::from_secs(5), async {
                        while let Some(Ok(event)) = stream.next().await {
                            let (outs, term) = fold(&mut st, &event, &ctx);
                            emit(outs, &shared);
                            if term.is_some() { return true; }
                        }
                        false
                    }).await;
                    if !matches!(drained, Ok(true)) {
                        let outs = abandon(&mut st, &ctx);
                        emit(outs, &shared);
                    }
                    break "interrupted";
                }
            }
        };
        // every queued ask answers before the prompt responds
        let (btx, brx) = oneshot::channel();
        let _ = ask_tx.send(Ask::Barrier(btx));
        let _ = brx.await;
        drop(ask_tx);
        let terminal = if interrupted { "interrupted" } else { terminal };
        if terminal != "interrupted"
            && let Some(error) = st.step_error.clone().or(st.execution_error.clone())
        {
            let ty = s(&error, "type");
            if ty == "provider.auth" {
                return Err(AcpError::auth_required());
            }
            if ty != "aborted" && ty != "provider.content-filter" {
                let msg = s(&error, "message");
                return Err(AcpError::service_failure(if msg.is_empty() { "OpenCode prompt failed" } else { msg }, Some("session"), Some(ty)));
            }
        }
        // usage update
        if let Some(last) = st.usage_last {
            let used: i64 = last.iter().sum();
            let model = sel.model.clone().or_else(|| self.selection(id).ok().and_then(|(_, s)| s.model)).unwrap_or(catalog.default_model.clone());
            let size = find_model(&catalog, s(&model, "providerID"), s(&model, "id")).and_then(|m| m.pointer("/limit/context")).and_then(Value::as_f64).filter(|c| *c > 0.0);
            if used > 0
                && let Some(size) = size
            {
                match self.client.call("session.get", Req::new().path("sessionID", id)).await {
                    Ok(v) => {
                        let cost = v.pointer("/data/cost").cloned().unwrap_or(json!(0));
                        self.conn.update(id, json!({"sessionUpdate": "usage_update", "used": used, "size": size as i64, "cost": {"amount": cost, "currency": "USD"}}));
                    }
                    Err(e) => tracing::warn!(cause = %e, "ACP usage update failed"),
                }
            }
        }
        let error_type = st.step_error.as_ref().or(st.execution_error.as_ref()).map(|e| s(e, "type").to_string());
        let stop = if terminal == "interrupted" || error_type.as_deref() == Some("aborted") {
            "cancelled"
        } else if st.finish.as_deref() == Some("length") {
            "max_tokens"
        } else if st.finish.as_deref() == Some("content-filter") || error_type.as_deref() == Some("provider.content-filter") {
            "refusal"
        } else {
            "end_turn"
        };
        let mut response = json!({"stopReason": stop});
        if let Some(t) = st.usage_turn {
            let mut usage = json!({"inputTokens": t[0], "outputTokens": t[1], "totalTokens": t.iter().sum::<i64>()});
            if t[2] > 0 {
                usage["thoughtTokens"] = json!(t[2]);
            }
            if t[3] > 0 {
                usage["cachedReadTokens"] = json!(t[3]);
            }
            if t[4] > 0 {
                usage["cachedWriteTokens"] = json!(t[4]);
            }
            response["usage"] = usage;
        }
        response["_meta"] = match st.retries.get(id) {
            Some(r) => json!({"opencode/retry": r}),
            None => json!({}),
        };
        Ok(response)
    }

    async fn ask_worker(self: Arc<Self>, root: String, cwd: String, mut rx: mpsc::UnboundedReceiver<Ask>, shared: Arc<AskShared>, cancelled: watch::Receiver<bool>, caps: Caps) {
        while let Some(ask) = rx.recv().await {
            let (ask_id, fallback, method, params) = match &ask {
                Ask::Barrier(_) => {
                    if let Ask::Barrier(tx) = ask {
                        let _ = tx.send(());
                    }
                    continue;
                }
                Ask::Permission { event, tool, child } => {
                    let data = event.get("data").cloned().unwrap_or(Value::Null);
                    (s(&data, "id").to_string(), "reject", "session/request_permission", permission_params(&root, &cwd, &data, tool.as_ref(), child.as_ref()))
                }
                Ask::Form { form, child, schema } => {
                    let tool_sent = child.is_none() || !caps.child_updates;
                    let mut p = json!({"mode": "form", "sessionId": root});
                    if tool_sent && let Some(t) = form.pointer("/metadata/tool/id").and_then(Value::as_str) {
                        p["toolCallId"] = json!(match child { Some(c) => format!("{}:{t}", c.id), None => t.to_string() });
                    }
                    let title = s(form, "title");
                    p["message"] = json!(match child.as_ref().and_then(|c| c.title.as_ref()).filter(|t| !t.is_empty()) { Some(ct) => format!("{ct}: {title}"), None => title.to_string() });
                    p["requestedSchema"] = schema.clone();
                    (s(form, "id").to_string(), "cancel", "elicitation/create", p)
                }
            };
            let is_settled = || shared.settled.lock().contains(&ask_id);
            let outcome: Option<Value> = if *cancelled.borrow() {
                None
            } else if is_settled() {
                continue;
            } else {
                let (rid, response) = self.conn.request(method, params);
                let mut c2 = cancelled.clone();
                let notified = shared.notify.notified();
                tokio::pin!(notified);
                let mut response = response;
                loop {
                    tokio::select! {
                        r = &mut response => break r.ok().and_then(|r| r.ok()),
                        _ = c2.changed() => { self.conn.cancel_request(rid); break None; }
                        _ = &mut notified => {
                            if is_settled() { self.conn.cancel_request(rid); break Some(json!({"__settled": true})); }
                            notified.set(shared.notify.notified());
                        }
                    }
                }
            };
            if outcome.as_ref().is_some_and(|o| o.get("__settled").is_some()) {
                continue;
            }
            match ask {
                Ask::Permission { event, .. } => {
                    let data = event.get("data").cloned().unwrap_or(Value::Null);
                    let decision = outcome
                        .as_ref()
                        .filter(|o| o.pointer("/outcome/outcome").and_then(Value::as_str) == Some("selected"))
                        .and_then(|o| o.pointer("/outcome/optionId").and_then(Value::as_str))
                        .filter(|d| *d == "once" || *d == "always")
                        .unwrap_or(fallback);
                    let _ = self
                        .client
                        .call("session.permission.reply", Req::new().path("sessionID", s(&data, "sessionID")).path("requestID", s(&data, "id")).body(json!({"decision": decision})))
                        .await;
                }
                Ask::Form { form, .. } => {
                    let answer = outcome.as_ref().filter(|o| s(o, "action") == "accept").and_then(|o| form_answer(&form, o.get("content").unwrap_or(&json!({}))));
                    match answer {
                        Some(a) => {
                            let r = self
                                .client
                                .call("session.form.reply", Req::new().path("sessionID", s(&form, "sessionID")).path("formID", s(&form, "id")).body(json!({"answer": a})))
                                .await;
                            if let Err(e) = r
                                && !matches!(e.tag(), Some("FormAlreadySettledError") | Some("FormNotFoundError"))
                            {
                                tracing::warn!(cause = %e, "ACP form reply failed");
                                cancel_form(&self.client, &form, false).await;
                            }
                        }
                        None => cancel_form(&self.client, &form, false).await,
                    }
                }
                Ask::Barrier(_) => {}
            }
        }
    }
}

async fn cancel_form(client: &Client, form: &Value, unshown_question: bool) {
    let mut req = Req::new().path("sessionID", s(form, "sessionID")).path("formID", s(form, "id"));
    if unshown_question {
        req = req.query("message", json!(UNSHOWN_QUESTION));
    }
    match client.call("session.form.cancel", req).await {
        Ok(_) => {}
        Err(e) if matches!(e.tag(), Some("FormAlreadySettledError") | Some("FormNotFoundError")) => {}
        Err(_) => {
            let _ = client.call("session.interrupt", Req::new().path("sessionID", s(form, "sessionID"))).await;
        }
    }
}

fn permission_params(root: &str, cwd: &str, data: &Value, tool: Option<&Tracked>, child: Option<&Child>) -> Value {
    let tool_name = tool.map(|t| t.name.clone()).unwrap_or_else(|| s(data, "action").to_string());
    let input = tool.map(|t| t.input.clone()).or_else(|| data.get("metadata").filter(|m| m.is_object()).cloned()).unwrap_or(json!({}));
    let str_of = |k: &str| input.get(k).and_then(Value::as_str).map(String::from);
    let previews = permission_previews(&tool_name, &input, data.get("metadata"), cwd);
    let title = match canonical(&tool_name).as_str() {
        _ if previews.len() > 1 => Some(format!("{} files", previews.len())),
        "external_directory" => str_of("description").or_else(|| str_of("command")).or_else(|| str_of("parentDir")),
        "webfetch" => str_of("url"),
        "websearch" => str_of("query"),
        "grep" | "glob" => str_of("pattern"),
        "read" | "edit" | "write" | "patch" => str_of("path").or_else(|| str_of("filePath")).or_else(|| previews.first().map(|p| s(p, "path").to_string())),
        _ => None,
    };
    let prefixed = |id: &str| match child {
        Some(c) => format!("{}:{id}", c.id),
        None => id.to_string(),
    };
    let title = title.map(|t| match child.and_then(|c| c.title.as_ref()).filter(|t| !t.is_empty()) {
        Some(ct) => format!("{ct}: {t}"),
        None => t,
    });
    let call_id = prefixed(tool.map(|t| t.id.as_str()).unwrap_or(s(data, "id")));
    let mut call = pending_tool_call(&call_id, &tool_name, &input, title.as_deref().or(child.and_then(|c| c.title.as_deref())), cwd);
    let mut locs = locations(&tool_name, &input, cwd);
    if locs.as_array().is_some_and(|a| a.is_empty()) && ["read", "edit", "external_directory"].contains(&s(data, "action")) {
        let mut seen: Vec<String> = vec![];
        for r in data.get("resources").and_then(Value::as_array).cloned().unwrap_or_default() {
            let r = r.as_str().unwrap_or("").trim_end_matches("/*").to_string();
            if r.is_empty() || r.contains('*') || r.contains('?') {
                continue;
            }
            let a = absolutize(&r, cwd);
            if !seen.contains(&a) {
                seen.push(a);
            }
        }
        locs = Value::Array(seen.into_iter().map(|p| json!({"path": p})).collect());
    }
    let o = call.as_object_mut().unwrap();
    match tool {
        Some(t) => {
            o.insert("name".into(), json!(t.name));
        }
        None => {
            o.remove("name");
            o.remove("rawInput");
        }
    }
    o.insert("locations".into(), locs);
    if !previews.is_empty() {
        o.insert("content".into(), Value::Array(previews));
    }
    if let Some(c) = child {
        o.insert("_meta".into(), child_meta(c));
    }
    json!({
        "sessionId": root,
        "toolCall": call,
        "options": [
            {"optionId": "once", "kind": "allow_once", "name": "Allow once"},
            {"optionId": "always", "kind": "allow_always", "name": "Always allow"},
            {"optionId": "reject", "kind": "reject_once", "name": "Reject"},
        ],
    })
}

/// `requestedSchema(form, caps)`
fn form_schema(form: &Value, caps: Caps) -> Option<Value> {
    if !caps.form_elicitation {
        return None;
    }
    let kind = form.pointer("/metadata/kind").and_then(Value::as_str)?;
    if kind != "question" && kind != "websearch.provider" {
        return None;
    }
    let fields = form.get("fields").and_then(Value::as_array).cloned().unwrap_or_default();
    let credential = regex::Regex::new(r"(?i)password|passphrase|secret|token|api[_ -]?key|credential|private[_ -]?key").unwrap();
    for f in &fields {
        let mut texts = vec![s(f, "key").to_string(), s(f, "title").to_string(), s(f, "description").to_string()];
        if matches!(s(f, "type"), "string" | "multiselect") {
            texts.extend(f.get("options").and_then(Value::as_array).cloned().unwrap_or_default().iter().map(|o| s(o, "label").to_string()));
        }
        if texts.iter().any(|t| !t.is_empty() && credential.is_match(t)) {
            return None;
        }
        if s(f, "type") == "external" || f.get("when").and_then(Value::as_array).is_some_and(|w| !w.is_empty()) {
            return None;
        }
        if f.get("hidden") == Some(&json!(true)) && f.get("required") == Some(&json!(true)) && f.get("default").is_none() {
            return None;
        }
    }
    let keys: HashSet<String> = fields.iter().map(|f| s(f, "key").to_string()).collect();
    let visible: Vec<&Value> = fields.iter().filter(|f| f.get("hidden") != Some(&json!(true))).collect();
    let has_options = |f: &Value| s(f, "type") == "multiselect" || f.get("options").is_some();
    for f in &visible {
        if !matches!(s(f, "type"), "string" | "multiselect") || !has_options(f) {
            continue;
        }
        let values: HashSet<String> = f.get("options").and_then(Value::as_array).cloned().unwrap_or_default().iter().map(|o| s(o, "value").to_string()).collect();
        let defaults: Vec<String> = match f.get("default") {
            Some(Value::String(d)) => vec![d.clone()],
            Some(Value::Array(a)) => a.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
            _ => vec![],
        };
        if defaults.iter().any(|d| !values.contains(d)) {
            return None;
        }
        if f.get("custom") == Some(&json!(true)) {
            if f.get("required") == Some(&json!(true)) || keys.contains(&format!("{}_custom", s(f, "key"))) {
                return None;
            }
            if s(f, "type") == "multiselect" && (f.get("minItems").is_some() || f.get("maxItems").is_some()) {
                return None;
            }
        }
    }
    let mut properties = Map::new();
    let clean = |v: Value| -> Value {
        match v {
            Value::Object(o) => Value::Object(o.into_iter().filter(|(_, v)| !v.is_null()).collect()),
            v => v,
        }
    };
    for f in &visible {
        let key = s(f, "key").to_string();
        let required = f.get("required") == Some(&json!(true));
        let base = |extra: Value| {
            let mut o = json!({"title": f.get("title"), "description": f.get("description")});
            if let Value::Object(e) = extra {
                o.as_object_mut().unwrap().extend(e);
            }
            o
        };
        let string_constraints = || {
            let min = f.get("minLength").and_then(Value::as_i64);
            json!({"format": f.get("format"), "minLength": if required { Some(min.unwrap_or(0).max(1)) } else { min }, "maxLength": f.get("maxLength"), "pattern": f.get("pattern")})
        };
        let options = || -> Value {
            Value::Array(f.get("options").and_then(Value::as_array).cloned().unwrap_or_default().iter().map(|o| clean(json!({"const": o.get("value"), "title": o.get("label"), "description": o.get("description")}))).collect())
        };
        let other = |desc: &str| {
            let mut o = json!({"type": "string", "title": format!("{} (other)", f.get("title").and_then(Value::as_str).unwrap_or(&key)), "description": desc});
            if s(f, "type") == "string" {
                if let Value::Object(c) = string_constraints() {
                    o.as_object_mut().unwrap().extend(c);
                }
            }
            clean(o)
        };
        match s(f, "type") {
            "string" if !has_options(f) => {
                let mut p = base(json!({"type": "string"}));
                if let Value::Object(c) = string_constraints() {
                    p.as_object_mut().unwrap().extend(c);
                }
                p["default"] = f.get("default").cloned().unwrap_or(Value::Null);
                properties.insert(key.clone(), clean(reorder_type(p)));
            }
            "string" => {
                let p = base(json!({"type": "string", "oneOf": options(), "default": f.get("default")}));
                properties.insert(key.clone(), clean(reorder_type(p)));
                if f.get("custom") == Some(&json!(true)) {
                    properties.insert(format!("{key}_custom"), other("Type your own answer"));
                }
            }
            "multiselect" => {
                let min = f.get("minItems").and_then(Value::as_i64);
                let p = base(json!({"type": "array", "items": {"anyOf": options()}, "minItems": if required { Some(min.unwrap_or(0).max(1)) } else { min }, "maxItems": f.get("maxItems"), "default": f.get("default")}));
                properties.insert(key.clone(), clean(reorder_type(p)));
                if f.get("custom") == Some(&json!(true)) {
                    properties.insert(format!("{key}_custom"), other("Add your own answer"));
                }
            }
            "number" | "integer" => {
                let p = base(json!({"type": s(f, "type"), "minimum": f.get("minimum"), "maximum": f.get("maximum"), "default": f.get("default")}));
                properties.insert(key.clone(), clean(reorder_type(p)));
            }
            _ => {
                let p = base(json!({"type": "boolean", "default": f.get("default")}));
                properties.insert(key.clone(), clean(reorder_type(p)));
            }
        }
    }
    Some(json!({"type": "object", "properties": properties, "required": visible.iter().filter(|f| f.get("required") == Some(&json!(true))).map(|f| f.get("key")).collect::<Vec<_>>()}))
}

fn reorder_type(v: Value) -> Value {
    let Value::Object(mut o) = v else { return v };
    let mut out = Map::new();
    if let Some(t) = o.remove("type") {
        out.insert("type".into(), t);
    }
    out.extend(o);
    Value::Object(out)
}

/// `answer(form, response.content)`
fn form_answer(form: &Value, content: &Value) -> Option<Value> {
    let content = content.as_object()?;
    let mut out = Map::new();
    for f in form.get("fields").and_then(Value::as_array).cloned().unwrap_or_default() {
        let key = s(&f, "key").to_string();
        let value = if s(&f, "type") != "external" && f.get("hidden") == Some(&json!(true)) {
            f.get("default").cloned()
        } else {
            let value = content.get(&key).cloned();
            let selectable = matches!(s(&f, "type"), "string" | "multiselect") && f.get("custom") == Some(&json!(true)) && (s(&f, "type") == "multiselect" || f.get("options").is_some());
            match content.get(&format!("{key}_custom")).and_then(Value::as_str).filter(|c| selectable && !c.trim().is_empty()) {
                None => value,
                Some(custom) if s(&f, "type") == "string" => Some(json!(custom)),
                Some(custom) => match value {
                    Some(Value::Array(mut a)) => {
                        a.push(json!(custom));
                        Some(Value::Array(a))
                    }
                    _ => Some(json!([custom])),
                },
            }
        };
        if let Some(v) = value.filter(|v| !v.is_null()) {
            out.insert(key, v);
        }
    }
    Some(Value::Object(out))
}

// ---------------------------------------------------------------------------------------------
// selection / catalog event subscription (`acp/sessions.ts`, `acp/catalog.ts`)

async fn watch_events(agent: Arc<Agent>) {
    let Ok(mut stream) = agent.client.subscribe().await else {
        tracing::warn!("ACP selection event stream failed");
        return;
    };
    while let Some(Ok(event)) = stream.next().await {
        let ty = s(&event, "type").to_string();
        let data = event.get("data").cloned().unwrap_or(Value::Null);
        match ty.as_str() {
            "session.model.selected" | "session.agent.selected" => {
                let sid = s(&data, "sessionID").to_string();
                let before = {
                    let mut sessions = agent.sessions.lock();
                    let Some(a) = sessions.get_mut(&sid) else { continue };
                    let before = (a.cwd.clone(), a.selection.clone());
                    if ty == "session.model.selected" {
                        a.selection.model = data.get("model").cloned();
                    } else {
                        a.selection.mode = data.get("agent").and_then(Value::as_str).map(String::from);
                    }
                    (before, a.selection.clone())
                };
                let ((cwd, old), new) = before;
                if let Ok(catalog) = agent.catalog(&cwd).await {
                    let (a, b) = (config_options(&catalog, &old), config_options(&catalog, &new));
                    if a != b {
                        agent.conn.update(&sid, json!({"sessionUpdate": "config_option_update", "configOptions": b}));
                    }
                }
            }
            "model.updated" | "agent.updated" | "command.updated" => {
                let dir = event.pointer("/location/directory").and_then(Value::as_str).map(fs_resolve);
                let attached: Vec<(String, String, Selection)> = agent.sessions.lock().iter().map(|(id, a)| (id.clone(), a.cwd.clone(), a.selection.clone())).collect();
                let cwds: HashSet<String> = attached.iter().map(|(_, c, _)| c.clone()).filter(|c| dir.as_ref().is_none_or(|d| fs_resolve(c) == *d)).collect();
                for cwd in cwds {
                    let previous = agent.catalogs.lock().await.get(&fs_resolve(&cwd)).cloned();
                    let Ok(next) = agent.reload(&cwd).await else { continue };
                    let Some(previous) = previous else { continue };
                    for (id, c, sel) in attached.iter().filter(|(_, c, _)| *c == cwd) {
                        let _ = c;
                        let (a, b) = (config_options(&previous, sel), config_options(&next, sel));
                        if a != b {
                            agent.conn.update(id, json!({"sessionUpdate": "config_option_update", "configOptions": b}));
                        }
                        if previous.commands != next.commands {
                            agent.conn.update(id, json!({"sessionUpdate": "available_commands_update", "availableCommands": available_commands(&next)}));
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------------------------
// entry (`handlers/acp.ts`)

pub async fn run(global: &Global, input: &Input) -> anyhow::Result<i32> {
    if input.flag("login") {
        return Ok(crate::commands::auth::handle(
            crate::commands::auth::login_flow(global, None, None, vec![], crate::connection::Args::default()).await,
        ));
    }
    unsafe { std::env::set_var("OPENCODE_CLIENT", "acp") };
    let (standalone, endpoint) = Standalone::start(&crate::connection::startup_dir())?;
    let standalone = Arc::new(Mutex::new(standalone));
    let conn = Arc::new(Conn { out: Mutex::new(std::io::stdout()), next_id: AtomicI64::new(0), pending: Mutex::new(HashMap::new()) });
    let agent = Arc::new(Agent {
        conn: conn.clone(),
        client: Client::new(&endpoint),
        caps: Mutex::new(Caps::default()),
        catalogs: tokio::sync::Mutex::new(HashMap::new()),
        sessions: Mutex::new(HashMap::new()),
        mcp: Mutex::new(HashMap::new()),
        turns: Mutex::new(HashMap::new()),
        prompt_requests: Mutex::new(HashMap::new()),
    });
    tokio::spawn(watch_events(agent.clone()));

    // stdin reader (blocking thread)
    let (line_tx, mut line_rx) = mpsc::unbounded_channel::<Result<String, String>>();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut reader = std::io::BufReader::new(stdin.lock());
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if buf.len() > MAX_MESSAGE_BYTES {
                        let _ = line_tx.send(Err(format!("incoming message exceeded the {} MiB limit", MAX_MESSAGE_BYTES / 1024 / 1024)));
                        return;
                    }
                    let line = String::from_utf8_lossy(&buf).trim().to_string();
                    if !line.is_empty() {
                        let _ = line_tx.send(Ok(line));
                    }
                }
            }
        }
    });

    let exited = {
        let standalone = standalone.clone();
        async move {
            loop {
                tokio::time::sleep(Duration::from_millis(50)).await;
                let status = standalone.lock().child_mut().try_wait();
                match status {
                    Ok(Some(s)) => {
                        #[cfg(unix)]
                        {
                            use std::os::unix::process::ExitStatusExt;
                            if let Some(sig) = s.signal() {
                                return format!("signal {}", crate::service::signal_name(sig));
                            }
                        }
                        return format!("code {}", s.code().unwrap_or(1));
                    }
                    Ok(None) => {}
                    Err(e) => return e.to_string(),
                }
            }
        }
    };
    let read = async {
        while let Some(line) = line_rx.recv().await {
            let line = match line {
                Ok(l) => l,
                Err(e) => return Some(e),
            };
            let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                conn.send(&json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "Parse error"}}));
                continue;
            };
            let method = msg.get("method").and_then(Value::as_str).map(String::from);
            let id = msg.get("id").cloned();
            match (method, id) {
                (Some(method), Some(id)) => {
                    let agent = agent.clone();
                    let params = msg.get("params").cloned().unwrap_or(json!({}));
                    tokio::spawn(async move {
                        let result = agent.handle(&method, params, &id).await;
                        let ok = result.is_ok();
                        agent.conn.respond(&id, result);
                        let after = AFTER.lock().remove(&id.to_string());
                        if ok && let Some((sid, catalog)) = after {
                            agent.after_attach(&sid, &catalog);
                        }
                    });
                }
                (Some(method), None) => {
                    let params = msg.get("params").cloned().unwrap_or(Value::Null);
                    match method.as_str() {
                        "session/cancel" => {
                            let a = agent.clone();
                            let sid = s(&params, "sessionId").to_string();
                            tokio::spawn(async move { a.cancel_turn(&sid, false).await });
                        }
                        "$/cancel_request" => {
                            let rid = params.get("requestId").cloned().unwrap_or(Value::Null).to_string();
                            let sid = agent.prompt_requests.lock().get(&rid).cloned();
                            if let Some(sid) = sid {
                                let a = agent.clone();
                                tokio::spawn(async move { a.cancel_turn(&sid, false).await });
                            }
                        }
                        _ => {}
                    }
                }
                (None, Some(id)) => {
                    if let Some(n) = id.as_i64()
                        && let Some(tx) = conn.pending.lock().remove(&n)
                    {
                        let _ = tx.send(match msg.get("error") {
                            Some(e) => Err(e.clone()),
                            None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                        });
                    }
                }
                (None, None) => {}
            }
        }
        None
    };
    let failure = tokio::select! {
        f = read => f,
        reason = exited => Some(format!("server exited unexpectedly ({reason})")),
    };
    if let Some(f) = failure {
        crate::term::write_stderr(&format!("ocd acp: {f}\n"));
        std::process::exit(1);
    }
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> Catalog {
        Catalog {
            models: vec![
                json!({"providerID": "b", "id": "m2", "name": "Two", "variants": [{"id": "high"}, {"id": "x-high"}]}),
                json!({"providerID": "a", "id": "m1", "name": "One"}),
            ],
            default_model: json!({"providerID": "a", "id": "m1"}),
            modes: vec![json!({"id": "build", "name": "Build", "description": ""}), json!({"id": "plan", "name": "Plan", "description": "Plans"})],
            default_mode: "build".into(),
            commands: vec![json!({"name": "review"})],
        }
    }

    #[test]
    fn config_options_shape() {
        let c = catalog();
        let opts = config_options(&c, &Selection { model: Some(json!({"providerID": "b", "id": "m2", "variant": "x-high"})), mode: None });
        assert_eq!(opts[0]["currentValue"], "b/m2");
        assert_eq!(opts[0]["options"][0]["value"], "a/m1");
        assert_eq!(opts[1]["id"], "effort");
        assert_eq!(opts[1]["currentValue"], "x-high");
        assert_eq!(opts[1]["options"][1], json!({"value": "x-high", "name": "X High"}));
        assert_eq!(opts[1]["options"][2]["value"], "default");
        assert_eq!(opts[2]["options"][0], json!({"value": "build", "name": "Build"}));
        assert_eq!(opts[2]["options"][1]["description"], "Plans");
        assert_eq!(available_commands(&c)[1], json!({"name": "compact", "description": "Compact the session"}));
    }

    #[test]
    fn changes() {
        let c = catalog();
        let sel = Selection::default();
        let Change::Model(m) = resolve_change(&c, &sel, "model", "b/m2/high").ok().unwrap() else { panic!() };
        assert_eq!(m, json!({"providerID": "b", "id": "m2", "variant": "high"}));
        assert_eq!(resolve_change(&c, &sel, "model", "z/q").err().unwrap().message, "Invalid params: model not found: z/q");
        assert_eq!(resolve_change(&c, &sel, "mode", "nope").err().unwrap().data, json!({"mode": "nope"}));
        assert_eq!(resolve_change(&c, &sel, "other", "x").err().unwrap().message, "Invalid params: unknown config option: other");
    }

    #[test]
    fn prompt_conversion() {
        let c = catalog();
        let blocks = vec![
            json!({"type": "text", "text": "hi"}),
            json!({"type": "text", "text": "ctx", "annotations": {"audience": ["assistant"]}}),
            json!({"type": "resource_link", "uri": "https://x/remote.png", "name": "remote.png"}),
            json!({"type": "resource", "resource": {"uri": "file:///tmp/a.rs#L3", "text": "code"}}),
        ];
        let p = prepare(&c, &blocks).ok().unwrap();
        assert_eq!(p.text, "hi\n[remote.png](https://x/remote.png)\n[/tmp/a.rs:3]\ncode");
        assert_eq!(p.synthetic, vec!["ctx"]);
        let p = prepare(&c, &[json!({"type": "text", "text": "/review  a b "})]).ok().unwrap();
        assert_eq!(p.slash, Some(("review".into(), "a b".into())));
        assert!(p.command.is_some());
        assert!(prepare(&c, &[json!({"type": "image"})]).err().unwrap().message.contains("image content has no data or uri"));
    }

    #[test]
    fn tool_shapes() {
        let call = pending_tool_call("t1", "bash", &json!({"command": "printf hello"}), None, "/workspace");
        assert_eq!(call["title"], "printf hello");
        assert_eq!(call["kind"], "execute");
        assert_eq!(call["locations"], json!([{"path": "/workspace"}]));
        assert_eq!(call["rawInput"], json!({"command": "printf hello", "cwd": "/workspace"}));
        let err = error_update("t", "read", &json!({"path": "a.txt"}), &json!([]), None, "boom", "/w");
        assert_eq!(err["content"], json!([{"type": "content", "content": {"type": "text", "text": "boom"}}]));
        assert_eq!(err["locations"], json!([{"path": "/w/a.txt"}]));
    }

    const PATCH: &str = "*** Begin Patch\n*** Update File: b.py\n@@\n def f():\n-    x = 1\n+    x = 2\n     return x\n*** Update File: e.py\n@@\n     a = 1\n-    b = 2\n+    b = 3\n     c = 3\n*** Add File: c.txt\n+hello\n*** Delete File: gone.txt\n*** End Patch";

    fn file(file: &str, patch: &str, status: &str) -> Value {
        json!({"file": file, "patch": patch, "status": status, "additions": 1, "deletions": 1})
    }

    /// Recorded from upstream 2.1.4 `opencode acp` completing a patch call (e.py's patch is trimmed by core, so
    /// its old text is rebuilt from the patch hunks).
    #[test]
    fn completed_patch_diffs_match_upstream() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().display().to_string();
        std::fs::write(dir.path().join("b.py"), "def f():\n    x = 2\n    return x\n").unwrap();
        std::fs::write(dir.path().join("e.py"), "    a = 1\n    b = 3\n    c = 3\n").unwrap();
        std::fs::write(dir.path().join("c.txt"), "hello\n").unwrap();
        let metadata = json!({"files": [
            file("b.py", "Index: b.py\n===================================================================\n--- b.py\n+++ b.py\n@@ -1,3 +1,3 @@\n def f():\n-    x = 1\n+    x = 2\n     return x\n", "modified"),
            file("e.py", "Index: e.py\n===================================================================\n--- e.py\n+++ e.py\n@@ -1,3 +1,3 @@\n a = 1\n-b = 2\n+b = 3\n c = 3\n", "modified"),
            file("c.txt", "Index: c.txt\n===================================================================\n--- c.txt\n+++ c.txt\n@@ -0,0 +1,1 @@\n+hello\n", "added"),
            file("gone.txt", "Index: gone.txt\n===================================================================\n--- gone.txt\n+++ gone.txt\n@@ -1,1 +0,0 @@\n-bye\n", "deleted"),
        ], "truncated": false});
        let input = json!({"patchText": PATCH});
        let update = with_update("tool_call_update", completed_update("call_patch", "patch", &input, &json!([{"type": "text", "text": "Success."}]), Some(&metadata), &cwd));
        let update = with_completed_diffs(update, "patch", &input, Some(&metadata), &cwd);
        let p = |f: &str| format!("{cwd}/{f}");
        assert_eq!(
            update["content"],
            json!([
                {"type": "content", "content": {"type": "text", "text": "Success."}},
                {"type": "diff", "path": p("b.py"), "oldText": "def f():\n    x = 1\n    return x\n", "newText": "def f():\n    x = 2\n    return x\n"},
                {"type": "diff", "path": p("e.py"), "oldText": "    a = 1\n    b = 2\n    c = 3\n", "newText": "    a = 1\n    b = 3\n    c = 3\n"},
                {"type": "diff", "path": p("c.txt"), "oldText": null, "newText": "hello\n"},
                {"type": "diff", "path": p("gone.txt"), "oldText": "bye\n", "newText": ""},
            ])
        );
        let keys: Vec<&String> = update.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["sessionUpdate", "toolCallId", "status", "locations", "content", "rawOutput"]);
        // The same trimmed patch on an edit call has no hunks to rebuild from, so that file gets no diff.
        let edit = with_completed_diffs(with_update("tool_call_update", json!({"content": []})), "edit", &json!({}), Some(&metadata), &cwd);
        assert_eq!(edit["content"].as_array().unwrap().len(), 3);
        // One file that doesn't decode drops them all; other updates pass through.
        let bad = json!({"files": [metadata["files"][0].clone(), {"file": "x", "patch": "", "status": "renamed", "additions": 0, "deletions": 0}]});
        assert_eq!(with_completed_diffs(with_update("tool_call_update", json!({"content": []})), "edit", &json!({}), Some(&bad), &cwd)["content"], json!([]));
        assert_eq!(with_completed_diffs(json!({"sessionUpdate": "tool_call"}), "edit", &json!({}), Some(&metadata), &cwd), json!({"sessionUpdate": "tool_call"}));
    }

    /// Recorded from upstream 2.1.4's permission requests for the same patch and for an edit.
    #[test]
    fn permission_previews_match_upstream() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().display().to_string();
        std::fs::write(dir.path().join("b.py"), "def f():\n    x = 1\n    return x\n").unwrap();
        std::fs::write(dir.path().join("e.py"), "    a = 1\n    b = 2\n    c = 3\n").unwrap();
        std::fs::write(dir.path().join("gone.txt"), "bye\n").unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let p = |f: &str| format!("{cwd}/{f}");
        let tool = Tracked { session: "s".into(), id: "call_patch".into(), name: "patch".into(), input: json!({"patchText": PATCH}), metadata: json!({}) };
        let params = permission_params("root", &cwd, &json!({"id": "per", "action": "edit", "resources": []}), Some(&tool), None);
        let call = &params["toolCall"];
        assert_eq!(call["title"], "4 files");
        assert_eq!(
            call["content"],
            json!([
                {"type": "diff", "path": p("b.py"), "oldText": "def f():\n    x = 1\n    return x\n", "newText": "def f():\n    x = 2\n    return x\n"},
                {"type": "diff", "path": p("e.py"), "oldText": "    a = 1\n    b = 2\n    c = 3\n", "newText": "    a = 1\n    b = 3\n    c = 3\n"},
                {"type": "diff", "path": p("c.txt"), "oldText": null, "newText": "hello\n"},
                {"type": "diff", "path": p("gone.txt"), "oldText": "bye\n", "newText": ""},
            ])
        );
        let keys: Vec<&String> = call.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["toolCallId", "name", "title", "kind", "status", "locations", "rawInput", "content"]);

        let edit = Tracked { session: "s".into(), id: "call_edit".into(), name: "edit".into(), input: json!({"path": "a.txt", "oldString": "two", "newString": "TWO"}), metadata: json!({}) };
        let data = json!({"id": "per", "action": "edit", "resources": [], "metadata": {"files": [file("a.txt", "Index: a.txt\n===================================================================\n--- a.txt\n+++ a.txt\n@@ -1,3 +1,3 @@\n one\n-two\n+TWO\n three\n", "modified")]}});
        let call = permission_params("root", &cwd, &data, Some(&edit), None)["toolCall"].clone();
        assert_eq!(call["title"], "a.txt");
        assert_eq!(call["content"], json!([{"type": "diff", "path": p("a.txt"), "oldText": "one\ntwo\nthree\n", "newText": "one\nTWO\nthree\n"}]));
        // A file that no longer matches its patch is left out; a missing file drops every preview.
        std::fs::write(dir.path().join("a.txt"), "changed\n").unwrap();
        assert!(permission_params("root", &cwd, &data, Some(&edit), None)["toolCall"].get("content").is_none());
        std::fs::remove_file(dir.path().join("a.txt")).unwrap();
        assert!(permission_params("root", &cwd, &data, Some(&edit), None)["toolCall"].get("content").is_none());
    }

    #[test]
    fn fold_text_and_terminal() {
        let ctx = Ctx { session: "ses_1".into(), cwd: "/w".into(), start: "msg_1".into(), caps: Caps::default() };
        let mut st = TurnState::default();
        let ev = |ty: &str, data: Value| json!({"id": "evt_1", "type": ty, "data": data});
        assert!(fold(&mut st, &ev("session.text.delta", json!({"sessionID": "ses_1", "assistantMessageID": "msg_2", "delta": "x"})), &ctx).0.is_empty());
        fold(&mut st, &ev("session.inbox.delivered", json!({"sessionID": "ses_1", "inboxID": "msg_1"})), &ctx);
        let (out, _) = fold(&mut st, &ev("session.text.delta", json!({"sessionID": "ses_1", "assistantMessageID": "msg_2", "delta": "x"})), &ctx);
        assert!(matches!(&out[0], Out::Update(u) if u["sessionUpdate"] == "agent_message_chunk"));
        fold(&mut st, &ev("session.step.ended", json!({"sessionID": "ses_1", "finish": "stop", "tokens": {"input": 100, "output": 40, "reasoning": 7, "cache": {"read": 11, "write": 13}}})), &ctx);
        assert_eq!(st.usage_turn.unwrap().iter().sum::<i64>(), 171);
        let (_, t) = fold(&mut st, &ev("session.execution.succeeded", json!({"sessionID": "ses_1"})), &ctx);
        assert_eq!(t, Some("succeeded"));
    }

    #[test]
    fn elicitation_schema_and_answer() {
        let form = json!({"id": "f", "sessionID": "ses_1", "title": "Q", "metadata": {"kind": "question"}, "fields": [
            {"key": "q0", "type": "string", "options": [{"value": "Bun", "label": "Bun"}]},
            {"key": "q1", "type": "multiselect", "custom": true, "options": [{"value": "Fast", "label": "Fast"}]},
        ]});
        let caps = Caps { form_elicitation: true, ..Default::default() };
        let schema = form_schema(&form, caps).unwrap();
        assert!(schema["properties"]["q1_custom"].is_object());
        assert!(form_schema(&form, Caps::default()).is_none());
        let a = form_answer(&form, &json!({"q0": "Bun", "q1": ["Fast"], "q1_custom": "Small"})).unwrap();
        assert_eq!(a, json!({"q0": "Bun", "q1": ["Fast", "Small"]}));
    }
}
