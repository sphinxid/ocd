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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamespaceSummary {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub description: Option<String>,
    pub count: usize,
    pub entries: Vec<Listing>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub total: usize,
    pub shown: usize,
    pub namespaces: Vec<NamespaceSummary>,
}

const DESCRIPTION_LIMIT: usize = 120;
const CHARACTERS_PER_TOKEN: f64 = 4.0;
/// Inline budget in "tokens" (characters / 4, rounded).
pub const INLINE_BUDGET: usize = 2_000;

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

fn cost(text: &str) -> i64 {
    // Math.round(length / 4)
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

/// `- name (N tools[, K shown | , none shown])[ // description]`
pub fn namespace_line(namespace: &NamespaceSummary) -> String {
    let count = if namespace.count == 1 { "1 tool".to_string() } else { format!("{} tools", namespace.count) };
    let label = if namespace.entries.len() == namespace.count {
        count
    } else if namespace.entries.is_empty() {
        format!("{count}, none shown")
    } else {
        format!("{count}, {} shown", namespace.entries.len())
    };
    match &namespace.description {
        None => format!("- {} ({label})", namespace.name),
        Some(d) => format!("- {} ({label}) // {d}", namespace.name),
    }
}

/// Keeps every namespace visible, then selects full listings one per namespace per round, shorter listings first,
/// until the inline budget is exhausted. Pinned listings are always shown.
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

    struct Ns {
        name: String,
        description: Option<String>,
        listings: Vec<Listing>,
        order: Vec<(usize, i64)>,
        selected: Vec<bool>,
        index: usize,
    }
    let mut namespaces: Vec<Ns> = groups
        .into_iter()
        .map(|(name, entries)| {
            let description = namespace_meta.iter().find(|(k, _)| *k == name).and_then(|(_, d)| d.clone());
            let mut listings: Vec<(Listing, bool)> = entries
                .iter()
                .map(|entry| {
                    let first_line = js_trim(entry.description.split('\n').next().unwrap_or("")).to_string();
                    let description = if utf16_len(&first_line) > DESCRIPTION_LIMIT {
                        format!("{}...", utf16_prefix(&first_line, DESCRIPTION_LIMIT - 3))
                    } else {
                        first_line
                    };
                    let suffix = if description.is_empty() { String::new() } else { format!(" // {description}") };
                    (Listing { path: entry.path.clone(), line: format!("  - {}{}", entry.signature, suffix) }, entry.pinned)
                })
                .collect();
            listings.sort_by(|a, b| js_lt(&a.0.path, &b.0.path));
            let pinned_paths: HashSet<String> =
                listings.iter().filter(|(_, p)| *p).map(|(l, _)| l.path.clone()).collect();
            let listings: Vec<Listing> = listings.into_iter().map(|(l, _)| l).collect();
            // Duplicate paths: only the first listing with a pinned path is selected (`listings.find`).
            let mut selected_fixed = vec![false; listings.len()];
            for path in &pinned_paths {
                if let Some(i) = listings.iter().position(|l| &l.path == path) {
                    selected_fixed[i] = true;
                }
            }
            let mut ranked: Vec<(usize, i64)> = listings.iter().enumerate().map(|(i, l)| (i, cost(&l.line))).collect();
            ranked.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| js_lt(&listings[a.0].path, &listings[b.0].path)));
            let order: Vec<(usize, i64)> = ranked.into_iter().filter(|(i, _)| !selected_fixed[*i]).collect();
            Ns { name, description, listings, order, selected: selected_fixed, index: 0 }
        })
        .collect();

    let mut remaining = budget
        - namespaces
            .iter()
            .map(|ns| {
                cost(&namespace_line(&NamespaceSummary {
                    name: ns.name.clone(),
                    description: ns.description.clone(),
                    count: ns.listings.len(),
                    entries: Vec::new(),
                }))
            })
            .sum::<i64>()
        - namespaces
            .iter()
            .flat_map(|ns| ns.listings.iter().zip(ns.selected.iter()).filter(|(_, s)| **s).map(|(l, _)| cost(&l.line)))
            .sum::<i64>();
    let mut active: Vec<bool> = vec![true; namespaces.len()];
    while active.iter().any(|a| *a) {
        for (i, ns) in namespaces.iter_mut().enumerate() {
            if !active[i] {
                continue;
            }
            let candidate = ns.order.get(ns.index).copied();
            match candidate {
                Some((listing, c)) if c <= remaining => {
                    ns.selected[listing] = true;
                    ns.index += 1;
                    remaining -= c;
                    if ns.index == ns.order.len() {
                        active[i] = false;
                    }
                }
                _ => active[i] = false,
            }
        }
    }
    let summaries: Vec<NamespaceSummary> = namespaces
        .iter()
        .map(|ns| NamespaceSummary {
            name: ns.name.clone(),
            description: ns.description.clone(),
            count: ns.listings.len(),
            entries: ns.listings.iter().zip(ns.selected.iter()).filter(|(_, s)| **s).map(|(l, _)| l.clone()).collect(),
        })
        .collect();
    Summary { total: tools.len(), shown: summaries.iter().map(|n| n.entries.len()).sum(), namespaces: summaries }
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

pub const EMPTY_CATALOG: &str = "No Code Mode tools are currently available. Later Code Mode catalog updates may add or remove tools. Do not call `execute` unless there is at least one available Code Mode tool.";
pub const REMOVED_CATALOG: &str =
    "Code Mode tools are no longer available. Do not use any previously listed Code Mode tools.";

/// The initial catalog instruction (keyed `core/codemode` upstream).
pub fn render(catalog: &Summary) -> String {
    if catalog.total == 0 {
        return EMPTY_CATALOG.into();
    }
    let mut tools: Vec<String> = Vec::new();
    for ns in &catalog.namespaces {
        tools.push(namespace_line(ns));
        tools.extend(ns.entries.iter().map(|e| e.line.clone()));
    }
    format!("{}\n\n{}", prompt(catalog.shown < catalog.total), tools.join("\n"))
}

struct Diff<'a, A> {
    added: Vec<&'a A>,
    removed: Vec<&'a A>,
    changed: Vec<(&'a A, &'a A)>,
}

impl<A> Diff<'_, A> {
    fn any(&self) -> bool {
        !self.added.is_empty() || !self.removed.is_empty() || !self.changed.is_empty()
    }
}

fn diff_by_key<'a, A>(
    previous: &[&'a A],
    current: &[&'a A],
    key: impl Fn(&A) -> String,
    changed: impl Fn(&A, &A) -> bool,
) -> Diff<'a, A> {
    let current_keys: HashSet<String> = current.iter().map(|v| key(v)).collect();
    // Map semantics: the last previous value for a key wins.
    let previous_by_key = |k: &str| previous.iter().rev().find(|v| key(v) == k).copied();
    Diff {
        added: current.iter().filter(|v| previous_by_key(&key(v)).is_none()).copied().collect(),
        removed: previous.iter().filter(|v| !current_keys.contains(&key(v))).copied().collect(),
        changed: current
            .iter()
            .filter_map(|v| previous_by_key(&key(v)).filter(|before| changed(before, v)).map(|before| (before, *v)))
            .collect(),
    }
}

/// The instruction text when the catalog changes between steps: a delta when shorter, else a replacement.
pub fn update(previous: &Summary, current: &Summary) -> String {
    let replacement = format!(
        "The Code Mode tool catalog has changed. This catalog supersedes the previous Code Mode tool catalog.\n\n{}",
        render(current)
    );
    if current.total == 0 {
        return replacement;
    }
    let previous_complete = previous.shown == previous.total;
    let current_complete = current.shown == current.total;
    if previous_complete != current_complete {
        return replacement;
    }
    fn with_desc(s: &Summary) -> Vec<&NamespaceSummary> {
        s.namespaces.iter().filter(|n| n.description.is_some()).collect()
    }
    let (pd, cd) = (with_desc(previous), with_desc(current));
    let descriptions = diff_by_key(&pd, &cd, |n| n.name.clone(), |b, a| b.description != a.description);
    if descriptions.any() {
        return replacement;
    }
    let pe: Vec<&Listing> = previous.namespaces.iter().flat_map(|n| n.entries.iter()).collect();
    let ce: Vec<&Listing> = current.namespaces.iter().flat_map(|n| n.entries.iter()).collect();
    let diff = diff_by_key(&pe, &ce, |e| e.path.clone(), |b, a| b.line != a.line);
    let entries_changed = diff.any();
    let js_len = |s: &str| utf16_len(s);

    if !current_complete {
        if entries_changed {
            return replacement;
        }
        let pn: Vec<&NamespaceSummary> = previous.namespaces.iter().collect();
        let cn: Vec<&NamespaceSummary> = current.namespaces.iter().collect();
        let namespaces = diff_by_key(&pn, &cn, |n| n.name.clone(), |b, a| b.count != a.count);
        if !namespaces.any() {
            return replacement;
        }
        let mut parts = vec!["The Code Mode tool catalog has changed.".to_string()];
        if !namespaces.added.is_empty() {
            parts.push(format!(
                "New tool namespaces are available: {}.",
                namespaces.added.iter().map(|n| format!("`{}` ({} tools)", n.name, n.count)).collect::<Vec<_>>().join(", ")
            ));
        }
        if !namespaces.changed.is_empty() {
            parts.push(format!(
                "The following namespace inventories changed; search them again before relying on previous results: {}.",
                namespaces
                    .changed
                    .iter()
                    .map(|(_, c)| format!("`{}` now has {} tools", c.name, c.count))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !namespaces.removed.is_empty() {
            parts.push(format!(
                "The following tool namespaces are no longer available and must not be used: {}.",
                namespaces.removed.iter().map(|n| format!("`{}`", n.name)).collect::<Vec<_>>().join(", ")
            ));
        }
        let delta = parts.join("\n\n");
        return if js_len(&delta) < js_len(&replacement) { delta } else { replacement };
    }

    if !entries_changed {
        return replacement;
    }
    let mut parts = vec!["The Code Mode tool catalog has changed.".to_string()];
    if !diff.added.is_empty() {
        let mut lines = vec!["New tools are available in addition to those previously listed:".to_string()];
        lines.extend(diff.added.iter().map(|e| e.line.clone()));
        parts.push(lines.join("\n"));
    }
    if !diff.changed.is_empty() {
        let mut lines = vec!["Changed tool listings supersede the previously listed ones:".to_string()];
        lines.extend(diff.changed.iter().map(|(_, c)| c.line.clone()));
        parts.push(lines.join("\n"));
    }
    if !diff.removed.is_empty() {
        parts.push(format!(
            "The following tools are no longer available and must not be called: {}.",
            diff.removed.iter().map(|e| tool_expression(&e.path)).collect::<Vec<_>>().join(", ")
        ));
    }
    let delta = parts.join("\n\n");
    if js_len(&delta) < js_len(&replacement) { delta } else { replacement }
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
        assert_eq!(summary.shown, 1);
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
        assert!(summary.shown < 50);
        let text = render(&summary);
        assert!(text.contains("The catalog is partial."));
        assert!(text.contains(&format!("- ns (50 tools, {} shown)", summary.shown)));
    }
}
