//! Model-facing Code Mode catalog: inline-budget summarization and the instruction texts. Port of
//! `packages/core/src/codemode/{catalog,instructions}.ts` and the catalog half of `codemode/tool.ts`.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::tool_runtime::{tool_expression, ToolDescription, SEARCH_SIGNATURE};

/// A catalog tool leaf.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogTool {
    pub name: String,
    pub description: String,
    pub signature: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub pinned: Option<bool>,
}

/// A catalog namespace with nested entries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogNamespace {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub description: Option<String>,
    pub tools: Vec<CatalogEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum CatalogEntry {
    Tool(CatalogTool),
    Namespace(CatalogNamespace),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listing {
    pub path: String,
    pub line: String,
}

/// A namespace as the stored summary records it. `tools` holds every tool's path, so an update can tell a new tool
/// from one that was not shown. Field order is the hashed JSON's.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamespaceSummary {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub description: Option<String>,
    pub tools: Vec<String>,
    pub entries: Vec<Listing>,
}

/// `CodeModeCatalog.Summary` (2.2.7): a summary stored in the older shape (no namespace `tools`) does not decode, so
/// a resumed session gets the whole catalog again. Unknown keys are ignored, as Effect's decoding does.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub total: usize,
    pub namespaces: Vec<NamespaceSummary>,
}

const DESCRIPTION_LIMIT: usize = 120;
const CHARACTERS_PER_TOKEN: f64 = 4.0;
/// Inline budget in "tokens" (characters / 4, rounded).
pub const INLINE_BUDGET: usize = 2_000;
/// What the new listings of an update may cost while the catalog is partial.
const UPDATE_BUDGET: f64 = 1_000.0;

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// `cost(text)`: `Math.round(length / 4)`.
pub fn cost(text: &str) -> i64 {
    ((utf16_len(text) as f64 / CHARACTERS_PER_TOKEN) + 0.5).floor() as i64
}

fn js_lt(a: &str, b: &str) -> std::cmp::Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

fn js_trim(s: &str) -> &str {
    s.trim_matches(|c: char| (c as u32) < 0x10000 && crate::number::is_js_whitespace(c as u16))
}

fn utf16_prefix(s: &str, units: usize) -> String {
    let u: Vec<u16> = s.encode_utf16().take(units).collect();
    String::from_utf16_lossy(&u)
}

struct FlatTool {
    path: String,
    description: String,
    signature: String,
    pinned: bool,
}

fn flatten(entries: &[CatalogEntry], path: &[String], tools: &mut Vec<FlatTool>, namespaces: &mut Vec<(String, Option<String>)>) {
    for entry in entries {
        match entry {
            CatalogEntry::Tool(t) => {
                let mut p = path.to_vec();
                p.push(t.name.clone());
                tools.push(FlatTool {
                    path: p.join("."),
                    description: t.description.clone(),
                    signature: t.signature.clone(),
                    pinned: t.pinned == Some(true),
                });
            }
            CatalogEntry::Namespace(ns) => {
                let mut next = path.to_vec();
                next.push(ns.name.clone());
                let key = next.join(".");
                if let Some(existing) = namespaces.iter_mut().find(|(k, _)| *k == key) {
                    existing.1 = ns.description.clone();
                } else {
                    namespaces.push((key, ns.description.clone()));
                }
                flatten(&ns.tools, &next, tools, namespaces);
            }
        }
    }
}

/// `listing(entry)`: the tool's signature and the first line of its description, cut to 117 characters plus `...`.
fn listing(tool: &FlatTool) -> Listing {
    let first_line = js_trim(tool.description.split('\n').next().unwrap_or("")).to_string();
    let description =
        if utf16_len(&first_line) > DESCRIPTION_LIMIT { format!("{}...", utf16_prefix(&first_line, DESCRIPTION_LIMIT - 3)) } else { first_line };
    let suffix = if description.is_empty() { String::new() } else { format!(" // {description}") };
    Listing { path: tool.path.clone(), line: format!("  - {}{}", tool.signature, suffix) }
}

/// `- name (N tools[, K shown | , none shown])[ // description]`, where `shown` defaults to the listed entries.
pub fn namespace_line(namespace: &NamespaceSummary, shown: Option<usize>) -> String {
    let shown = shown.unwrap_or(namespace.entries.len());
    let total = namespace.tools.len();
    let count = if total == 1 { "1 tool".to_string() } else { format!("{total} tools") };
    let label = if shown == total {
        count
    } else if shown == 0 {
        format!("{count}, none shown")
    } else {
        format!("{count}, {shown} shown")
    };
    match &namespace.description {
        None => format!("- {} ({label})", namespace.name),
        Some(d) => format!("- {} ({label}) // {d}", namespace.name),
    }
}

/// Keeps every namespace visible, then selects full listings one per namespace per round, in path order, until the
/// inline budget is exhausted; a namespace stops at its first listing that does not fit. Pinned listings are always
/// shown. The selection depends only on the tools.
pub fn summarize(inventory: &[CatalogEntry], budget: Option<usize>) -> Summary {
    let budget = budget.unwrap_or(INLINE_BUDGET) as i64;
    let mut tools = Vec::new();
    let mut namespace_meta = Vec::new();
    flatten(inventory, &[], &mut tools, &mut namespace_meta);
    let mut groups: Vec<(String, Vec<&FlatTool>)> = Vec::new();
    for tool in &tools {
        let name = tool.path.split('.').next().unwrap_or(&tool.path).to_string();
        match groups.iter_mut().find(|(n, _)| *n == name) {
            Some(g) => g.1.push(tool),
            None => groups.push((name, vec![tool])),
        }
    }
    groups.sort_by(|a, b| js_lt(&a.0, &b.0));

    struct Ns<'a> {
        summary: NamespaceSummary,
        order: Vec<&'a FlatTool>,
        index: usize,
    }
    let mut namespaces: Vec<Ns> = groups
        .into_iter()
        .map(|(name, mut entries)| {
            let description = namespace_meta.iter().find(|(k, _)| *k == name).and_then(|(_, d)| d.clone());
            entries.sort_by(|a, b| js_lt(&a.path, &b.path));
            let summary = NamespaceSummary {
                name,
                description,
                tools: entries.iter().map(|e| e.path.clone()).collect(),
                entries: entries.iter().filter(|e| e.pinned).map(|e| listing(e)).collect(),
            };
            Ns { summary, order: entries.into_iter().filter(|e| !e.pinned).collect(), index: 0 }
        })
        .collect();

    let mut remaining = budget
        - namespaces.iter().map(|ns| cost(&namespace_line(&NamespaceSummary { entries: Vec::new(), ..ns.summary.clone() }, None))).sum::<i64>()
        - namespaces.iter().flat_map(|ns| ns.summary.entries.iter()).map(|l| cost(&l.line)).sum::<i64>();
    let mut active: Vec<bool> = vec![true; namespaces.len()];
    while active.iter().any(|a| *a) {
        for (i, ns) in namespaces.iter_mut().enumerate() {
            if !active[i] {
                continue;
            }
            let candidate = ns.order.get(ns.index).map(|tool| listing(tool));
            match candidate {
                Some(candidate) if cost(&candidate.line) <= remaining => {
                    remaining -= cost(&candidate.line);
                    ns.summary.entries.push(candidate);
                    ns.index += 1;
                    if ns.index == ns.order.len() {
                        active[i] = false;
                    }
                }
                _ => active[i] = false,
            }
        }
    }
    Summary { total: tools.len(), namespaces: namespaces.into_iter().map(|ns| ns.summary).collect() }
}

/// `lookup(inventory)`: any tool's listing as the catalog would show it (the last tool of a path wins).
pub fn lookup(inventory: &[CatalogEntry]) -> impl Fn(&str) -> Option<Listing> + Send + Sync + 'static {
    let mut tools = Vec::new();
    flatten(inventory, &[], &mut tools, &mut Vec::new());
    let mut listings: std::collections::HashMap<String, Listing> = std::collections::HashMap::new();
    for tool in &tools {
        listings.insert(tool.path.clone(), listing(tool));
    }
    move |path| listings.get(path).cloned()
}

fn prompt(has_more_tools: bool) -> String {
    let mut out = String::from("# Code Mode\n\nThe tools listed below");
    if has_more_tools {
        out.push_str(", and `search`,");
    }
    out.push_str(" cannot be called directly: they only work as JavaScript inside the `execute` tool. Every other tool you have is a normal tool call: call it directly, never inside `execute`.\n\n");
    if has_more_tools {
        out.push_str("The catalog is partial. Before calling a tool that is not listed below, find it with the global `search(...)` function. Do not guess tool names. Each result's `path` is the complete call expression: write it out as code, for example `await tools.ns.tool_name({ ... })`. Do not use it as a key such as `tools[path]`.\n\n- ");
        out.push_str(SEARCH_SIGNATURE);
    } else {
        out.push_str("The catalog is complete. Do not guess tool names.");
    }
    out.push_str("\n\n## Code Mode tools (call only inside `execute`)");
    out
}

/// Sent once, when tools arrive that the catalog cannot list in full, so the model learns about `search`.
fn search_note() -> String {
    format!(
        "You can also look up tools that aren't listed with the global `search(...)` function inside `execute`. Before calling a tool that is not listed, find it this way. Do not guess tool names. Each result's `path` is the complete call expression: write it out as code, for example `await tools.ns.tool_name({{ ... }})`. Do not use it as a key such as `tools[path]`.\n\n- {SEARCH_SIGNATURE}"
    )
}

pub const EMPTY_CATALOG: &str = "No Code Mode tools are currently available. Later Code Mode catalog updates may add or remove tools. Do not call `execute` unless there is at least one available Code Mode tool.";
pub const REMOVED_CATALOG: &str =
    "Code Mode tools are no longer available. Do not use any previously listed Code Mode tools.";

/// Every tool is listed.
fn complete(catalog: &Summary) -> bool {
    catalog.namespaces.iter().all(|ns| ns.entries.len() == ns.tools.len())
}

/// The initial catalog instruction (keyed `core/codemode` upstream).
pub fn render(catalog: &Summary) -> String {
    if catalog.total == 0 {
        return EMPTY_CATALOG.into();
    }
    let mut tools: Vec<String> = Vec::new();
    for ns in &catalog.namespaces {
        tools.push(namespace_line(ns, None));
        tools.extend(ns.entries.iter().map(|e| e.line.clone()));
    }
    format!("{}\n\n{}", prompt(!complete(catalog)), tools.join("\n"))
}

struct Diff<'a, A> {
    added: Vec<&'a A>,
    removed: Vec<&'a A>,
    changed: Vec<(&'a A, &'a A)>,
}

/// `Instructions.diffByKey`.
fn diff_by_key<'a, A>(
    previous: &'a [A],
    current: &'a [A],
    key: impl Fn(&A) -> String,
    changed: impl Fn(&A, &A) -> bool,
) -> Diff<'a, A> {
    let current_keys: HashSet<String> = current.iter().map(&key).collect();
    // Map semantics: the last previous value for a key wins.
    let previous_by_key = |k: &str| previous.iter().rev().find(|v| key(v) == k);
    Diff {
        added: current.iter().filter(|v| previous_by_key(&key(v)).is_none()).collect(),
        removed: previous.iter().filter(|v| !current_keys.contains(&key(v))).collect(),
        changed: current
            .iter()
            .filter_map(|v| previous_by_key(&key(v)).filter(|before| changed(before, v)).map(|before| (before, v)))
            .collect(),
    }
}

/// `fit(listings, budget)`: the leading listings whose cost fits `budget`.
fn fit(listings: Vec<Listing>, budget: f64) -> Vec<Listing> {
    let mut total = 0i64;
    let end = listings.iter().position(|entry| {
        total += cost(&entry.line);
        total as f64 > budget
    });
    match end {
        Some(end) => listings.into_iter().take(end).collect(),
        None => listings,
    }
}

/// The instruction text when the catalog changes between steps (`update`): what changed for the model — namespaces
/// and tools that came or went, and signatures it was shown that changed, even if they are no longer shown — when
/// that is shorter than the whole catalog restated. `None` when nothing the model was told changed. `lookup` gives
/// any current tool's listing.
pub fn update(previous: &Summary, current: &Summary, lookup: &dyn Fn(&str) -> Option<Listing>) -> Option<String> {
    let replacement = format!(
        "The Code Mode tool catalog has changed. This catalog supersedes the previous Code Mode tool catalog.\n\n{}",
        render(current)
    );
    if current.total == 0 || previous.total == 0 {
        return Some(replacement);
    }
    let namespaces = diff_by_key(&previous.namespaces, &current.namespaces, |n| n.name.clone(), |b, a| b.description != a.description);
    // A namespace whose description changed is restated as if new.
    let restated: Vec<&NamespaceSummary> = namespaces.added.iter().copied().chain(namespaces.changed.iter().map(|(_, c)| *c)).collect();
    let unchanged: HashSet<&str> =
        current.namespaces.iter().map(|n| n.name.as_str()).filter(|name| !restated.iter().any(|r| r.name == *name)).collect();
    let kept = |path: &str| unchanged.contains(path.split('.').next().unwrap_or(path));
    let before: indexmap::IndexSet<&str> = previous.namespaces.iter().flat_map(|n| n.tools.iter().map(String::as_str)).collect();
    let after: indexmap::IndexSet<&str> = current.namespaces.iter().flat_map(|n| n.tools.iter().map(String::as_str)).collect();
    let added: Vec<&str> = after.iter().copied().filter(|path| kept(path) && !before.contains(path)).collect();
    let removed: Vec<&str> = before.iter().copied().filter(|path| kept(path) && !after.contains(path)).collect();
    let changed: Vec<Listing> = previous
        .namespaces
        .iter()
        .flat_map(|n| n.entries.iter())
        .filter(|entry| kept(&entry.path) && after.contains(entry.path.as_str()))
        .filter_map(|entry| lookup(&entry.path).filter(|listing| listing.line != entry.line))
        .collect();
    let groups = restated.len() + usize::from(!added.is_empty());
    let budget = if complete(current) { f64::INFINITY } else { UPDATE_BUDGET / (if groups == 0 { 1 } else { groups }) as f64 };

    let mut parts = vec!["The Code Mode tool catalog has changed.".to_string()];
    if !restated.is_empty() {
        let mut lines = vec!["New tools are available:".to_string()];
        for namespace in &restated {
            let shown = fit(namespace.tools.iter().filter_map(|path| lookup(path)).collect(), budget);
            lines.push(namespace_line(namespace, Some(shown.len())));
            lines.extend(shown.into_iter().map(|entry| entry.line));
        }
        parts.push(lines.join("\n"));
    }
    if !added.is_empty() {
        let shown = fit(added.iter().filter_map(|path| lookup(path)).collect(), budget);
        let rest = added.len() - shown.len();
        let mut lines = vec!["New tools are available in addition to those previously listed:".to_string()];
        lines.extend(shown.into_iter().map(|entry| entry.line));
        if rest != 0 {
            lines.push(format!("  - {rest} more; search for them"));
        }
        parts.push(lines.join("\n"));
    }
    if complete(previous) && !complete(current) {
        parts.push(search_note());
    }
    if !changed.is_empty() {
        let mut lines = vec!["Changed tool listings supersede the previously listed ones:".to_string()];
        lines.extend(changed.into_iter().map(|entry| entry.line));
        parts.push(lines.join("\n"));
    }
    if !removed.is_empty() {
        parts.push(format!(
            "The following tools are no longer available and must not be called: {}.",
            removed.iter().map(|path| tool_expression(path)).collect::<Vec<_>>().join(", ")
        ));
    }
    if !namespaces.removed.is_empty() {
        parts.push(format!(
            "The following tool namespaces are no longer available and must not be used: {}.",
            namespaces.removed.iter().map(|n| format!("`{}`", n.name)).collect::<Vec<_>>().join(", ")
        ));
    }
    if parts.len() == 1 {
        return None;
    }
    let delta = parts.join("\n\n");
    Some(if utf16_len(&delta) < utf16_len(&replacement) { delta } else { replacement })
}

/// Builds the catalog inventory tree from a runtime catalog, namespace metadata, and pinned paths (core's
/// `CodeModeTool.catalog` + `renderCatalog`).
pub fn inventory(
    catalog: &[ToolDescription],
    namespaces: &[(String, Option<String>)],
    pinned: &HashSet<String>,
) -> Vec<CatalogEntry> {
    #[derive(Default)]
    struct Node {
        tool: Option<CatalogTool>,
        namespace: Option<Option<String>>,
        children: Vec<(String, Node)>,
    }
    fn get<'a>(root: &'a mut Node, path: &str) -> &'a mut Node {
        let mut node = root;
        for name in path.split('.') {
            let i = match node.children.iter().position(|(n, _)| n == name) {
                Some(i) => i,
                None => {
                    node.children.push((name.to_string(), Node::default()));
                    node.children.len() - 1
                }
            };
            node = &mut node.children[i].1;
        }
        node
    }
    let mut root = Node::default();
    for (name, description) in namespaces {
        get(&mut root, name).namespace = Some(description.clone());
    }
    for tool in catalog {
        let node = get(&mut root, &tool.path);
        node.tool = Some(CatalogTool {
            name: tool.path.rsplit('.').next().unwrap_or(&tool.path).to_string(),
            description: tool.description.clone(),
            signature: tool.signature.clone(),
            pinned: Some(pinned.contains(&tool.path)),
        });
    }
    fn render_node(node: &Node) -> Vec<CatalogEntry> {
        let mut children: Vec<&(String, Node)> = node.children.iter().collect();
        children.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()));
        let mut out = Vec::new();
        for (name, child) in children {
            let tools = render_node(child);
            let namespace = if child.namespace.is_none() && tools.is_empty() {
                None
            } else {
                Some(CatalogEntry::Namespace(CatalogNamespace {
                    name: name.clone(),
                    description: child.namespace.clone().flatten(),
                    tools,
                }))
            };
            match (&child.tool, namespace) {
                (None, None) => {}
                (None, Some(ns)) => out.push(ns),
                (Some(t), None) => out.push(CatalogEntry::Tool(t.clone())),
                (Some(t), Some(ns)) => {
                    out.push(CatalogEntry::Tool(t.clone()));
                    out.push(ns);
                }
            }
        }
        out
    }
    render_node(&root)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, sig: &str, desc: &str) -> CatalogEntry {
        CatalogEntry::Tool(CatalogTool { name: name.into(), description: desc.into(), signature: sig.into(), pinned: None })
    }

    #[test]
    fn complete_catalog() {
        let inv = vec![CatalogEntry::Namespace(CatalogNamespace {
            name: "github".into(),
            description: Some("GitHub".into()),
            tools: vec![tool("list", "tools.github.list(): Promise<void>", "List things\nmore")],
        })];
        let summary = summarize(&inv, None);
        assert_eq!(summary.total, 1);
        assert_eq!(summary.namespaces[0].entries.len(), 1);
        let text = render(&summary);
        assert!(text.ends_with("## Code Mode tools (call only inside `execute`)\n\n- github (1 tool) // GitHub\n  - tools.github.list(): Promise<void> // List things"));
        assert!(text.contains("The catalog is complete."));
    }

    #[test]
    fn partial_catalog_respects_budget() {
        let tools: Vec<CatalogEntry> =
            (0..50).map(|i| tool(&format!("t{i:02}"), &format!("tools.ns.t{i:02}({{ x: string }}): Promise<void>"), "d")).collect();
        let inv = vec![CatalogEntry::Namespace(CatalogNamespace { name: "ns".into(), description: None, tools })];
        let summary = summarize(&inv, Some(100));
        let shown = summary.namespaces[0].entries.len();
        assert!(shown < 50);
        let text = render(&summary);
        assert!(text.contains("The catalog is partial."));
        assert!(text.contains(&format!("- ns (50 tools, {} shown)", shown)));
    }
}
