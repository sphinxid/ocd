//! The tool tree, discovery (`search`), call admission, hooks, and limits. Port of `tool-runtime.ts`.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value as Json};

use crate::data::{from_json, quote, to_boundary};
use crate::host::{CallResult, Host, ToolCallError, ToolInvocation};
use futures::future::LocalBoxFuture;

use crate::ast::Loc;
use crate::interp::errors::{locate, tool_runtime_error, DiagnosticKind, JsResult, Thrown};
use crate::interp::runner::Resolving;
use crate::interp::{Rt, Site};
use crate::tool_schema::{
    input_properties, is_empty_input, is_identifier_segment, output_typescript, schema_to_typescript, ToolSchema,
};
use crate::value::Value;

/// One schema-described tool exposed to programs as `tools.<path>`.
#[derive(Clone, Debug)]
pub struct ToolDef {
    /// Dotted path; dots create namespaces (`"github.issues.list"`).
    pub path: String,
    pub description: String,
    pub input: ToolSchema,
    /// `None` renders `Promise<void>` and the program receives `undefined`.
    pub output: Option<ToolSchema>,
}

impl ToolDef {
    pub fn new(path: impl Into<String>, description: impl Into<String>, input: Json) -> ToolDef {
        ToolDef { path: path.into(), description: description.into(), input: ToolSchema::Json(input), output: None }
    }
    pub fn with_output(mut self, output: Json) -> ToolDef {
        self.output = Some(ToolSchema::Json(output));
        self
    }
}

/// An entry of the tool tree, in declaration order.
#[derive(Clone, Debug)]
pub enum ToolEntry {
    Tool(ToolDef),
    /// Namespace metadata (descriptions participate in search for every descendant tool).
    Namespace { path: String, description: Option<String> },
}

/// The structured description of one visible tool.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDescription {
    pub path: String,
    pub description: String,
    pub signature: String,
}

#[derive(Default)]
struct Node {
    tool: Option<Arc<ToolDef>>,
    namespace: Option<Option<String>>,
    children: Vec<(String, Node)>,
}

impl Node {
    fn child(&mut self, name: &str) -> &mut Node {
        if let Some(i) = self.children.iter().position(|(n, _)| n == name) {
            return &mut self.children[i].1;
        }
        self.children.push((name.to_string(), Node::default()));
        &mut self.children.last_mut().unwrap().1
    }
    fn get(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|(n, _)| n == name).map(|(_, n)| n)
    }
}

pub struct SearchEntry {
    pub description: ToolDescription,
    /// The path split into words, so `zones` matches `get_zones` as a word rather than inside `timezones`.
    pub path_words: Vec<String>,
    pub search_text: String,
}

/// Tools indexed once per runtime, with discovery materialized on demand.
pub struct Prepared {
    root: Node,
    index: OnceLock<Vec<SearchEntry>>,
}

/// UTF-16 code-unit comparison (`compareText`).
pub fn compare_text(a: &str, b: &str) -> std::cmp::Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

pub fn tool_expression(path: &str) -> String {
    let mut out = String::from("tools");
    for segment in path.split('.') {
        if is_identifier_segment(segment) {
            out.push('.');
            out.push_str(segment);
        } else {
            out.push('[');
            out.push_str(&quote(segment));
            out.push(']');
        }
    }
    out
}

/// Exact callable signature of the built-in `search` function, for host-owned instructions.
pub const SEARCH_SIGNATURE: &str = "search({\n  query?: string,\n  namespace?: string,\n  /** @integer @exclusiveMinimum 0 */\n  limit?: number,\n  /** @integer @minimum 0 */\n  offset?: number,\n}): {\n  items: Array<{\n      path: string,\n      description: string,\n      signature: string,\n    }>,\n  /** @integer @minimum 0 */\n  remaining: number,\n  next: {\n      /** @integer @minimum 0 */\n      offset: number,\n    } | null,\n}";

pub fn signature_of(def: &ToolDef) -> String {
    let input = if is_empty_input(&def.input) {
        "()".to_string()
    } else {
        format!("({})", schema_to_typescript(&def.input, true))
    };
    format!("{}{}: Promise<{}>", tool_expression(&def.path), input, output_typescript(def.output.as_ref(), true))
}

fn tokenize(query: &str) -> Vec<String> {
    let chars: Vec<char> = query.chars().collect();
    let mut spaced = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if i + 1 < chars.len() && (c.is_ascii_lowercase() || c.is_ascii_digit()) && chars[i + 1].is_ascii_uppercase() {
            spaced.push(c);
            spaced.push(' ');
            spaced.push(chars[i + 1]);
            i += 2;
        } else {
            spaced.push(c);
            i += 1;
        }
    }
    spaced
        .to_lowercase()
        .split(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit()))
        .filter(|t| !t.is_empty() && *t != "*")
        .map(String::from)
        .collect()
}

fn term_forms(term: &str) -> Vec<String> {
    let mut forms = vec![term.to_string()];
    let len = term.encode_utf16().count();
    if term.ends_with("es") && len > 3 {
        forms.push(term[..term.len() - 2].to_string());
    }
    if term.ends_with('s') && len > 2 {
        forms.push(term[..term.len() - 1].to_string());
    }
    forms
}

fn rank<'a>(entries: &[&'a SearchEntry], query: &str) -> Vec<&'a SearchEntry> {
    let terms: Vec<Vec<String>> = tokenize(query).iter().map(|t| term_forms(t)).collect();
    let mut scored: Vec<(&SearchEntry, i64)> = entries
        .iter()
        .map(|entry| {
            let path = entry.description.path.to_lowercase();
            let description = entry.description.description.to_lowercase();
            let score = terms.iter().fold(0i64, |total, forms| {
                let any = |f: &dyn Fn(&str) -> bool| forms.iter().any(|form| f(form));
                total
                    + if any(&|form| path == form || path.ends_with(&format!(".{form}"))) { 20 } else { 0 }
                    + if any(&|form| entry.path_words.iter().any(|w| w == form)) { 12 } else { 0 }
                    + if any(&|form| path.contains(form)) { 8 } else { 0 }
                    + if any(&|form| description.contains(form)) { 4 } else { 0 }
                    + if any(&|form| entry.search_text.contains(form)) { 2 } else { 0 }
            });
            (*entry, score)
        })
        .filter(|(_, score)| terms.is_empty() || *score > 0)
        .collect();
    scored.sort_by(|(l, ls), (r, rs)| rs.cmp(ls).then_with(|| compare_text(&l.description.path, &r.description.path)));
    scored.into_iter().map(|(e, _)| e).collect()
}

fn flatten(node: &Node, path: &mut Vec<String>, namespaces: &[Option<String>], out: &mut Vec<(String, Arc<ToolDef>, Vec<Option<String>>)>) {
    let mut next = namespaces.to_vec();
    if let Some(ns) = &node.namespace {
        next.push(ns.clone());
    }
    if let Some(tool) = &node.tool {
        out.push((path.join("."), tool.clone(), next.clone()));
    }
    for (name, child) in &node.children {
        path.push(name.clone());
        flatten(child, path, &next, out);
        path.pop();
    }
}

impl Prepared {
    /// Builds the tool trie. Dots in paths are namespace separators; the last tool for a canonical path wins.
    pub fn new(entries: Vec<ToolEntry>) -> Result<Prepared, String> {
        let mut root = Node::default();
        for entry in entries {
            let path = match &entry {
                ToolEntry::Tool(t) => t.path.clone(),
                ToolEntry::Namespace { path, .. } => path.clone(),
            };
            let mut current = &mut root;
            for segment in path.split('.') {
                if segment.is_empty() {
                    return Err(format!("Tool name '{path}' contains an empty segment."));
                }
                current = current.child(segment);
            }
            match entry {
                ToolEntry::Tool(t) => current.tool = Some(Arc::new(t)),
                ToolEntry::Namespace { description, .. } => current.namespace = Some(description),
            }
        }
        Ok(Prepared { root, index: OnceLock::new() })
    }

    pub fn search_index(&self) -> &[SearchEntry] {
        self.index.get_or_init(|| {
            let mut tools = Vec::new();
            flatten(&self.root, &mut Vec::new(), &[], &mut tools);
            tools.sort_by(|a, b| compare_text(&a.0, &b.0));
            tools
                .into_iter()
                .map(|(path, tool, namespaces)| {
                    let mut text = vec![path.clone(), tool.description.clone()];
                    text.extend(namespaces.into_iter().flatten());
                    for p in input_properties(&tool.input) {
                        text.push(p.name);
                        if let Some(d) = p.description {
                            text.push(d);
                        }
                    }
                    SearchEntry {
                        path_words: tokenize(&path),
                        search_text: text.join("\n").to_lowercase(),
                        description: ToolDescription {
                            signature: signature_of(&ToolDef { path: path.clone(), ..(*tool).clone() }),
                            path,
                            description: tool.description.clone(),
                        },
                    }
                })
                .collect()
        })
    }

    /// Structured descriptors (path, description, generated signature) for every visible tool, sorted by path.
    pub fn catalog(&self) -> Vec<ToolDescription> {
        self.search_index().iter().map(|e| e.description.clone()).collect()
    }

    fn lookup(&self, segments: &[String]) -> Option<&Node> {
        let mut node = &self.root;
        for s in segments {
            node = node.get(s)?;
        }
        Some(node)
    }

    fn search(&self, request: &Map<String, Json>) -> Json {
        let index = self.search_index();
        let query = request.get("query").and_then(|q| q.as_str()).unwrap_or("").to_string();
        let offset = request.get("offset").and_then(|o| o.as_f64()).unwrap_or(0.0) as usize;
        let limit = request.get("limit").and_then(|o| o.as_f64()).unwrap_or(10.0) as usize;
        let mut ns = request.get("namespace").and_then(|n| n.as_str()).map(String::from);
        if let Some(n) = &ns
            && !index.iter().any(|e| e.description.path.starts_with("tools."))
        {
            if n == "tools" {
                ns = None;
            } else if let Some(rest) = n.strip_prefix("tools.") {
                ns = Some(rest.to_string());
            }
        }
        let scoped: Vec<&SearchEntry> = match &ns {
            None => index.iter().collect(),
            Some(ns) => index
                .iter()
                .filter(|e| e.description.path == *ns || e.description.path.starts_with(&format!("{ns}.")))
                .collect(),
        };
        let trimmed = query.trim_matches(|c: char| (c as u32) < 0x10000 && crate::number::is_js_whitespace(c as u16));
        let path_query = trimmed.strip_prefix("tools.").unwrap_or(trimmed);
        let exact = if path_query.is_empty() {
            None
        } else {
            scoped
                .iter()
                .find(|e| e.description.path == path_query || tool_expression(&e.description.path) == trimmed)
                .copied()
        };
        let ranked = match exact {
            Some(e) => vec![e],
            None => rank(&scoped, &query),
        };
        let items: Vec<Json> = ranked
            .iter()
            .skip(offset)
            .take(limit)
            .map(|e| {
                json!({
                    "path": tool_expression(&e.description.path),
                    "description": e.description.description,
                    "signature": e.description.signature,
                })
            })
            .collect();
        let remaining = ranked.len().saturating_sub(offset + items.len());
        let next = if remaining > 0 { json!({ "offset": offset + items.len() }) } else { Json::Null };
        json!({ "items": items, "remaining": remaining, "next": next })
    }

    fn namespace_keys(&self, path: &[std::rc::Rc<str>]) -> JsResult<Vec<String>> {
        let segments = canonical_segments(path);
        match self.lookup(&segments) {
            Some(node) => Ok(node.children.iter().map(|(n, _)| n.clone()).collect()),
            None => Err(tool_runtime_error(
                DiagnosticKind::UnknownTool,
                format!("Unknown tool namespace '{}'.", segments.join(".")),
                Vec::new(),
            )),
        }
    }

    fn resolve(&self, segments: &[String]) -> JsResult<Arc<ToolDef>> {
        let Some(node) = self.lookup(segments) else {
            let name = segments.join(".");
            let ns = if segments.len() > 1 && self.root.get(&segments[0]).is_some() { Some(&segments[0]) } else { None };
            let index = self.search_index();
            let candidates: Vec<&SearchEntry> = match ns {
                Some(ns) => index.iter().filter(|e| e.description.path.starts_with(&format!("{ns}."))).collect(),
                None => index.iter().collect(),
            };
            let query = match ns {
                Some(_) => segments[1..].join(" "),
                None => name.clone(),
            };
            let closest = rank(&candidates, &query).first().map(|e| e.description.path.clone());
            let message = match closest {
                Some(c) => format!("Unknown tool '{name}'. Did you mean {}?", tool_expression(&c)),
                None => format!("Unknown tool '{name}'."),
            };
            return Err(tool_runtime_error(
                DiagnosticKind::UnknownTool,
                message,
                vec!["Use search to find available tools.".into()],
            ));
        };
        match &node.tool {
            Some(t) => Ok(t.clone()),
            None => Err(tool_runtime_error(
                DiagnosticKind::UnknownTool,
                format!("Tool '{}' is not callable.", segments.join(".")),
                Vec::new(),
            )),
        }
    }
}

fn canonical_segments(path: &[std::rc::Rc<str>]) -> Vec<String> {
    path.iter().flat_map(|s| s.split('.').map(String::from).collect::<Vec<_>>()).collect()
}

/// The tool bridge of one execution.
pub struct ToolRuntime {
    pub prepared: Arc<Prepared>,
    pub host: Arc<dyn Host>,
    pub max_tool_calls: Option<usize>,
    pub calls: RefCell<Vec<String>>,
    next_id: Cell<u64>,
}

impl ToolRuntime {
    pub fn new(prepared: Arc<Prepared>, host: Arc<dyn Host>, max_tool_calls: Option<usize>) -> ToolRuntime {
        ToolRuntime { prepared, host, max_tool_calls, calls: RefCell::new(Vec::new()), next_id: Cell::new(0) }
    }

    pub fn keys(&self, path: &[std::rc::Rc<str>]) -> JsResult<Vec<String>> {
        self.prepared.namespace_keys(path)
    }

    /// A call id unique across all executions of the process (hooks correlate `before`/`after` by it).
    pub fn next_call_id(&self) -> u64 {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        self.next_id.set(self.next_id.get() + 1);
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    fn record_call(&self, name: &str) -> JsResult<()> {
        if let Some(max) = self.max_tool_calls
            && self.calls.borrow().len() >= max
        {
            return Err(tool_runtime_error(
                DiagnosticKind::ToolCallLimitExceeded,
                format!("Execution exceeded its tool-call limit of {max}."),
                Vec::new(),
            ));
        }
        self.calls.borrow_mut().push(name.to_string());
        Ok(())
    }
}

pub enum ToolTarget {
    Search,
    Path(Vec<String>),
}

fn schema_error(message: &str, path: Option<&str>) -> String {
    match path {
        Some(p) => format!("SchemaError({message}\n  at [{}])", quote(p)),
        None => format!("SchemaError({message})"),
    }
}

/// Decodes `search` input like its Effect Schema.
fn decode_search_input(input: Option<&Json>) -> Result<Map<String, Json>, String> {
    let Some(Json::Object(map)) = input else { return Err(schema_error("Expected object", None)) };
    for key in ["query", "namespace"] {
        if let Some(v) = map.get(key)
            && !v.is_string()
        {
            return Err(schema_error("Expected string", Some(key)));
        }
    }
    for (key, minimum, exclusive) in [("limit", 0.0, true), ("offset", 0.0, false)] {
        let Some(v) = map.get(key) else { continue };
        let Some(n) = v.as_f64() else { return Err(schema_error("Expected number", Some(key))) };
        if n.fract() != 0.0 || n.abs() > 9007199254740991.0 {
            return Err(schema_error("Expected an integer", Some(key)));
        }
        if exclusive && n <= minimum {
            return Err(schema_error("Expected a value greater than 0", Some(key)));
        }
        if !exclusive && n < minimum {
            return Err(schema_error("Expected a value greater than or equal to 0", Some(key)));
        }
    }
    Ok(map.clone())
}

/// Reports an interrupted call to `tool_after` when the call's future is dropped before it settles.
struct AfterGuard {
    host: Arc<dyn Host>,
    call: Option<ToolInvocation>,
}

impl Drop for AfterGuard {
    fn drop(&mut self) {
        if let Some(call) = self.call.take() {
            self.host.tool_after(&call, &CallResult::Interrupted);
        }
    }
}

/// One host tool call (`ToolRuntime.execute`/`search`): resolution, input decoding, admission, hooks, the call, and
/// output handling. Its synchronous part (everything up to the first host suspension) runs when it is first polled.
fn tool_work(rt: &Rt, target: ToolTarget, json_args: Vec<Option<Json>>) -> LocalBoxFuture<'static, Result<Option<Json>, Thrown>> {
    let rt = rt.clone();
    Box::pin(async move {
        let tools = &rt.tools;
        let (name, def) = match target {
            ToolTarget::Search => ("search".to_string(), None),
            ToolTarget::Path(path) => {
                let rc: Vec<std::rc::Rc<str>> = path.iter().map(|s| std::rc::Rc::from(s.as_str())).collect();
                let segments = canonical_segments(&rc);
                // Models often write `tools.search(...)` for the bare `search(...)`; honor it unless a tool owns it.
                if segments.len() == 1 && segments[0] == "search" && tools.prepared.lookup(&segments).is_none() {
                    ("search".to_string(), None)
                } else {
                    let def = tools.prepared.resolve(&segments)?;
                    (segments.join("."), Some(def))
                }
            }
        };
        let normalized: Vec<Option<Json>> = if json_args.is_empty() { vec![Some(json!({}))] } else { json_args };
        if normalized.len() != 1 {
            return Err(tool_runtime_error(
                DiagnosticKind::InvalidToolInput,
                format!("Tool '{name}' expects at most one input object."),
                Vec::new(),
            ));
        }
        let raw_input = normalized.into_iter().next().unwrap();
        let invalid = |message: String| {
            tool_runtime_error(
                DiagnosticKind::InvalidToolInput,
                format!("Invalid input for tool '{name}': {message}"),
                if def.is_none() {
                    Vec::new()
                } else {
                    vec!["The signature may have changed. Use search to get the current signature.".into()]
                },
            )
        };
        let (input, search_request) = match &def {
            None => {
                let request = decode_search_input(raw_input.as_ref()).map_err(invalid)?;
                (Some(Json::Object(request.clone())), Some(request))
            }
            Some(_) => (tools.host.decode_input(&name, raw_input).map_err(invalid)?, None),
        };
        tools.record_call(&name)?;
        let invocation = ToolInvocation { id: tools.next_call_id(), name: name.clone(), input: input.clone() };
        let host = tools.host.clone();
        host.tool_before(&invocation).await.map_err(tool_error)?;
        let mut guard = AfterGuard { host: host.clone(), call: Some(invocation.clone()) };
        let outcome: Result<Option<Json>, Thrown> = match search_request {
            Some(request) => Ok(Some(tools.prepared.search(&request))),
            None => match host.call(&name, input).await {
                Ok(value) => Ok(if def.as_ref().unwrap().output.is_none() { None } else { value }),
                Err(ToolCallError::Failure(m)) => Err(Thrown::ToolFailure(m.into())),
                Err(ToolCallError::InvalidOutput(m)) => Err(tool_runtime_error(
                    DiagnosticKind::InvalidToolOutput,
                    format!("Invalid output from tool '{name}': {m}"),
                    Vec::new(),
                )),
            },
        };
        let call = guard.call.take().unwrap();
        let result = match &outcome {
            Ok(v) => CallResult::Success(v.clone()),
            Err(Thrown::ToolFailure(m)) => CallResult::Failure(m.to_string()),
            Err(Thrown::ToolRuntime(e)) => CallResult::Failure(e.message.clone()),
            Err(_) => CallResult::Failure(String::new()),
        };
        host.tool_after(&call, &result);
        outcome
    })
}

/// Starts a tool call as host work that runs concurrently with the program; `made` settles in a later job, after
/// the jobs already queued, like I/O. A defect is located at `site`, the call that started the work.
pub fn start_tool(rt: &Rt, target: ToolTarget, json_args: Vec<Option<Json>>, made: Resolving, site: Option<Loc>) {
    let work = tool_work(rt, target, json_args);
    rt.runner.host(Box::pin(async move {
        let outcome = work.await;
        Box::new(move || {
            let rt = crate::interp::rt();
            match outcome {
                Ok(value) => {
                    let value = from_json(&rt, value.as_ref());
                    made.resolve(&rt, value)
                }
                Err(error) => made.reject(&rt, locate(error, site)),
            }
        }) as Box<dyn FnOnce()>
    }));
}

/// The built-in `search`: arguments cross as JSON, then the call runs to completion before any other job, and the
/// result comes back as values that can also be awaited or chained like a Promise.
pub async fn run_search(rt: &Rt, args: Vec<Value>, site: Site) -> JsResult<Value> {
    let mut json_args: Vec<Option<Json>> = Vec::with_capacity(args.len());
    for a in args {
        json_args.push(to_boundary(rt, a, site).await?);
    }
    let value = rt.runner.block_on(rt, tool_work(rt, ToolTarget::Search, json_args))??;
    Ok(crate::interp::promises::promise_like(rt, from_json(rt, value.as_ref())))
}

fn tool_error(e: ToolCallError) -> Thrown {
    match e {
        ToolCallError::Failure(m) | ToolCallError::InvalidOutput(m) => Thrown::ToolFailure(m.into()),
    }
}

#[allow(dead_code)]
fn _unused(_: HashMap<u8, u8>) {}
