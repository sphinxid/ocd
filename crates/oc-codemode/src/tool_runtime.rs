//! The tool tree, discovery (`search`), call admission, hooks, and limits. Port of `tool-runtime.ts`.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value as Json};

use crate::data::{from_json, quote};
use crate::host::HostValue;
use crate::interp::extensions::Crossed;
use crate::host::{CallResult, Host, ToolCallError, ToolInvocation};
use futures::future::LocalBoxFuture;

use crate::ast::Loc;
use crate::interp::errors::{locate, tool_runtime_error, DiagnosticKind, JsResult, Thrown};
use crate::interp::runner::Resolving;
use crate::interp::{Rt, Site};
use crate::tool_schema::{
    input_properties, is_empty_input, is_identifier_segment, output_typescript, schema_to_typescript, string_parameter,
    StringParameter, ToolSchema,
};
use crate::value::Value;

/// One schema-described tool exposed to programs as `tools.<path>`.
#[derive(Clone, Debug)]
pub struct ToolDef {
    /// Dotted path; dots create namespaces (`"github.issues.list"`).
    pub path: String,
    pub description: String,
    pub input: ToolSchema,
    /// The tool's input is one required string field that programs pass directly, as in `tools.patch("...")`.
    /// The object form still works, and the executor receives the object either way.
    pub freeform: bool,
    /// `None` renders `Promise<void>` and the program receives `undefined`.
    pub output: Option<ToolSchema>,
}

impl ToolDef {
    pub fn new(path: impl Into<String>, description: impl Into<String>, input: Json) -> ToolDef {
        ToolDef { path: path.into(), description: description.into(), input: ToolSchema::Json(input), freeform: false, output: None }
    }
    pub fn with_freeform(mut self, freeform: bool) -> ToolDef {
        self.freeform = freeform;
        self
    }
    /// `stringParameter(tool)`: the string field a freeform tool takes directly.
    pub fn string_parameter(&self) -> Option<StringParameter> {
        if self.freeform { string_parameter(&self.input) } else { None }
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
    /// Position in the full search index, for scoring through the word index.
    pub position: usize,
    /// The tool's searchable texts by field, in [`FIELD_WEIGHTS`] order.
    texts: [Vec<String>; 5],
    /// Words in the tool's own name (the last path segment).
    name_length: usize,
}

/// Tools indexed once per runtime, with discovery materialized on demand.
pub struct Prepared {
    root: Node,
    index: OnceLock<Vec<SearchEntry>>,
    words: OnceLock<WordIndex>,
}

/// UTF-16 code-unit comparison (`compareText`).
pub fn compare_text(a: &str, b: &str) -> std::cmp::Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

/// How a program reads `key` from `base` (upstream `member`): `base.key`, or `base["key"]` for a key that isn't an
/// ASCII identifier.
pub fn member(base: &str, key: &str) -> String {
    let mut chars = key.chars();
    let identifier = matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
    if identifier { format!("{base}.{key}") } else { format!("{base}[{}]", quote(key)) }
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

/// Exact callable signature of the built-in `search` function, for host-owned instructions (a query string or the
/// options object since 2.3.1, #64347).
pub const SEARCH_SIGNATURE: &str = "search(query: string | {\n  query?: string,\n  namespace?: string,\n  /** @integer @exclusiveMinimum 0 */\n  limit?: number,\n  /** @integer @minimum 0 */\n  offset?: number,\n}): {\n  items: Array<{\n      path: string,\n      description: string,\n      signature: string,\n    }>,\n  /** @integer @minimum 0 */\n  remaining: number,\n  next: {\n      /** @integer @minimum 0 */\n      offset: number,\n    } | null,\n}";

pub fn signature_of(def: &ToolDef) -> String {
    let input = if let Some(parameter) = def.string_parameter() {
        parameter.signature
    } else if is_empty_input(&def.input) {
        "()".to_string()
    } else {
        format!("({})", schema_to_typescript(&def.input, true))
    };
    format!("{}{}: Promise<{}>", tool_expression(&def.path), input, output_typescript(def.output.as_ref(), true))
}

// Standard field-weighted BM25 (k1 1.2, b 0.75) plus a preference for short names: name, namespace, description,
// namespace description, parameter names.
const FIELD_WEIGHTS: [f64; 5] = [3.0, 2.0, 1.0, 0.4, 0.3];
const K1: f64 = 1.2;
const LENGTH_NORMALIZATION: f64 = 0.75;
/// score × (1 + BREVITY_BOOST / words in the tool name): "users" prefers `list_users` over `list_user_groups`.
const BREVITY_BOOST: f64 = 0.5;
/// A query word of 4+ letters also matches longer words it starts with, at this weight: `term` → `terminal`.
const PREFIX_WEIGHT: f64 = 0.3;

fn lower_or_digit(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit()
}

/// Words split at case changes and non-alphanumerics: `listUserGroups` → list, user, groups.
fn split_words(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut spaced = String::with_capacity(text.len() + 8);
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if i + 1 < chars.len() && lower_or_digit(c) && chars[i + 1].is_ascii_uppercase() {
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
        .split(|c: char| !lower_or_digit(c))
        .filter(|t| !t.is_empty())
        .map(String::from)
        .collect()
}

/// Plural endings only, applied the same everywhere: `settings` → `setting`, `statuses` → `status`, `vms` → `vm`.
fn stem(word: String) -> String {
    let len = word.len();
    if len > 4 && word.ends_with("ies") {
        return format!("{}y", &word[..len - 3]);
    }
    if len > 4 && ["sses", "shes", "ches", "xes", "zes", "uses"].iter().any(|e| word.ends_with(e)) {
        return word[..len - 2].to_string();
    }
    if len > 2 && word.ends_with('s') && !["ss", "us", "is"].iter().any(|e| word.ends_with(e)) {
        return word[..len - 1].to_string();
    }
    word
}

/// Searchable words of a text: its split words, plus each camelCase word whole (`CronJob` → cron, job, cronjob).
fn search_words(text: &str) -> Vec<String> {
    let mut words = split_words(text);
    // `/[A-Za-z0-9]*[a-z0-9][A-Z][A-Za-z0-9]*/g`: every alphanumeric run with a case change, whole.
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_alphanumeric() {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_alphanumeric() {
            i += 1;
        }
        let run = &bytes[start..i];
        if run.windows(2).any(|w| lower_or_digit(w[0] as char) && w[1].is_ascii_uppercase()) {
            words.push(String::from_utf8_lossy(run).to_ascii_lowercase());
        }
    }
    words.into_iter().map(stem).collect()
}

/// For each word, the tools containing it and their field-weighted, length-normalized term frequency.
struct WordIndex {
    postings: HashMap<String, (Vec<usize>, Vec<f32>)>,
    documents: usize,
    /// Sorted indexed words, for partial-word matching.
    words: Vec<String>,
}

fn build_word_index(index: &[SearchEntry]) -> WordIndex {
    let mut cache: HashMap<&str, Vec<String>> = HashMap::new();
    let fields: Vec<[Vec<String>; 5]> = index
        .iter()
        .map(|entry| {
            entry.texts.each_ref().map(|texts| {
                texts
                    .iter()
                    .flat_map(|text| cache.entry(text.as_str()).or_insert_with(|| search_words(text)).clone())
                    .collect()
            })
        })
        .collect();
    let average: [f64; 5] = std::array::from_fn(|field| {
        let total: usize = fields.iter().map(|entry| entry[field].len()).sum();
        let average = total as f64 / index.len() as f64;
        if average == 0.0 || average.is_nan() { 1.0 } else { average }
    });
    let mut postings: HashMap<String, (Vec<usize>, Vec<f32>)> = HashMap::new();
    for (position, entry) in fields.iter().enumerate() {
        let mut weights: indexmap::IndexMap<&str, f64> = indexmap::IndexMap::new();
        for field in 0..5 {
            let weight = FIELD_WEIGHTS[field]
                / (1.0 - LENGTH_NORMALIZATION + (LENGTH_NORMALIZATION * entry[field].len() as f64) / average[field]);
            for word in &entry[field] {
                *weights.entry(word.as_str()).or_insert(0.0) += weight;
            }
        }
        for (word, weight) in weights {
            let posting = postings.entry(word.to_string()).or_default();
            posting.0.push(position);
            posting.1.push(weight as f32);
        }
    }
    let mut words: Vec<String> = postings.keys().cloned().collect();
    words.sort_by(|a, b| compare_text(a, b));
    WordIndex { postings, documents: index.len(), words }
}

/// Indexed words longer than a 4+ letter query word that start with it.
fn prefix_matches<'a>(words: &'a WordIndex, term: &str) -> Vec<(&'a str, f64)> {
    if term.encode_utf16().count() < 4 {
        return Vec::new();
    }
    let low = words.words.partition_point(|w| compare_text(w, term).is_lt());
    words.words[low..]
        .iter()
        .take_while(|w| w.starts_with(term))
        .filter(|w| w.as_str() != term)
        .map(|w| (w.as_str(), PREFIX_WEIGHT))
        .collect()
}

impl Prepared {
    /// `rank(entries, query, index)`: the entries matching the query by BM25 score, best first (ties by path), or
    /// every entry by path for a query without words.
    fn rank<'a>(&'a self, entries: &[&'a SearchEntry], query: &str) -> Vec<&'a SearchEntry> {
        let terms = search_words(query);
        if terms.is_empty() {
            let mut sorted = entries.to_vec();
            sorted.sort_by(|l, r| compare_text(&l.description.path, &r.description.path));
            return sorted;
        }
        let index = self.search_index();
        let words = self.words.get_or_init(|| build_word_index(index));
        let allowed: Option<std::collections::HashSet<usize>> =
            (entries.len() != index.len()).then(|| entries.iter().map(|e| e.position).collect());
        let mut scores: indexmap::IndexMap<usize, f64> = indexmap::IndexMap::new();
        for term in &terms {
            // Each tool counts a query word once, through its best-scoring match.
            let mut best: indexmap::IndexMap<usize, f64> = indexmap::IndexMap::new();
            let candidates = std::iter::once((term.as_str(), 1.0)).chain(prefix_matches(words, term));
            for (word, factor) in candidates {
                let Some((positions, weights)) = words.postings.get(word) else { continue };
                let n = positions.len() as f64;
                let idf = (1.0 + (words.documents as f64 - n + 0.5) / (n + 0.5)).ln();
                for (i, &position) in positions.iter().enumerate() {
                    if let Some(allowed) = &allowed
                        && !allowed.contains(&position)
                    {
                        continue;
                    }
                    let weight = weights[i] as f64;
                    let score = (factor * idf * weight * (K1 + 1.0)) / (weight + K1);
                    if score > best.get(&position).copied().unwrap_or(0.0) {
                        best.insert(position, score);
                    }
                }
            }
            for (position, score) in best {
                *scores.entry(position).or_insert(0.0) += score;
            }
        }
        let mut ranked: Vec<(&SearchEntry, f64)> = scores
            .into_iter()
            .map(|(position, score)| {
                let entry = &index[position];
                (entry, score * (1.0 + BREVITY_BOOST / entry.name_length.max(1) as f64))
            })
            .collect();
        ranked.sort_by(|(l, ls), (r, rs)| {
            rs.partial_cmp(ls)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| compare_text(&l.description.path, &r.description.path))
        });
        ranked.into_iter().map(|(e, _)| e).collect()
    }
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
        Ok(Prepared { root, index: OnceLock::new(), words: OnceLock::new() })
    }

    pub fn search_index(&self) -> &[SearchEntry] {
        self.index.get_or_init(|| {
            let mut tools = Vec::new();
            flatten(&self.root, &mut Vec::new(), &[], &mut tools);
            tools.sort_by(|a, b| compare_text(&a.0, &b.0));
            tools
                .into_iter()
                .enumerate()
                .map(|(position, (path, tool, namespaces))| {
                    let segments: Vec<String> = path.split('.').map(String::from).collect();
                    let name = segments.last().cloned().unwrap_or_default();
                    let texts = [
                        vec![name.clone()],
                        segments[..segments.len() - 1].to_vec(),
                        vec![tool.description.clone()],
                        namespaces.into_iter().map(Option::unwrap_or_default).collect(),
                        // Parameter names only: their descriptions are half of all words and didn't improve ranking.
                        input_properties(&tool.input).into_iter().map(|p| p.name).collect(),
                    ];
                    SearchEntry {
                        position,
                        name_length: split_words(&name).len(),
                        texts,
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
            None => self.rank(&scoped, path_query),
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
            let closest = self.rank(&candidates, &query).first().map(|e| e.description.path.clone());
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
        if let Some(tool) = &node.tool {
            return Ok(tool.clone());
        }
        let name = segments.join(".");
        let prefix = format!("{name}.");
        let under: Vec<&SearchEntry> =
            self.search_index().iter().filter(|e| e.description.path.starts_with(&prefix)).collect();
        let ranked = self.rank(&under, segments.last().map(String::as_str).unwrap_or(""));
        let shown: Vec<String> = ranked
            .iter()
            .copied()
            .chain(under.iter().copied().filter(|e| !ranked.iter().any(|r| std::ptr::eq(*r, *e))))
            .take(3)
            .map(|e| tool_expression(&e.description.path))
            .collect();
        let more = under.len() - shown.len();
        let message = if shown.is_empty() {
            format!("Tool '{name}' is not callable.")
        } else {
            format!("Tool '{name}' is not callable, it is a namespace. Did you mean {}?", disjunction(&shown))
        };
        let suggestion = if more > 0 {
            format!("{more} more under '{name}'. Use search({{ namespace: {} }}) to list them.", quote(&name))
        } else {
            "Use search to find available tools.".to_string()
        };
        Err(tool_runtime_error(DiagnosticKind::UnknownTool, message, vec![suggestion]))
    }
}

/// `new Intl.ListFormat("en", { type: "disjunction" }).format(items)`.
fn disjunction(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [a, b] => format!("{a} or {b}"),
        [init @ .., last] => format!("{}, or {last}", init.join(", ")),
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
fn tool_work(rt: &Rt, target: ToolTarget, args: Vec<HostValue>) -> LocalBoxFuture<'static, Result<Option<Json>, Thrown>> {
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
        // `search("notes")` is shorthand for `search({ query: "notes" })` (2.3.1, #64347).
        let mut args = args;
        if def.is_none()
            && let Some(HostValue::String(query)) = args.first()
        {
            args[0] = HostValue::Object(vec![("query".into(), HostValue::String(query.clone()))]);
        }
        // Arguments arrive as host copies and become JSON here (2.3.1, `tool-runtime.ts` `toJson`).
        let json_args = args.iter().map(host_to_json).collect::<Result<Vec<_>, Thrown>>()?;
        let normalized: Vec<Option<Json>> = if json_args.is_empty() { vec![Some(json!({}))] } else { json_args };
        if normalized.len() != 1 {
            return Err(tool_runtime_error(
                DiagnosticKind::InvalidToolInput,
                format!("Tool '{name}' expects at most one input object."),
                Vec::new(),
            ));
        }
        let raw_input = normalized.into_iter().next().unwrap();
        // A freeform tool takes its one string field directly.
        let raw_input = match (&raw_input, def.as_ref().and_then(|d| d.string_parameter())) {
            (Some(Json::String(text)), Some(parameter)) => Some(json!({ (parameter.name): text })),
            _ => raw_input,
        };
        let invalid = |message: String| {
            tool_runtime_error(
                DiagnosticKind::InvalidToolInput,
                format!("Invalid input for tool '{name}': {message}"),
                if def.is_none() {
                    vec![SEARCH_SUGGESTION.into()]
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
pub fn start_tool(rt: &Rt, target: ToolTarget, args: Vec<HostValue>, made: Resolving, site: Option<Loc>) {
    let work = tool_work(rt, target, args);
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

/// The built-in `search`: arguments cross as host copies (promises in them awaited where the call is, as `await`
/// would), then the call runs to completion before any other job, and the result comes back as values that can also
/// be awaited or chained like a Promise.
pub async fn run_search(rt: &Rt, args: Vec<Value>, site: Site) -> JsResult<Value> {
    let _ = site;
    let host_args = loop {
        match crate::interp::extensions::cross_arguments(&args, "search", false)? {
            Crossed::Ready(host_args) => break host_args,
            Crossed::Waiting(waiting) => {
                for p in waiting {
                    crate::interp::runner::await_value(rt, Value::Object(p), None).await?;
                }
            }
        }
    };
    let value = rt.runner.block_on(rt, tool_work(rt, ToolTarget::Search, host_args))??;
    Ok(crate::interp::promises::promise_like(rt, from_json(rt, value.as_ref())))
}

/// What a search input error suggests (2.3.1, #64347).
pub const SEARCH_SUGGESTION: &str =
    "Pass search a query string or an object, such as search(\"issues\") or search({ namespace: \"github\" }).";

/// A tool argument as JSON, as `JSON.stringify` would give it, with a few departures so a mistake is not a silent
/// `{}` (2.3.1, `tool-runtime.ts` `toJson`): an Error gives its name, message and own fields, a Set its items, a
/// URLSearchParams its query string, and a Uint8Array asks to be encoded as text first. A `__proto__` key is dropped.
pub fn host_to_json(value: &HostValue) -> Result<Option<Json>, Thrown> {
    Ok(Some(match value {
        HostValue::Undefined | HostValue::Function(_) | HostValue::Unrepresentable(_) => return Ok(None),
        HostValue::Null => Json::Null,
        HostValue::Bool(b) => Json::Bool(*b),
        HostValue::String(s) => Json::String(s.clone()),
        HostValue::Number(n) => crate::data::json_number(*n),
        HostValue::Date(t) => {
            if t.is_finite() { Json::String(crate::stdlib::date::iso_string(*t)) } else { Json::Null }
        }
        HostValue::Url(href) => Json::String(href.clone()),
        HostValue::SearchParams(pairs) => Json::String(
            url::form_urlencoded::Serializer::new(String::new()).extend_pairs(pairs.iter()).finish(),
        ),
        HostValue::Headers(list) => {
            // `Object.fromEntries(headers)`: a repeated name (only `set-cookie`) keeps its last value.
            let mut out: Vec<(String, Json)> = Vec::new();
            for (k, v) in list {
                match out.iter_mut().find(|(name, _)| name == k) {
                    Some(existing) => existing.1 = Json::String(v.clone()),
                    None => out.push((k.clone(), Json::String(v.clone()))),
                }
            }
            Json::Object(crate::data::js_ordered(out))
        }
        HostValue::Bytes(_) | HostValue::Buffer(_) => {
            return Err(tool_runtime_error(
                DiagnosticKind::InvalidDataValue,
                "A Uint8Array cannot cross to a tool; pass text instead, e.g. `new TextDecoder().decode(bytes)` or `bytes.toBase64()`.".to_string(),
                Vec::new(),
            ));
        }
        HostValue::Array(items) | HostValue::Set(items) => Json::Array(
            items.iter().map(|item| Ok(host_to_json(item)?.unwrap_or(Json::Null))).collect::<Result<_, Thrown>>()?,
        ),
        HostValue::Map(_) | HostValue::RegExp { .. } => Json::Object(Map::new()),
        HostValue::Error { name, message, fields, .. } => {
            let mut out = vec![("name".to_string(), Json::String(name.clone())), ("message".to_string(), Json::String(message.clone()))];
            object_fields(fields, &mut out)?;
            Json::Object(crate::data::js_ordered(out))
        }
        HostValue::Object(fields) => {
            let mut out = Vec::new();
            object_fields(fields, &mut out)?;
            Json::Object(crate::data::js_ordered(out))
        }
        HostValue::Live(_) => Json::Object(Map::new()),
    }))
}

fn object_fields(fields: &[(String, HostValue)], out: &mut Vec<(String, Json)>) -> Result<(), Thrown> {
    for (key, item) in fields {
        if key == "__proto__" {
            continue;
        }
        if let Some(json) = host_to_json(item)? {
            match out.iter_mut().find(|(k, _)| k == key) {
                Some(existing) => existing.1 = json,
                None => out.push((key.clone(), json)),
            }
        }
    }
    Ok(())
}

fn tool_error(e: ToolCallError) -> Thrown {
    match e {
        ToolCallError::Failure(m) | ToolCallError::InvalidOutput(m) => Thrown::ToolFailure(m.into()),
    }
}

