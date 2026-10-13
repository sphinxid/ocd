//! Session transcript: row model (`rows.ts`, `grouping/*`) and rendering of every message,
//! part, tool, group and footer kind (`routes/session/index.tsx`, `group-view.tsx`).

use super::diff;
use super::highlight::{filetype, highlight};
use super::markdown::{self, MdOpts};
use super::text::*;
use crate::api::types::*;
use crate::config::Config;
use crate::theme::{Rgba, SyntaxStyle, Theme, tint};
use crate::util::locale::{self, titlecase, width as sw};
use ratatui::style::{Modifier, Style};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PartRef {
    pub message: String,
    pub part: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GroupKind {
    Activity,
    Reasoning,
    Exploration,
    Instructions,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Entry {
    Message(String),
    CompactionQueued(String),
    Part(PartRef),
    Footer(String),
    TurnUsage(Vec<String>),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Node {
    Entry(Entry),
    Group { kind: GroupKind, children: Vec<Node>, size: usize },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Row {
    Entry(Entry),
    Group { kind: GroupKind, children: Vec<Node>, completed: bool, pending: Vec<PartRef> },
}

struct Proj {
    entry: Entry,
    path: Vec<GroupKind>,
    closes_previous: bool,
    reasoning_done: Option<bool>,
}

fn append_node(nodes: &mut Vec<Node>, entry: Entry, path: &[GroupKind]) {
    let Some(kind) = path.first() else {
        nodes.push(Node::Entry(entry));
        return;
    };
    if let Some(Node::Group { kind: k, children, size }) = nodes.last_mut()
        && k == kind
    {
        *size += 1;
        append_node(children, entry, &path[1..]);
        return;
    }
    let mut children = Vec::new();
    append_node(&mut children, entry, &path[1..]);
    nodes.push(Node::Group { kind: *kind, children, size: 1 });
}

pub fn reasoning_content(text: &str) -> String {
    text.replacen("[REDACTED]", "", 1).trim().to_string()
}

pub fn canonical_tool(name: &str) -> &str {
    match name {
        "bash" => "shell",
        "task" => "subagent",
        "apply_patch" => "patch",
        n => n,
    }
}

const EXPLORATION: [&str; 5] = ["read", "glob", "grep", "webfetch", "websearch"];

fn part_path(part: &Content, verbosity: &str) -> Vec<GroupKind> {
    if let Content::Tool(t) = part
        && t.name.to_lowercase() == "question"
    {
        return vec![];
    }
    let mut p = Vec::new();
    if verbosity == "low" && !matches!(part, Content::Text { .. }) {
        p.push(GroupKind::Activity);
    }
    match part {
        Content::Reasoning { .. } => p.push(GroupKind::Reasoning),
        Content::Tool(t) if EXPLORATION.contains(&t.name.to_lowercase().as_str()) => p.push(GroupKind::Exploration),
        _ => {}
    }
    p
}

pub fn instruction_paths(m: &Message) -> Vec<String> {
    if m.kind != "synthetic" {
        return vec![];
    }
    m.metadata
        .as_ref()
        .and_then(|md| md.get("instruction"))
        .and_then(|i| i.get("paths"))
        .and_then(|p| p.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default()
}

/// `reduceSessionRows` + compaction-queued insertion + pending partition.
pub fn build_rows(
    messages: &[Message],
    inputs: &HashSet<String>,
    pending_compactions: &[String],
    turn_tokens: bool,
    verbosity: &str,
    blocked: &HashSet<String>,
) -> Vec<Row> {
    build_rows_with(messages, inputs, pending_compactions, turn_tokens, verbosity, blocked, is_legacy(messages))
}

/// Histories without `idle` markers use the legacy turn rules.
pub fn is_legacy(messages: &[Message]) -> bool {
    !messages.iter().rev().any(|m| m.kind == "idle")
}

/// [`build_rows`] with the legacy flag given explicitly, so a suffix of the message list can be
/// rebuilt on its own (the flag is a property of the whole list).
pub fn build_rows_with(
    messages: &[Message],
    inputs: &HashSet<String>,
    pending_compactions: &[String],
    turn_tokens: bool,
    verbosity: &str,
    blocked: &HashSet<String>,
    legacy: bool,
) -> Vec<Row> {
    let running_compactions: Vec<&Message> =
        messages.iter().filter(|m| m.kind == "compaction" && m.status.as_deref() == Some("running")).collect();
    let mut pending: HashSet<String> = running_compactions.iter().map(|m| m.id.clone()).collect();
    pending.extend(inputs.iter().cloned());
    let mut steps: Vec<&Message> = Vec::new();
    let mut entries: Vec<Proj> = Vec::new();
    let flush = |entries: &mut Vec<Proj>, steps: &mut Vec<&Message>| {
        if !turn_tokens {
            return;
        }
        let ids: Vec<String> =
            steps.iter().filter(|s| s.tokens.as_ref().is_some_and(|t| t.total() > 0.0)).map(|s| s.id.clone()).collect();
        steps.clear();
        if ids.is_empty() {
            return;
        }
        entries.push(Proj { entry: Entry::TurnUsage(ids), path: vec![], closes_previous: true, reasoning_done: None });
    };
    let ordered: Vec<&Message> = messages
        .iter()
        .filter(|m| !pending.contains(&m.id))
        .chain(running_compactions.iter().copied())
        .chain(messages.iter().filter(|m| inputs.contains(&m.id)))
        .collect();
    for m in ordered {
        if m.kind != "assistant" {
            if m.kind == "idle" {
                flush(&mut entries, &mut steps);
                if m.error.is_some() {
                    entries.push(Proj { entry: Entry::Message(m.id.clone()), path: vec![], closes_previous: true, reasoning_done: None });
                }
                continue;
            }
            if m.kind == "synthetic" && m.description.as_deref().unwrap_or("").trim().is_empty() {
                continue;
            }
            let path = if instruction_paths(m).is_empty() {
                vec![]
            } else if verbosity == "low" {
                vec![GroupKind::Activity, GroupKind::Instructions]
            } else {
                vec![GroupKind::Instructions]
            };
            entries.push(Proj {
                entry: Entry::Message(m.id.clone()),
                path,
                closes_previous: !pending.contains(&m.id),
                reasoning_done: None,
            });
            continue;
        }
        steps.push(m);
        let (mut text_n, mut reason_n) = (0, 0);
        for part in &m.content {
            let pid = match part {
                Content::Tool(t) => t.id.clone(),
                Content::Text { .. } => {
                    text_n += 1;
                    format!("text:{}", text_n - 1)
                }
                Content::Reasoning { .. } => {
                    reason_n += 1;
                    format!("reasoning:{}", reason_n - 1)
                }
            };
            match part {
                Content::Text { text, .. } | Content::Reasoning { text, .. } if text.trim().is_empty() => continue,
                _ => {}
            }
            let done = match part {
                Content::Reasoning { time, .. } => Some(time.as_ref().and_then(|t| t.completed).is_some()),
                _ => None,
            };
            entries.push(Proj {
                entry: Entry::Part(PartRef { message: m.id.clone(), part: pid }),
                path: part_path(part, verbosity),
                closes_previous: true,
                reasoning_done: done,
            });
        }
        let terminal = m.finish.as_deref().is_some_and(|f| f != "tool-calls" && f != "unknown") || m.error.is_some();
        if terminal || m.retry.is_some() {
            entries.push(Proj { entry: Entry::Footer(m.id.clone()), path: vec![], closes_previous: true, reasoning_done: None });
        }
        if terminal && legacy {
            flush(&mut entries, &mut steps);
        }
    }
    // Group adjacent entries.
    let mut nodes: Vec<(Node, Vec<(bool, Option<bool>)>)> = Vec::new();
    for p in entries {
        let meta = (p.closes_previous, p.reasoning_done);
        match (nodes.last_mut(), p.path.first()) {
            (Some((Node::Group { kind, children, size }, metas)), Some(k)) if kind == k => {
                *size += 1;
                append_node(children, p.entry, &p.path[1..]);
                metas.push(meta);
            }
            _ => {
                let mut v = Vec::new();
                append_node(&mut v, p.entry, &p.path);
                nodes.push((v.pop().unwrap(), vec![meta]));
            }
        }
    }
    let n = nodes.len();
    let mut rows = Vec::new();
    let firsts: Vec<(bool, bool)> = nodes
        .iter()
        .map(|(node, metas)| (matches!(node, Node::Group { .. }), metas.first().map(|m| m.0).unwrap_or(true)))
        .collect();
    for (i, (node, metas)) in nodes.into_iter().enumerate() {
        match node {
            Node::Entry(e) => rows.push(Row::Entry(e)),
            Node::Group { kind, children, .. } => {
                let next = if i + 1 < n { Some(firsts[i + 1]) } else { None };
                let completed = next.is_some_and(|(is_group, closes)| is_group || closes)
                    || (kind == GroupKind::Reasoning && metas.iter().all(|m| m.1 == Some(true)));
                rows.push(Row::Group { kind, children, completed, pending: vec![] });
            }
        }
    }
    // Partition permission-blocked tools to the end of exploration groups.
    for row in rows.iter_mut() {
        if let Row::Group { kind, children, pending, .. } = row
            && (*kind == GroupKind::Exploration || *kind == GroupKind::Activity)
        {
            let is_blocked = |n: &Node| matches!(n, Node::Entry(Entry::Part(r)) if blocked.contains(&r.part));
            if *kind == GroupKind::Exploration {
                let (b, nb): (Vec<Node>, Vec<Node>) = children.drain(..).partition(is_blocked);
                children.extend(nb);
                children.extend(b);
            }
            let mut refs = Vec::new();
            collect_refs(children, &mut refs);
            *pending = refs.into_iter().filter(|r| blocked.contains(&r.part)).collect();
        }
    }
    // Pending compactions go before the first pending input row.
    let pos = rows
        .iter()
        .position(|r| matches!(r, Row::Entry(Entry::Message(id)) if inputs.contains(id)))
        .unwrap_or(rows.len());
    for (k, id) in pending_compactions.iter().enumerate() {
        rows.insert(pos + k, Row::Entry(Entry::CompactionQueued(id.clone())));
    }
    rows
}

fn collect_refs(nodes: &[Node], out: &mut Vec<PartRef>) {
    for n in nodes {
        match n {
            Node::Entry(Entry::Part(r)) => out.push(r.clone()),
            Node::Entry(_) => {}
            Node::Group { children, .. } => collect_refs(children, out),
        }
    }
}

fn collect_entries<'a>(nodes: &'a [Node], out: &mut Vec<&'a Entry>) {
    for n in nodes {
        match n {
            Node::Entry(e) => out.push(e),
            Node::Group { children, .. } => collect_entries(children, out),
        }
    }
}

fn first_ref(nodes: &[Node]) -> Option<(String, String)> {
    for n in nodes {
        match n {
            Node::Entry(Entry::Part(r)) => return Some((r.message.clone(), r.part.clone())),
            Node::Entry(Entry::Message(id)) => return Some((id.clone(), "message".into())),
            Node::Entry(_) => {}
            Node::Group { children, .. } => {
                if let Some(r) = first_ref(children) {
                    return Some(r);
                }
            }
        }
    }
    None
}

/// `groupID`: `JSON([messageID, partID, kind, level])`.
pub fn group_id(kind: GroupKind, children: &[Node], level: usize) -> String {
    let (m, p) = first_ref(children).unwrap_or_default();
    let k = match kind {
        GroupKind::Activity => "activity",
        GroupKind::Reasoning => "reasoning",
        GroupKind::Exploration => "exploration",
        GroupKind::Instructions => "instructions",
    };
    serde_json::json!([m, p, k, level]).to_string()
}

/// Everything a row renderer needs.
pub struct RCtx<'a> {
    pub theme: &'a Theme,
    pub syntax: &'a SyntaxStyle,
    pub thinking_syntax: &'a SyntaxStyle,
    pub config: &'a Config,
    /// Content width (`ctx.width`).
    pub width: usize,
    pub term_width: usize,
    pub models: &'a [ModelInfo],
    pub messages: &'a [Message],
    pub session_agent: Option<&'a str>,
    pub agent_colors: &'a HashMap<String, Rgba>,
    pub default_agent_color: Rgba,
    pub expanded: &'a HashMap<String, bool>,
    pub permission_tool: Option<&'a str>,
    pub pending_inputs: &'a HashMap<String, String>,
    pub now: i64,
    pub animations: bool,
    pub redo_key: String,
    pub running_shells: &'a HashSet<String>,
    pub shell_output: &'a HashMap<String, String>,
    pub running_sessions: &'a HashSet<String>,
    pub home: Option<String>,
    /// Optional `message id → index in messages` map (first occurrence); without it lookups scan.
    pub index: Option<&'a HashMap<String, usize>>,
    /// Precomputed [`is_legacy`] of `messages`.
    pub legacy: Option<bool>,
}

impl RCtx<'_> {
    fn t(&self) -> &Theme {
        self.theme
    }
    fn agent_color(&self, agent: &str) -> Rgba {
        self.agent_colors.get(agent).copied().unwrap_or(self.default_agent_color)
    }
    fn message(&self, id: &str) -> Option<&Message> {
        self.position(id).map(|i| &self.messages[i])
    }
    fn position(&self, id: &str) -> Option<usize> {
        if let Some(i) = self.index.and_then(|ix| ix.get(id)).copied()
            && self.messages.get(i).is_some_and(|m| m.id == id)
        {
            return Some(i);
        }
        self.messages.iter().position(|m| m.id == id)
    }
    fn legacy(&self) -> bool {
        self.legacy.unwrap_or_else(|| is_legacy(self.messages))
    }
    fn is_expanded(&self, id: &str, default: bool) -> bool {
        self.expanded.get(id).copied().unwrap_or(default)
    }
    fn spinner(&self) -> String {
        if self.animations { SPINNER_GLYPH.to_string() } else { "⋯".into() }
    }
    fn conceal(&self) -> bool {
        self.config.markdown_mode() == "rendered"
    }
    fn path(&self, p: &str) -> String {
        format_path(p, self.home.as_deref())
    }
    fn split_view(&self) -> bool {
        match self.config.diffs_view() {
            "unified" => false,
            "split" => true,
            _ => self.width > 120,
        }
    }
}

/// `usePathFormatter().format`: relative paths stay; home-prefixed paths become `~/…`.
pub fn format_path(p: &str, cwd: Option<&str>) -> String {
    if let Some(cwd) = cwd
        && let Some(rest) = p.strip_prefix(&format!("{cwd}/"))
    {
        return rest.to_string();
    }
    locale::abbreviate_home(p)
}

fn s(text: impl Into<String>, c: Rgba) -> Seg {
    Seg::new(text, fg(c))
}

fn pad(n: usize) -> Seg {
    Seg::new(" ".repeat(n), Style::default())
}

/// Inline tool row (`InlineToolRow`): icon column (2) at paddingLeft 3.
#[allow(clippy::too_many_arguments)]
fn inline_row(
    ctx: &RCtx,
    icon: &str,
    color: Rgba,
    complete: bool,
    failed: bool,
    denied: bool,
    spinner: bool,
    pending: &str,
    label: Vec<Seg>,
    status: Option<Seg>,
    error: Option<(&str, bool)>,
    click: Option<Click>,
) -> Vec<SLine> {
    let w = ctx.width;
    let err_c = ctx.t().text.feedback.error.base;
    let mut style = fg(if failed { err_c } else { color });
    if denied {
        style = style.add_modifier(Modifier::CROSSED_OUT);
    }
    let mut out = Vec::new();
    let mut head: Vec<Seg>;
    if spinner {
        head = vec![Seg::new(format!("{} ", ctx.spinner()), fg(color))];
        head.extend(label.into_iter().map(|s| Seg::new(s.text, s.style.fg(color.tui()))));
    } else if complete || failed {
        head = vec![Seg::new(format!("{icon:<2}"), style)];
        head.extend(label.into_iter().map(|s| {
            let mut st = s.style.fg(if failed { err_c.tui() } else { color.tui() });
            if denied {
                st = st.add_modifier(Modifier::CROSSED_OUT);
            }
            Seg::new(s.text, st)
        }));
    } else {
        head = vec![Seg::new(format!("{} {pending}", ctx.spinner()), fg(color))];
    }
    if let Some(st) = status {
        head.push(Seg::new(" ", Style::default()));
        head.push(st);
    }
    let lines = wrap(&head, w.saturating_sub(3).max(1), Wrap::Word);
    for (i, l) in lines.into_iter().enumerate() {
        let prefix = if i == 0 { vec![pad(3)] } else { vec![pad(5)] };
        out.push(SLine::new(l).prefixed(&prefix).on_click(click.clone()));
    }
    if let Some((msg, true)) = error
        && failed
    {
        for l in wrap(&[s(msg, err_c)], w.saturating_sub(5).max(1), Wrap::Word) {
            out.push(SLine::new(l).prefixed(&[pad(5)]).on_click(click.clone()));
        }
    }
    out
}

/// `BlockTool`: left bar in `background.base` over a raised panel.
fn block(ctx: &RCtx, header: Vec<Seg>, body: Vec<SLine>, error: Option<(String, Rgba)>, click: Option<Click>) -> Vec<SLine> {
    let t = ctx.t();
    let bg = t.background.raised.base.tui();
    let bar = vec![Seg::new("┃", Style::default().fg(t.background.base.tui()).bg(bg)), pad(2)];
    let inner = ctx.width.saturating_sub(3).max(1);
    let mut out = vec![SLine::new(vec![bar[0].clone()]).with_bg(Some(bg))];
    let mut sections: Vec<Vec<SLine>> = Vec::new();
    if !header.is_empty() {
        sections.push(wrap(&header, inner, Wrap::Word).into_iter().map(SLine::new).collect());
    }
    if !body.is_empty() {
        sections.push(body);
    }
    if let Some((e, c)) = error {
        sections.push(wrap(&[s(e, c)], inner, Wrap::Word).into_iter().map(SLine::new).collect());
    }
    for (i, sec) in sections.into_iter().enumerate() {
        if i > 0 {
            out.push(SLine::new(vec![bar[0].clone()]).with_bg(Some(bg)));
        }
        for l in sec {
            let lbg = l.bg;
            let mut line = l.prefixed(&bar).with_bg(Some(bg));
            if let Some(b) = lbg {
                // Keep inner line backgrounds (diff rows) for their own cells.
                for seg in line.segs.iter_mut().skip(2) {
                    if seg.style.bg.is_none() {
                        seg.style = seg.style.bg(b);
                    }
                }
                let used = line.width();
                if used < ctx.width {
                    line.segs.push(Seg::new(" ".repeat(ctx.width - used), Style::default().bg(b)));
                }
            }
            out.push(line);
        }
    }
    out.push(SLine::new(vec![bar[0].clone()]).with_bg(Some(bg)));
    for l in out.iter_mut() {
        l.click = click.clone();
    }
    out
}

fn path_header(ctx: &RCtx, label: &str, path: &str, color: Rgba, spinner: bool) -> Vec<Seg> {
    let label = if spinner { format!("{} {}", ctx.spinner(), label.trim_start_matches("# ")) } else { label.to_string() };
    let max = ctx.width.saturating_sub(4 + sw(&label) + if spinner { 2 } else { 0 }).max(2);
    vec![s(label, color), s(" ", color), s(locale::truncate_file_path(path, max), color)]
}

fn str_of<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(|x| x.as_str())
}

fn num_of(v: &Value, k: &str) -> Option<f64> {
    v.get(k).and_then(|x| x.as_f64()).filter(|n| n.is_finite())
}

fn primitive_summary(input: &Value) -> String {
    let Some(o) = input.as_object() else { return String::new() };
    let entries: Vec<String> = o
        .iter()
        .filter_map(|(k, v)| match v {
            Value::String(s) => Some(format!("{k}={s}")),
            Value::Number(n) => Some(format!("{k}={n}")),
            Value::Bool(b) => Some(format!("{k}={b}")),
            _ => None,
        })
        .collect();
    if entries.is_empty() { String::new() } else { format!("[{}]", entries.join(", ")) }
}

pub fn tool_output(t: &ToolPart) -> Option<String> {
    if t.state.status == "streaming" || t.state.status == "running" {
        return if t.state.status == "streaming" { None } else { Some(String::new()) };
    }
    Some(
        t.state
            .content
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|c| if c.kind == "text" { c.text.clone().unwrap_or_default() } else { c.name.clone().or(c.uri.clone()).unwrap_or_default() })
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

fn tool_error(t: &ToolPart) -> Option<String> {
    if t.state.status == "error" { t.state.error.as_ref().map(|e| e.message.clone()) } else { None }
}

fn is_denied(e: &str) -> bool {
    e.contains("QuestionRejectedError") || e.contains("rejected permission") || e.contains("specified a rule") || e.contains("user dismissed")
}

/// Render an inline tool with the shared InlineTool color/error rules.
fn inline_tool(ctx: &RCtx, t: &ToolPart, icon: &str, pending: &str, complete: bool, spinner: bool, label: Vec<Seg>, explicit: Option<Rgba>, on_click: Option<Click>, status: Option<Seg>) -> Vec<SLine> {
    let th = ctx.t();
    let error = tool_error(t);
    let denied = error.as_deref().is_some_and(is_denied);
    let failed = error.is_some() && !denied;
    let permission = ctx.permission_tool == Some(t.id.as_str());
    let color = explicit.unwrap_or(if permission {
        th.text.feedback.warning.base
    } else if failed {
        th.text.feedback.error.base
    } else {
        th.text.muted
    });
    let expanded = ctx.is_expanded(&format!("error:{}", t.id), false);
    let click = if failed { Some(Click::ToggleError(t.id.clone())) } else { on_click };
    let label = if failed && !complete { vec![s(error.clone().unwrap_or_default(), color)] } else { label };
    inline_row(ctx, icon, color, complete, failed, denied, spinner, pending, label, status, error.as_deref().map(|e| (e, expanded)), click)
}

fn diagnostics(ctx: &RCtx, meta: &Value, path: &str) -> Vec<SLine> {
    let Some(list) = meta.get("diagnostics").and_then(|d| d.get(path)).and_then(|d| d.as_array()) else { return vec![] };
    list.iter()
        .filter(|d| d.get("severity").and_then(|s| s.as_i64()) == Some(1))
        .filter_map(|d| {
            let line = d.pointer("/range/start/line")?.as_i64()?;
            let ch = d.pointer("/range/start/character")?.as_i64()?;
            let msg = d.get("message")?.as_str()?;
            Some(format!("Error [{}:{}] {msg}", line + 1, ch + 1))
        })
        .take(3)
        .flat_map(|t| wrap(&[s(t, ctx.t().text.feedback.error.base)], ctx.width.saturating_sub(3), Wrap::Word).into_iter().map(SLine::new).collect::<Vec<_>>())
        .collect()
}

#[derive(Clone, Debug)]
struct PatchFile {
    kind: String,
    path: String,
    patch: String,
    deletions: i64,
}

fn patch_files(meta: &Value) -> Vec<PatchFile> {
    let Some(arr) = meta.get("files").and_then(|f| f.as_array()) else { return vec![] };
    arr.iter()
        .filter_map(|f| {
            let status = str_of(f, "status");
            let kind = str_of(f, "type").map(|s| s.to_string()).or_else(|| match status {
                Some("added") => Some("add".into()),
                Some("deleted") => Some("delete".into()),
                Some("modified") => Some("update".into()),
                _ => None,
            })?;
            let path = str_of(f, "file").or(str_of(f, "relativePath"))?.to_string();
            let patch = str_of(f, "patch")?.to_string();
            num_of(f, "additions")?;
            let deletions = num_of(f, "deletions")? as i64;
            Some(PatchFile { kind, path, patch, deletions })
        })
        .collect()
}

fn diff_body(ctx: &RCtx, patch: &str, path: &str) -> Vec<SLine> {
    let o = diff::DiffOpts {
        theme: ctx.t(),
        syntax: ctx.syntax,
        filetype: &filetype(Some(path)),
        split: ctx.split_view(),
        wrap: ctx.config.diffs_wrap() != "none",
    };
    diff::render(&diff::parse(patch), ctx.width.saturating_sub(4).max(10), &o).into_iter().map(|l| l.prefixed(&[pad(1)])).collect()
}

/// Shell output collapsing (`collapse-tool-output.ts`).
pub fn collapse_tool_output(output: &str, max_lines: usize, max_chars: usize) -> (String, bool) {
    let lines: Vec<&str> = output.split('\n').collect();
    if lines.len() <= max_lines && output.chars().count() <= max_chars {
        return (output.to_string(), false);
    }
    let mut visible: Vec<String> = lines.iter().take(max_lines).map(|s| s.to_string()).collect();
    if lines.len() > max_lines
        && let Some(l) = visible.last_mut()
    {
        l.push('…');
    }
    let preview = visible.join("\n");
    if preview.chars().count() > max_chars {
        let cut: String = preview.chars().take(max_chars.saturating_sub(1)).collect();
        return (format!("{cut}…"), true);
    }
    (preview, true)
}

fn collapse_tail(output: &str, max_lines: usize, max_chars: usize) -> String {
    let lines: Vec<&str> = output.split('\n').collect();
    if lines.len() <= max_lines && output.chars().count() <= max_chars {
        return output.to_string();
    }
    let count = (lines.len() as isize - (max_lines as isize - 1).max(0)).max(1);
    let label = format!("({count} earlier {})", if count == 1 { "line" } else { "lines" });
    if max_lines <= 1 {
        return label;
    }
    let preview: Vec<char> = lines[lines.len().saturating_sub(max_lines - 1)..].join("\n").chars().collect();
    let available = max_chars as isize - label.chars().count() as isize - 1;
    if available <= 0 {
        return label;
    }
    let tail: String = preview[preview.len().saturating_sub(available as usize)..].iter().collect();
    format!("{label}\n{tail}")
}

fn collapse_command(input: &str, max_lines: usize, line_w: usize) -> (String, bool, usize) {
    let mut visible: Vec<&str> = Vec::new();
    let mut lines = 1;
    let mut w = 0;
    let mut overflow = false;
    for g in locale::graphemes(input) {
        if g == "\n" {
            if lines >= max_lines {
                overflow = true;
                break;
            }
            visible.push(g);
            lines += 1;
            w = 0;
            continue;
        }
        let n = sw(g);
        if w + n > line_w {
            if lines >= max_lines {
                overflow = true;
                break;
            }
            lines += 1;
            w = 0;
        }
        visible.push(g);
        w += n;
    }
    if !overflow {
        return (input.to_string(), false, lines);
    }
    if w >= line_w
        && let Some(r) = visible.pop()
        && r != "\n"
    {
        let _ = r;
    }
    (format!("{}…", visible.concat()), true, lines)
}

/// `collapseShellOutput`: returns (input, output, overflow).
pub fn collapse_shell(input: &str, output: &str, max_lines: usize, max_chars: usize) -> (String, String, bool) {
    if input.is_empty() {
        let (_, of) = collapse_tool_output(output, max_lines, max_chars);
        return (String::new(), if of { collapse_tail(output, max_lines, max_chars) } else { output.to_string() }, of);
    }
    let cmd_lines = max_lines.min(2);
    let line_chars = (max_chars / max_lines.max(1)).max(1);
    let (cmd, cmd_of, used) = collapse_command(input, cmd_lines, line_chars);
    if output.is_empty() {
        return (cmd, String::new(), cmd_of);
    }
    let lines = max_lines.saturating_sub(used + 1).max(1);
    let chars = max_chars.saturating_sub(cmd.chars().count() + 2).max(1);
    let (_, of) = collapse_tool_output(output, lines, chars);
    (cmd, if of { collapse_tail(output, lines, chars) } else { output.to_string() }, cmd_of || of)
}

#[allow(clippy::too_many_arguments)]
fn shell_display(ctx: &RCtx, key: &str, part: Option<&ToolPart>, shell_id: Option<&str>, command: Option<&str>, workdir: Option<&str>, status: &str, background: bool, output: Option<&str>, error: Option<String>) -> Vec<SLine> {
    let t = ctx.t();
    let permission = part.is_some_and(|p| ctx.permission_tool == Some(p.id.as_str()));
    let color = if permission { t.text.feedback.warning.base } else { t.text.base };
    let bg_running = shell_id.is_some_and(|id| ctx.running_shells.contains(id));
    let running = status == "running" || bg_running;
    let expanded = ctx.is_expanded(&format!("shell:{key}"), false);
    let out_text = if status == "streaming" {
        String::new()
    } else if let Some(id) = shell_id {
        if background && !expanded {
            String::new()
        } else if status == "completed" && output.is_some() {
            locale::strip_ansi(output.unwrap_or("").trim())
        } else {
            locale::strip_ansi(ctx.shell_output.get(id).map(|s| s.as_str()).or(output).unwrap_or("").trim())
        }
    } else {
        locale::strip_ansi(output.unwrap_or("").trim())
    };
    let max_lines = 10;
    let max_chars = max_lines * 20usize.max(ctx.width.saturating_sub(6 + if running { 2 } else { 0 }));
    let wd = workdir.map(|w| ctx.path(w));
    let prefix = match wd.as_deref() {
        Some(w) if !w.is_empty() && w != "." => format!("cd {w} && "),
        _ => String::new(),
    };
    let input = command.map(|c| format!("{}{prefix}{c}", if running { "" } else { "$ " })).unwrap_or_default();
    let (ci, co, overflow) = collapse_shell(&input, &out_text, max_lines, max_chars);
    let (li, lo) = if expanded { (input.clone(), out_text.clone()) } else { (ci, co) };
    let inner = ctx.width.saturating_sub(3).max(1);
    let mut body: Vec<SLine> = Vec::new();
    match command {
        None => {
            let txt = if running || status == "streaming" { format!("{} Writing command…", ctx.spinner()) } else { "Writing command…".into() };
            body.push(SLine::new(vec![s(txt, if running { color } else { t.text.muted })]));
        }
        Some(_) => {
            let mode = if expanded { Wrap::Word } else { Wrap::Char };
            if running {
                let mut lines = wrap(&[s(&li, color)], inner.saturating_sub(2).max(1), mode);
                if !expanded {
                    lines.truncate(2);
                }
                for (i, l) in lines.into_iter().enumerate() {
                    let p = if i == 0 { vec![s(format!("{} ", ctx.spinner()), color)] } else { vec![pad(2)] };
                    body.push(SLine::new(l).prefixed(&p));
                }
            } else {
                let mut lines = wrap(&[s(&li, t.text.base)], inner, mode);
                if !expanded {
                    lines.truncate(2);
                }
                body.extend(lines.into_iter().map(SLine::new));
            }
            if !lo.is_empty() {
                body.push(SLine::blank());
                body.extend(wrap(&[s(&lo, t.text.muted)], inner, Wrap::Word).into_iter().map(SLine::new));
            }
        }
    }
    if background {
        body.push(SLine::blank());
        body.push(SLine::new(vec![Seg::new(" Background ", Style::default().bg(t.dec(t.background.raised.base).tui()).fg(t.text.muted.tui()))]));
    }
    let expandable = shell_id.is_some() || overflow;
    let click = if expandable { Some(Click::ToggleTool(format!("shell:{key}"))) } else { None };
    let err = error.or_else(|| part.and_then(tool_error)).map(|e| (e, t.text.feedback.error.base));
    block(ctx, vec![], body, err, click)
}

fn render_tool(ctx: &RCtx, msg: &Message, t: &ToolPart) -> Vec<SLine> {
    let th = ctx.t();
    let input = if t.state.input.is_object() { t.state.input.clone() } else { Value::Object(Default::default()) };
    let meta = if t.state.status == "streaming" { Value::Null } else { t.state.metadata.clone().unwrap_or(Value::Null) };
    let output = tool_output(t);
    let name = canonical_tool(&t.name);
    let status = t.state.status.as_str();
    let _ = msg;
    match name {
        "shell" => {
            let shell_id = str_of(&meta, "shellID");
            let background = status == "completed" && str_of(&meta, "status") == Some("running");
            shell_display(ctx, &t.id, Some(t), shell_id, str_of(&input, "command"), str_of(&input, "workdir"), status, background, if shell_id.is_some() { None } else { output.as_deref() }, None)
        }
        "glob" | "grep" => {
            let pattern = str_of(&input, "pattern");
            let mut label = format!("{} \"{}\" ", if name == "glob" { "Glob" } else { "Grep" }, pattern.unwrap_or(""));
            if let Some(p) = str_of(&input, "path") {
                label.push_str(&format!("in {} ", ctx.path(p)));
            }
            let count = num_of(&meta, if name == "glob" { "count" } else { "matches" }).filter(|n| *n != 0.0);
            if let Some(n) = count {
                label.push_str(&format!("({} {})", n as i64, if n == 1.0 { "match" } else { "matches" }));
            }
            let pending = if name == "glob" { "Finding files…" } else { "Searching content…" };
            inline_tool(ctx, t, "✱", pending, pattern.is_some(), false, vec![s(label.trim_end(), th.text.muted)], None, None, None)
        }
        "read" => {
            let path = str_of(&input, "path");
            let mut label = format!("Read {}", path.map(|p| ctx.path(p)).unwrap_or_default());
            if input.get("offset").is_some() || input.get("limit").is_some() {
                let off = num_of(&input, "offset").filter(|n| *n != 0.0).unwrap_or(1.0) as i64;
                let end = if input.get("limit").is_some_and(|l| !l.is_null() && l.as_f64() != Some(0.0)) {
                    (off + num_of(&input, "limit").unwrap_or(0.0) as i64 - 1).to_string()
                } else {
                    String::new()
                };
                label.push_str(&format!(":{off}-{end}"));
            }
            let mut out = inline_tool(ctx, t, "→", "Reading file…", path.is_some(), status == "running", vec![s(label, th.text.muted)], None, None, None);
            if status == "completed"
                && let Some(loaded) = meta.get("loaded").and_then(|l| l.as_array())
            {
                for p in loaded.iter().filter_map(|p| p.as_str()) {
                    out.push(SLine::new(vec![pad(6), s(format!("↳ Loaded {}", ctx.path(p)), th.text.muted)]));
                }
            }
            out
        }
        "webfetch" => {
            let url = str_of(&input, "url");
            inline_tool(ctx, t, "%", "Fetching from the web…", url.is_some(), false, vec![s(format!("WebFetch {}", url.unwrap_or("")), th.text.muted)], None, None, None)
        }
        "websearch" => {
            let query = str_of(&input, "query");
            let provider = str_of(&meta, "provider").filter(|p| !p.is_empty());
            let label = match provider {
                Some(p) => {
                    let name = if p == "opencode" { "OpenCode".to_string() } else { titlecase_first(p) };
                    format!("Web Search via {name} \"{}\"", query.unwrap_or(""))
                }
                None => format!("Web Search \"{}\"", query.unwrap_or("")),
            };
            inline_tool(ctx, t, "◈", "Searching web…", query.is_some(), false, vec![s(label, th.text.muted)], None, None, None)
        }
        "write" => {
            let path = str_of(&input, "path");
            if status == "completed" {
                let content = str_of(&input, "content").unwrap_or("");
                let ft = filetype(path);
                let hl = highlight(content, &ft, ctx.syntax);
                let nw = hl.len().to_string().len().max(3);
                let mut body = Vec::new();
                let code_w = ctx.width.saturating_sub(3 + nw + 1).max(1);
                for (i, l) in hl.into_iter().enumerate() {
                    for (k, wl) in wrap(&l, code_w, Wrap::Word).into_iter().enumerate() {
                        let n = if k == 0 { format!("{:>nw$} ", i + 1) } else { " ".repeat(nw + 1) };
                        body.push(SLine::new(wl).prefixed(&[s(n, th.text.muted)]));
                    }
                }
                body.extend(diagnostics(ctx, &meta, path.unwrap_or("")));
                let header = path_header(ctx, "# Wrote", &ctx.path(path.unwrap_or("")), th.text.muted, false);
                block(ctx, header, body, tool_error(t).map(|e| (e, th.text.feedback.error.base)), None)
            } else {
                inline_tool(ctx, t, "←", "Preparing write…", path.is_some(), false, vec![s(format!("Write {}", ctx.path(path.unwrap_or(""))), th.text.muted)], None, None, None)
            }
        }
        "edit" => {
            let files = patch_files(&meta);
            let path = files.first().map(|f| f.path.clone()).or(str_of(&input, "path").map(|s| s.to_string()));
            let permission = ctx.permission_tool == Some(t.id.as_str());
            let hc = if permission { th.text.feedback.warning.base } else { th.text.muted };
            let err = tool_error(t).map(|e| (e, th.text.feedback.error.base));
            if let Some(f) = files.first() {
                let mut body = diff_body(ctx, &f.patch, &f.path);
                body.extend(diagnostics(ctx, &meta, str_of(&input, "path").unwrap_or("")));
                block(ctx, path_header(ctx, "← Edit", &ctx.path(path.as_deref().unwrap_or("")), hc, false), body, err, None)
            } else {
                let spinner = status == "streaming";
                let header = match str_of(&input, "path") {
                    Some(p) => path_header(ctx, "← Edit", &ctx.path(p), hc, spinner),
                    None => vec![s(if spinner { format!("{} Preparing edit…", ctx.spinner()) } else { "# Preparing edit…".into() }, hc)],
                };
                block(ctx, header, vec![], err, None)
            }
        }
        "patch" => {
            let files = patch_files(&meta);
            let err = tool_error(t).map(|e| (e, th.text.feedback.error.base));
            if !files.is_empty() {
                let mut out = Vec::new();
                for (i, f) in files.iter().enumerate() {
                    if i > 0 {
                        out.push(SLine::blank());
                    }
                    let label = match f.kind.as_str() {
                        "add" => "# Created",
                        "delete" => "# Deleted",
                        _ => "← Patched",
                    };
                    let body = if f.kind == "delete" {
                        vec![SLine::new(vec![s(format!("-{} line{}", f.deletions, if f.deletions != 1 { "s" } else { "" }), th.diff.text_removed)])]
                    } else {
                        diff_body(ctx, &f.patch, &f.path)
                    };
                    out.extend(block(ctx, path_header(ctx, label, &ctx.path(&f.path), th.text.muted, false), body, err.clone(), None));
                }
                return out;
            }
            let applied: Vec<(String, String)> = meta
                .get("applied")
                .and_then(|a| a.as_array())
                .map(|a| a.iter().filter_map(|v| Some((str_of(v, "type")?.to_string(), str_of(v, "resource")?.to_string()))).collect())
                .unwrap_or_default();
            if !applied.is_empty() {
                let mut out = Vec::new();
                for (i, (kind, res)) in applied.iter().enumerate() {
                    if i > 0 {
                        out.push(SLine::blank());
                    }
                    let label = match kind.as_str() {
                        "add" => "# Created",
                        "delete" => "# Deleted",
                        _ => "← Patched",
                    };
                    let c = if kind == "delete" { th.diff.text_removed } else { th.text.muted };
                    let body = vec![SLine::new(vec![s(locale::truncate_file_path(res, ctx.width.saturating_sub(3).max(2)), c)])];
                    out.extend(block(ctx, path_header(ctx, label, &ctx.path(res), th.text.muted, false), body, None, None));
                }
                return out;
            }
            let re = regex::Regex::new(r"\*\*\* (?:Add|Update|Delete) File: ([^\r\n]+)").unwrap();
            let targets: Vec<String> = str_of(&input, "patchText").map(|p| re.captures_iter(p).map(|c| c[1].trim().to_string()).collect()).unwrap_or_default();
            let failed = status == "error";
            let label = if failed { "# Patch failed" } else { "Patching" };
            let hc = if failed { th.text.feedback.error.base } else { th.text.muted };
            let spinner = status == "streaming" || status == "running";
            let header = if targets.len() == 1 {
                path_header(ctx, label, &ctx.path(&targets[0]), hc, spinner)
            } else if spinner {
                vec![s(format!("{} {}", ctx.spinner(), label.trim_start_matches("# ")), th.text.muted)]
            } else {
                vec![s(label, hc)]
            };
            block(ctx, header, vec![], tool_error(t).map(|e| (e, if failed { th.text.muted } else { th.text.feedback.error.base })), None)
        }
        "subagent" => {
            let session = str_of(&meta, "sessionID").or(str_of(&meta, "sessionId")).map(|s| s.to_string());
            let description = str_of(&input, "description");
            let continuation = str_of(&input, "sessionID").is_some();
            let running = status == "running" || session.as_ref().is_some_and(|id| ctx.running_sessions.contains(id));
            let model = str_of(&input, "model").map(|m| subagent_model_label(m, ctx.models));
            let agent = titlecase(str_of(&input, "agent").or(str_of(&input, "subagent_type")).unwrap_or("General"));
            let head = if continuation { "Continue subagent".to_string() } else { format!("{agent} Subagent") };
            let label = format!("{head} — {}{}", description.unwrap_or("Subagent"), model.map(|m| format!(" · {m}")).unwrap_or_default());
            let icon = if continuation { "↳" } else if running { "│" } else if status == "completed" { "✓" } else { "│" };
            let badge = if status == "completed" && str_of(&meta, "status") == Some("running") {
                Some(Seg::new(" Background ", Style::default().bg(th.dec(th.background.base).tui()).fg(th.text.muted.tui())))
            } else {
                None
            };
            let click = session.map(Click::Navigate);
            inline_tool(ctx, t, icon, "Delegating…", description.is_some(), !continuation && running, vec![s(label, th.text.muted)], None, click, badge)
        }
        "execute" => {
            let loading = status == "streaming" || status == "running";
            let runtime_error = meta.get("error") == Some(&Value::Bool(true)) || status == "error";
            let icon = if runtime_error { "✗" } else if status == "completed" { "✓" } else { "│" };
            let color = if runtime_error { Some(th.text.feedback.error.base) } else { None };
            let click = Some(Click::Execute(msg.id.clone(), t.id.clone()));
            let mut out = inline_tool(ctx, t, icon, "execute", true, loading, vec![s("execute", th.text.muted)], color, click.clone(), None);
            if let Some(calls) = meta.get("toolCalls").and_then(|c| c.as_array()) {
                for call in calls {
                    let Some(tool) = str_of(call, "tool") else { continue };
                    let st = str_of(call, "status").unwrap_or("");
                    if !matches!(st, "running" | "completed" | "error") {
                        continue;
                    }
                    let c = if st == "error" { th.text.feedback.error.base } else { th.text.muted };
                    let args = primitive_summary(call.get("input").unwrap_or(&Value::Null)).split_whitespace().collect::<Vec<_>>().join(" ");
                    let text = if args.is_empty() { tool.to_string() } else { format!("{tool} {args}") };
                    let line = vec![pad(3), s(format!("{:<2}", if st == "error" { "✗" } else { "›" }), c), s(text, c)];
                    out.push(SLine::new(clip(&line, ctx.width)));
                }
            }
            if runtime_error {
                let o = locale::strip_ansi(output.as_deref().unwrap_or("").trim());
                if !o.is_empty() {
                    let (preview, _) = collapse_tool_output(&o, 4, 4 * 20usize.max(ctx.width.saturating_sub(6)));
                    for (i, l) in preview.split('\n').enumerate() {
                        out.push(SLine::new(vec![pad(6), s(format!("{}{l}", if i == 0 { "↳ " } else { "  " }), th.text.feedback.error.base)]));
                    }
                }
            }
            out
        }
        "question" => {
            let questions: Vec<String> = input.get("questions").and_then(|q| q.as_array()).map(|a| a.iter().filter_map(|q| str_of(q, "question").map(|s| s.to_string())).collect()).unwrap_or_default();
            let answers: Option<Vec<Vec<String>>> = meta.get("answers").and_then(|a| a.as_array()).map(|a| {
                a.iter().map(|x| x.as_array().map(|v| v.iter().filter_map(|s| s.as_str().map(|s| s.to_string())).collect()).unwrap_or_default()).collect()
            });
            if let Some(ans) = answers {
                let mut body = Vec::new();
                let inner = ctx.width.saturating_sub(3).max(1);
                for (i, q) in questions.iter().enumerate() {
                    if i > 0 {
                        body.push(SLine::blank());
                    }
                    body.extend(wrap(&[s(q, th.text.muted)], inner, Wrap::Word).into_iter().map(SLine::new));
                    let a = ans.get(i).filter(|a| !a.is_empty()).map(|a| a.join(", ")).unwrap_or_else(|| "(no answer)".into());
                    body.extend(wrap(&[s(a, th.text.base)], inner, Wrap::Word).into_iter().map(SLine::new));
                }
                block(ctx, vec![s("# Questions", th.text.muted)], body, tool_error(t).map(|e| (e, th.text.feedback.error.base)), None)
            } else {
                let n = questions.len();
                inline_tool(ctx, t, "→", "Asking questions…", n > 0, false, vec![s(format!("Asked {n} question{}", if n != 1 { "s" } else { "" }), th.text.muted)], None, None, None)
            }
        }
        "skill" => {
            let name = str_of(&meta, "name").or(str_of(&input, "id"));
            inline_tool(ctx, t, "→", "Loading skill…", name.is_some(), false, vec![s(format!("Skill \"{}\"", name.unwrap_or("")), th.text.muted)], None, None, None)
        }
        _ => {
            let loading = status == "streaming" || status == "running";
            let out_s = output.clone().unwrap_or_default().trim().to_string();
            let entries: Vec<(String, Value)> = input.as_object().map(|o| o.iter().map(|(k, v)| (k.clone(), v.clone())).collect()).unwrap_or_default();
            let expandable = !entries.is_empty() || !out_s.is_empty();
            let key = format!("tool:{}", t.id);
            let expanded = ctx.is_expanded(&key, false);
            let args = primitive_summary(&input).split_whitespace().collect::<Vec<_>>().join(" ");
            let label = if args.is_empty() { t.name.clone() } else { format!("{} {args}", t.name) };
            let click = if expandable { Some(Click::ToggleTool(key)) } else { None };
            let mut out = inline_tool(ctx, t, if status == "error" { "✗" } else { "✓" }, &t.name, status == "completed", loading, vec![s(label, th.text.muted)], None, click, None);
            if expanded {
                let inner = ctx.width.saturating_sub(5).max(1);
                let mut kv = entries.clone();
                if !out_s.is_empty() {
                    kv.push(("output".into(), Value::String(out_s.clone())));
                }
                for (k, v) in kv {
                    let val = match &v {
                        Value::String(s) => s.clone(),
                        other => serde_json::to_string_pretty(other).unwrap_or_default(),
                    };
                    let key = format!("{k}: ");
                    let kw = sw(&key);
                    for (i, l) in wrap(&[s(val, th.text.base)], inner.saturating_sub(kw).max(1), Wrap::Word).into_iter().enumerate() {
                        let p = if i == 0 { vec![pad(5), s(&key, th.text.muted)] } else { vec![pad(5 + kw)] };
                        out.push(SLine::new(l).prefixed(&p));
                    }
                }
            }
            out
        }
    }
}

fn titlecase_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

pub fn subagent_model_label(value: &str, models: &[ModelInfo]) -> String {
    let (reference, variant) = match value.split_once('#') {
        Some((r, v)) => (r, Some(v)),
        None => (value, None),
    };
    let (provider, model) = match reference.split_once('/') {
        Some((p, m)) => (p, m),
        None => ("", reference),
    };
    let name = models.iter().find(|m| m.provider_id == provider && m.id == model).map(|m| m.name.clone());
    format!("{}{}", name.unwrap_or_else(|| reference.to_string()), variant.map(|v| format!(" ({v})")).unwrap_or_default())
}

/// `switchLabel` (`src/util/model.ts`).
pub fn switch_label(model: &ModelRef, models: &[ModelInfo], previous: Option<&ModelRef>) -> String {
    if let Some(p) = previous
        && p.provider_id == model.provider_id
        && p.id == model.id
    {
        return format!("Switched variant to {}", model.variant.as_deref().unwrap_or("default"));
    }
    let display = models.iter().find(|m| m.provider_id == model.provider_id && m.id == model.id).map(|m| m.name.clone());
    match display {
        None => {
            let parts: Vec<&str> = [Some(model.provider_id.as_str()), Some(model.id.as_str()), model.variant.as_deref()].into_iter().flatten().collect();
            format!("Switched model to {}", parts.join("/"))
        }
        Some(d) => {
            let v = model.variant.as_deref().filter(|v| *v != "default").map(|v| format!(" ({v})")).unwrap_or_default();
            format!("Switched model to {d}{v}")
        }
    }
}

fn md_lines(ctx: &RCtx, text: &str, width: usize, mono: bool) -> Vec<SLine> {
    let o = MdOpts {
        theme: ctx.t(),
        syntax: if mono { ctx.thinking_syntax } else { ctx.syntax },
        conceal: ctx.conceal(),
        fg: if mono { ctx.t().text.muted } else { ctx.t().markdown.text },
        mono,
    };
    markdown::render(text, width, &o)
}

pub fn resolve_part<'a>(m: &'a Message, pid: &str) -> Option<&'a Content> {
    if let Some(c) = m.content.iter().find(|c| matches!(c, Content::Tool(t) if t.id == pid)) {
        return Some(c);
    }
    let (kind, n) = pid.split_once(':')?;
    let n: usize = n.parse().ok()?;
    m.content
        .iter()
        .filter(|c| match c {
            Content::Text { .. } => kind == "text",
            Content::Reasoning { .. } => kind == "reasoning",
            _ => false,
        })
        .nth(n)
}

fn warn60(ctx: &RCtx) -> Rgba {
    tint(ctx.t().background.base, ctx.t().text.feedback.warning.base, 0.6)
}

fn render_part(ctx: &RCtx, r: &PartRef) -> Vec<SLine> {
    let Some(m) = ctx.message(&r.message) else { return vec![] };
    let Some(part) = resolve_part(m, &r.part) else { return vec![] };
    match part {
        Content::Text { text, .. } => {
            let t = text.trim();
            if t.is_empty() {
                return vec![];
            }
            md_lines(ctx, t, ctx.width.saturating_sub(3), false).into_iter().map(|l| l.prefixed(&[pad(3)])).collect()
        }
        Content::Reasoning { text, time, .. } => {
            // thinking = "show": header + body with left bars.
            let content = reasoning_content(text);
            if content.is_empty() {
                return vec![];
            }
            let th = ctx.t();
            let done = time.as_ref().and_then(|t| t.completed).is_some() || m.time.completed.is_some();
            let bar_c = th.dec(th.background.base);
            let bar = vec![pad(3), s("┃", bar_c), pad(1)];
            let start = time.as_ref().map(|t| t.created).unwrap_or(m.time.created);
            let end = time.as_ref().and_then(|t| t.completed).or(m.time.completed);
            let header = if !done {
                vec![s(format!("{} Thinking", ctx.spinner()), warn60(ctx))]
            } else {
                let d = end.map(|e| locale::duration((e - start).max(0))).unwrap_or_default();
                vec![s(format!("Thought{}", if d.is_empty() { String::new() } else { format!(": {d}") }), warn60(ctx))]
            };
            let mut out = vec![SLine::new(header).prefixed(&bar), SLine::blank()];
            for l in md_lines(ctx, &content, ctx.width.saturating_sub(5), true) {
                out.push(l.prefixed(&bar));
            }
            out
        }
        Content::Tool(t) => render_tool(ctx, m, t),
    }
}

/// Index of the input message of the turn that contains `idx` (scanning back to the last `idle`).
pub fn turn_bounds(messages: &[Message], idx: usize, legacy: bool) -> Option<usize> {
    let mut input: Option<usize> = None;
    for i in (0..idx).rev() {
        let m = &messages[i];
        if m.kind == "idle" {
            return input;
        }
        if m.kind != "user" && m.kind != "synthetic" {
            continue;
        }
        if legacy {
            return Some(i);
        }
        input = Some(i);
    }
    input
}

pub fn turn_duration(m: &Message, messages: &[Message]) -> i64 {
    let idx = messages.iter().position(|x| x.id == m.id);
    turn_duration_at(m, messages, idx, is_legacy(messages))
}

fn turn_duration_at(m: &Message, messages: &[Message], idx: Option<usize>, legacy: bool) -> i64 {
    let Some(done) = m.time.completed else { return 0 };
    let idx = idx.unwrap_or(messages.len());
    let start = turn_bounds(messages, idx, legacy).map(|i| messages[i].time.created).unwrap_or(m.time.created);
    (done - start).max(0)
}

pub fn turn_tps(m: &Message, messages: &[Message]) -> Option<f64> {
    let idx = messages.iter().position(|x| x.id == m.id)?;
    turn_tps_at(messages, idx, is_legacy(messages))
}

fn turn_tps_at(messages: &[Message], idx: usize, legacy: bool) -> Option<f64> {
    let end = idx + 1;
    let start = turn_bounds(messages, end, legacy).map(|i| i as isize).unwrap_or(-1);
    let steps: Vec<&Message> = messages[((start + 1) as usize)..end].iter().filter(|x| x.kind == "assistant").collect();
    let durations: Vec<i64> = steps.iter().filter_map(|s| s.time.streamed.map(|st| (st - s.time.created).max(0))).collect();
    if steps.is_empty() || durations.len() != steps.len() {
        return None;
    }
    let out: f64 = steps.iter().map(|s| s.tokens.as_ref().map(|t| t.output + t.reasoning).unwrap_or(0.0)).sum();
    let d: i64 = durations.iter().sum();
    if out <= 0.0 || d <= 0 {
        return None;
    }
    Some(out / (d as f64 / 1000.0))
}

/// Seconds left on a scheduled retry, as the footer shows them (`Retrying in Ns`).
pub fn retry_secs(at: i64, now: i64) -> i64 {
    ((at - now) as f64 / 1000.0).ceil().max(0.0) as i64
}

fn render_footer(ctx: &RCtx, id: &str) -> Vec<SLine> {
    let Some(m) = ctx.message(id) else { return vec![] };
    let th = ctx.t();
    let mut out = Vec::new();
    let interrupted = m.error.as_ref().is_some_and(|e| e.message == "Step interrupted");
    if let Some(e) = &m.error
        && !interrupted
        && m.retry.is_none()
    {
        for l in wrap(&[s(format!("Error: {}", e.message), th.text.feedback.error.base)], ctx.width.saturating_sub(3), Wrap::Word) {
            out.push(SLine::new(l).prefixed(&[pad(3)]));
        }
    }
    if let Some(r) = &m.retry {
        let secs = retry_secs(r.at, ctx.now);
        let head = if secs > 0 { format!("Retrying in {secs}s") } else { "Retry due".into() };
        let text = format!("⚠ {head} · attempt {} · {}", r.attempt, r.error.message);
        for l in wrap(&[s(text, th.text.feedback.warning.base)], ctx.width.saturating_sub(3), Wrap::Word) {
            out.push(SLine::new(l).prefixed(&[pad(3)]));
        }
    }
    if m.retry.is_some() || (m.error.is_some() && !interrupted) {
        out.push(SLine::blank());
    }
    let agent = m.agent.clone().unwrap_or_default();
    let ac = if m.error.is_some() { th.text.muted } else { ctx.agent_color(&agent) };
    let mut segs = vec![s(titlecase(&agent), ac)];
    if ctx.term_width >= 28 {
        let model = m.model.as_ref().map(|mr| {
            ctx.models.iter().find(|x| x.provider_id == mr.provider_id && x.id == mr.id).map(|x| x.name.clone()).unwrap_or_else(|| format!("{}/{}", mr.provider_id, mr.id))
        });
        segs.push(s(format!(" · {}", model.unwrap_or_default()), th.text.muted));
    }
    let idx = ctx.position(id);
    let legacy = ctx.legacy();
    let d = turn_duration_at(m, ctx.messages, idx, legacy);
    if d > 0 && (ctx.term_width < 28 || ctx.term_width >= 36) {
        segs.push(s(format!(" · {}", locale::duration(d)), th.text.muted));
    }
    if ctx.config.tps()
        && let Some(v) = idx.and_then(|i| turn_tps_at(ctx.messages, i, legacy))
    {
        segs.push(s(format!(" · {v:.1} tok/s"), th.text.muted));
    }
    if interrupted {
        segs.push(s(" · interrupted", th.text.muted));
    }
    for l in wrap(&segs, ctx.width.saturating_sub(3), Wrap::Word) {
        out.push(SLine::new(l).prefixed(&[pad(3)]));
    }
    out
}

fn chip(label: &str, name: &str, ctx: &RCtx, light: bool) -> Vec<Seg> {
    let th = ctx.t();
    let lbg = th.accent(if light { 300 } else { 200 });
    vec![
        Seg::new(format!(" {label} "), Style::default().bg(lbg.tui()).fg(th.background.raised.base.tui()).add_modifier(Modifier::BOLD)),
        Seg::new(format!(" {name} "), Style::default().bg(th.dec(th.background.raised.base).tui()).fg(th.text.muted.tui())),
    ]
}

fn rule_line(ctx: &RCtx, center: Vec<Seg>, color: Rgba) -> SLine {
    let cw: usize = center.iter().map(|s| s.width()).sum::<usize>() + 2;
    let total = ctx.width;
    let left = total.saturating_sub(cw) / 2;
    let right = total.saturating_sub(cw + left);
    let mut segs = vec![s("─".repeat(left), color), pad(1)];
    segs.extend(center);
    segs.push(pad(1));
    segs.push(s("─".repeat(right), color));
    SLine::new(segs)
}

fn render_message(ctx: &RCtx, id: &str, light: bool) -> Vec<SLine> {
    let Some(m) = ctx.message(id) else { return vec![] };
    let th = ctx.t();
    let w = ctx.width;
    match m.kind.as_str() {
        "user" => {
            let files = m.files.clone().unwrap_or_default();
            let skills = m.skills.clone().unwrap_or_default();
            if m.text_str().trim().is_empty() && files.is_empty() && skills.is_empty() {
                return vec![];
            }
            let delivery = ctx.pending_inputs.get(id);
            let bar_c = if delivery.is_some() { th.border } else { ctx.agent_color(ctx.session_agent.unwrap_or("build")) };
            let bg = th.background.raised.base.tui();
            let bar = vec![Seg::new("┃", Style::default().fg(bar_c.tui()).bg(bg)), pad(2)];
            let click = if delivery.map(|d| d.as_str()) == Some("steer") { Click::PendingSteer(id.into()) } else { Click::OpenMessage(id.into()) };
            let mut out = vec![SLine::new(vec![bar[0].clone()])];
            for l in wrap(&[s(m.text_str(), th.text.base)], w.saturating_sub(3).max(1), Wrap::Word) {
                out.push(SLine::new(l).prefixed(&bar));
            }
            let mut chips: Vec<Vec<Seg>> = Vec::new();
            for sk in &skills {
                chips.push(chip("skill", sk.name.as_deref().unwrap_or(&sk.id), ctx, light));
            }
            for f in &files {
                let label = if f.mime.as_deref() == Some("application/x-directory") { "dir" } else { "file" };
                let name = f.name.clone().or_else(|| f.source_uri()).unwrap_or_else(|| "attachment".into());
                chips.push(chip(label, &name, ctx, light));
            }
            if !chips.is_empty() {
                out.push(SLine::new(vec![bar[0].clone()]));
                // flex-wrap chips with gap 1
                let mut line: Vec<Seg> = Vec::new();
                let mut used = 0;
                let avail = w.saturating_sub(3);
                for c in chips {
                    let cw: usize = c.iter().map(|x| x.width()).sum();
                    if used > 0 && used + 1 + cw > avail {
                        out.push(SLine::new(std::mem::take(&mut line)).prefixed(&bar));
                        used = 0;
                    }
                    if used > 0 {
                        line.push(pad(1));
                        used += 1;
                    }
                    used += cw;
                    line.extend(c);
                }
                if !line.is_empty() {
                    out.push(SLine::new(line).prefixed(&bar));
                }
            }
            out.push(SLine::new(vec![bar[0].clone()]));
            out.into_iter().map(|l| l.with_bg(Some(bg)).on_click(Some(click.clone()))).collect()
        }
        "shell" => {
            let error = match m.status.as_deref() {
                Some("killed") => Some("Command cancelled".to_string()),
                Some("timeout") => Some("Command timed out".to_string()),
                _ => m.exit.filter(|e| *e != 0).map(|e| format!("Command exited with code {e}")),
            };
            let status = if m.status.as_deref() == Some("running") { "running" } else { "completed" };
            let output = m.output.as_ref().map(|o| o.output.clone());
            shell_display(ctx, id, None, m.shell_id.as_deref(), m.command.as_deref(), None, status, false, output.as_deref(), error)
        }
        "location-switched" => {
            let dir = m.location.as_ref().map(|l| l.directory.clone()).unwrap_or_default();
            vec![SLine::new(clip(&[pad(3), s("↳ Moved to ", th.text.muted), s(dir, th.text.feedback.info.base)], w))]
        }
        "agent-switched" => {
            let agent = titlecase(m.agent.as_deref().unwrap_or(""));
            let prev = m.previous.as_ref().and_then(|p| p.as_str());
            let text = match prev {
                Some(p) if Some(p) != m.agent.as_deref() => format!("Switched agent from {} to {agent}", titlecase(p)),
                _ => format!("Switched agent to {agent}"),
            };
            wrap(&[s(text, th.text.muted)], w.saturating_sub(3), Wrap::Word).into_iter().map(|l| SLine::new(l).prefixed(&[pad(3)])).collect()
        }
        "model-switched" => {
            let prev: Option<ModelRef> = m.previous.as_ref().and_then(|p| serde_json::from_value(p.clone()).ok());
            let text = m.model.as_ref().map(|mr| switch_label(mr, ctx.models, prev.as_ref())).unwrap_or_default();
            wrap(&[s(text, th.text.muted)], w.saturating_sub(3), Wrap::Word).into_iter().map(|l| SLine::new(l).prefixed(&[pad(3)])).collect()
        }
        "system" | "synthetic" => {
            let meta = if m.kind == "synthetic" { m.metadata.clone() } else { None };
            let source = meta.as_ref().and_then(|md| str_of(md, "source")).map(|s| s.to_string());
            let text = if m.kind == "system" { m.description.clone().unwrap_or_else(|| "Instructions updated".into()) } else { m.description.clone().unwrap_or_default() };
            if matches!(source.as_deref(), Some("subagent") | Some("shell")) {
                let md = meta.unwrap_or(Value::Null);
                let state = str_of(&md, "state").map(|s| s.to_string());
                let actor = if source.as_deref() == Some("shell") { "Shell".to_string() } else { titlecase(str_of(&md, "agent").unwrap_or("Subagent")) };
                let status = match state.as_deref() {
                    Some("completed") => "finished".to_string(),
                    Some("error") => "failed".to_string(),
                    Some(o) => o.to_string(),
                    None => "finished".to_string(),
                };
                let heading = format!("{} {actor} {status}", if state.as_deref() == Some("completed") { "↳" } else { "!" });
                let desc = if source.as_deref() == Some("shell") { locale::collapse_ws(&text) } else { text };
                let suffix = locale::truncate_width(&format!(" · {desc}"), w.saturating_sub(3 + sw(&heading)));
                let color = match state.as_deref() {
                    Some("error") => th.text.feedback.error.base,
                    Some("cancelled") => th.text.feedback.warning.base,
                    _ => th.text.feedback.info.base,
                };
                let child = if source.as_deref() == Some("subagent") { str_of(&md, "childID").map(|s| Click::Navigate(s.to_string())) } else { None };
                return vec![SLine::new(clip(&[pad(3), s(heading, color), s(suffix, th.text.muted)], w)).on_click(child)];
            }
            inline_row(ctx, "◈", th.text.muted, true, false, false, false, "Notice", vec![s(text, th.text.muted)], None, None, None)
        }
        "skill" => inline_row(ctx, "→", th.text.muted, true, false, false, false, "Skill", vec![s(format!("Skill {}", m.name.as_deref().unwrap_or("")), th.text.muted)], None, None, None),
        "compaction" => {
            let status = m.status.as_deref().unwrap_or("running");
            let cancelled = status == "failed" && m.error.as_ref().is_some_and(|e| e.kind == "aborted");
            let color = if status == "failed" && !cancelled { th.text.feedback.error.base } else { th.text.muted };
            let mut center = Vec::new();
            if status == "running" {
                center.push(s(format!("{} ", ctx.spinner()), color));
            } else if status == "failed" && !cancelled {
                center.push(s("✗ ", color));
            }
            center.push(s(if status == "completed" && m.provider_context.is_some() { "Provider compaction" } else { "Compaction" }, color));
            if cancelled {
                center.push(s(" · cancelled", color));
            }
            if status != "running"
                && let Some(t) = &m.tokens
            {
                let input = t.input + t.cache.read + t.cache.write;
                let output = t.output + t.reasoning;
                if input + output > 0.0 {
                    center.push(s(format!(" · {} in · {} out", locale::number(input), locale::number(output)), color));
                }
            }
            let mut out = vec![rule_line(ctx, center, color)];
            let content = if status == "failed" { if cancelled { String::new() } else { m.error.as_ref().map(|e| e.message.clone()).unwrap_or_default() } } else { m.summary.clone().unwrap_or_default() };
            let content = content.trim();
            if !content.is_empty() {
                out.push(SLine::blank());
                out.extend(md_lines(ctx, content, w.saturating_sub(3), false).into_iter().map(|l| l.prefixed(&[pad(3)])));
            }
            out
        }
        "idle" => match &m.error {
            Some(e) => wrap(&[s(format!("Error: {}", e.message), th.text.feedback.error.base)], w.saturating_sub(3), Wrap::Word).into_iter().map(|l| SLine::new(l).prefixed(&[pad(3)])).collect(),
            None => vec![],
        },
        _ => vec![],
    }
}

fn group_lines(ctx: &RCtx, kind: GroupKind, children: &[Node], completed: bool, pending: &[PartRef], level: usize, light: bool) -> Vec<SLine> {
    let th = ctx.t();
    let id = group_id(kind, children, level);
    let verbosity = ctx.config.verbosity();
    let default_open = verbosity == "high" && matches!(kind, GroupKind::Exploration | GroupKind::Instructions);
    let expanded = ctx.is_expanded(&id, default_open);
    let mut entries = Vec::new();
    collect_entries(children, &mut entries);
    let is_pending = |e: &Entry| matches!(e, Entry::Part(r) if pending.iter().any(|p| p == r));
    let click = Some(Click::ToggleGroup(id.clone()));
    let mut out = Vec::new();
    let render_children = |mode: &str| -> Vec<SLine> {
        let mut v = Vec::new();
        for n in children {
            match n {
                Node::Group { kind: k, children: c, .. } => {
                    v.extend(group_lines(ctx, *k, c, true, &[], level + 1, light));
                }
                Node::Entry(e) => {
                    if is_pending(e) {
                        continue;
                    }
                    if mode == "thought" {
                        if let Entry::Part(r) = e
                            && let Some(m) = ctx.message(&r.message)
                            && let Some(Content::Reasoning { text, .. }) = resolve_part(m, &r.part)
                        {
                            let content = reasoning_content(text);
                            if content.is_empty() {
                                continue;
                            }
                            v.push(SLine::blank());
                            let bar = vec![pad(3), s("┃", th.dec(th.background.raised.base)), pad(1)];
                            for l in md_lines(ctx, &content, ctx.width.saturating_sub(5), true) {
                                v.push(l.prefixed(&bar));
                            }
                        }
                    } else {
                        v.extend(render_entry(ctx, e, light));
                    }
                }
            }
        }
        v
    };
    match kind {
        GroupKind::Reasoning => {
            let thoughts: Vec<(&Message, &Content)> = entries
                .iter()
                .filter(|e| !is_pending(e))
                .filter_map(|e| match e {
                    Entry::Part(r) => {
                        let m = ctx.message(&r.message)?;
                        let p = resolve_part(m, &r.part)?;
                        match p {
                            Content::Reasoning { text, .. } if !reasoning_content(text).is_empty() => Some((m, p)),
                            _ => None,
                        }
                    }
                    _ => None,
                })
                .collect();
            if thoughts.is_empty() {
                return out;
            }
            if ctx.config.thinking() != "hide" {
                // Not grouped: each part renders individually, separated by row gaps.
                let mut first = true;
                for e in &entries {
                    let lines = render_entry(ctx, e, light);
                    if lines.is_empty() {
                        continue;
                    }
                    if !first {
                        out.push(SLine::blank());
                    }
                    first = false;
                    out.extend(lines);
                }
                return out;
            }
            // Latest title.
            let mut latest: Option<String> = None;
            for (m, p) in &thoughts {
                if let Content::Reasoning { text, time, .. } = p {
                    match reasoning_title(&reasoning_content(text)) {
                        Some(t) => latest = Some(t),
                        None => {
                            if time.as_ref().and_then(|t| t.completed).is_some() || m.time.completed.is_some() {
                                latest = None;
                            }
                        }
                    }
                }
            }
            let duration: i64 = thoughts
                .iter()
                .map(|(_, p)| match p {
                    Content::Reasoning { time: Some(t), .. } => t.completed.map(|c| (c - t.created).max(0)).unwrap_or(0),
                    _ => 0,
                })
                .sum();
            let color = if !completed {
                th.text.base
            } else if expanded {
                th.text.feedback.warning.base
            } else {
                warn60(ctx)
            };
            let pending_label = latest.as_ref().map(|l| format!("Thinking: {l}")).unwrap_or_else(|| "Thinking".into());
            let mut label = if completed { "Thought".to_string() } else { pending_label.clone() };
            if completed && !expanded
                && let Some(l) = &latest
            {
                label.push_str(&format!(": {l}"));
            }
            if completed && thoughts.len() > 1 {
                label.push_str(&format!(" · {} steps", thoughts.len()));
            }
            if completed && duration > 0 {
                label.push_str(&format!(" · {}", locale::duration(duration)));
            }
            out.extend(inline_row(ctx, if expanded { "-" } else { "+" }, color, completed, false, false, !completed, &pending_label, vec![s(label, color)], None, None, click));
            if expanded {
                out.extend(render_children("thought"));
            }
        }
        GroupKind::Exploration => {
            let tools: Vec<&ToolPart> = entries
                .iter()
                .filter(|e| !is_pending(e))
                .filter_map(|e| match e {
                    Entry::Part(r) => match resolve_part(ctx.message(&r.message)?, &r.part)? {
                        Content::Tool(t) => Some(t),
                        _ => None,
                    },
                    _ => None,
                })
                .collect();
            if ctx.config.grouping() == "none" {
                let mut first = true;
                for e in &entries {
                    let lines = render_entry(ctx, e, light);
                    if lines.is_empty() {
                        continue;
                    }
                    if !first {
                        out.push(SLine::blank());
                    }
                    first = false;
                    out.extend(lines);
                }
                return out;
            }
            let done = completed || (!tools.is_empty() && tools.iter().all(|t| t.time.completed.is_some()));
            if !tools.is_empty() {
                let mut counts: Vec<(String, usize)> = Vec::new();
                for t in &tools {
                    let n = canonical_tool(&t.name);
                    let name = match n {
                        "grep" | "glob" | "websearch" => "search",
                        "webfetch" => "fetch",
                        o => o,
                    }
                    .to_string();
                    match counts.iter_mut().find(|(k, _)| *k == name) {
                        Some((_, c)) => *c += 1,
                        None => counts.push((name, 1)),
                    }
                }
                let names: Vec<String> = counts
                    .iter()
                    .map(|(n, c)| {
                        let plural = if *c == 1 { n.clone() } else if n == "search" || n == "fetch" { format!("{n}es") } else { format!("{n}s") };
                        format!("{c} {plural}")
                    })
                    .collect();
                let label = format!("{}: {}", if done { "Explored" } else { "Exploring" }, names.join(", "));
                out.extend(inline_row(ctx, if done { "→" } else { "✱" }, th.text.muted, done, false, false, !done, &label, vec![s(label.clone(), th.text.muted)], None, None, click));
                if expanded {
                    out.extend(render_children("tool"));
                }
            }
        }
        GroupKind::Instructions => {
            let mut files: HashSet<String> = HashSet::new();
            for e in &entries {
                if let Entry::Message(id) = e
                    && let Some(m) = ctx.message(id)
                {
                    files.extend(instruction_paths(m));
                }
            }
            let n = files.len();
            out.extend(inline_row(ctx, "◈", th.text.muted, true, false, false, false, "", vec![s(format!("Instructions: {n} {}", if n == 1 { "file" } else { "files" }), th.text.muted)], None, None, click));
            if expanded {
                out.extend(render_children("normal"));
            }
        }
        GroupKind::Activity => {
            let (label, active) = activity_summary(ctx, &entries, pending, completed);
            if !label.is_empty() {
                out.extend(inline_row(ctx, if expanded { "−" } else { "+" }, th.text.muted, true, false, false, !expanded && active, &label, vec![s(label.clone(), th.text.muted)], None, None, click));
                if expanded {
                    let mut first = true;
                    for n in children {
                        let lines = match n {
                            Node::Group { kind: k, children: c, .. } => group_lines(ctx, *k, c, true, &[], level + 1, light),
                            Node::Entry(e) if !is_pending(e) => render_entry(ctx, e, light),
                            _ => vec![],
                        };
                        if lines.is_empty() {
                            continue;
                        }
                        out.push(SLine::blank());
                        let _ = first;
                        first = false;
                        out.extend(lines);
                    }
                }
            }
        }
    }
    // Permission-blocked tools stay visible below the collapsed header.
    if level == 0 {
        for r in pending {
            if entries.iter().any(|e| matches!(e, Entry::Part(x) if x == r)) {
                out.extend(render_part(ctx, r));
            }
        }
    }
    out
}

pub fn reasoning_title(text: &str) -> Option<String> {
    let re = regex::Regex::new(r"^\*\*([^*\n]+)\*\*(?:\r?\n\r?\n|$)").unwrap();
    re.captures(text.trim()).map(|c| c[1].trim().to_string())
}

fn activity_summary(ctx: &RCtx, entries: &[&Entry], pending: &[PartRef], closed: bool) -> (String, bool) {
    let mut counts = [0usize; 6]; // command, edit, thought, read, tool, instruction
    let mut files: HashSet<String> = HashSet::new();
    let mut active_label: Option<String> = None;
    let mut active = false;
    for e in entries {
        match e {
            Entry::Message(id) => {
                if let Some(m) = ctx.message(id) {
                    files.extend(instruction_paths(m));
                }
            }
            Entry::Part(r) => {
                if pending.iter().any(|p| p == r) {
                    continue;
                }
                let Some(m) = ctx.message(&r.message) else { continue };
                let Some(p) = resolve_part(m, &r.part) else { continue };
                match p {
                    Content::Reasoning { text, time, .. } => {
                        let is_active = !closed && time.as_ref().and_then(|t| t.completed).is_none() && m.time.completed.is_none();
                        if is_active {
                            active = true;
                            active_label.get_or_insert("Thinking…".into());
                        } else if !reasoning_content(text).is_empty() {
                            counts[2] += 1;
                        }
                    }
                    Content::Tool(t) => {
                        let st = t.state.status.as_str();
                        let is_active = st == "streaming" || st == "running";
                        let name = canonical_tool(&t.name);
                        if is_active {
                            active = true;
                            let noun = match name {
                                "shell" => "command",
                                "execute" => "code",
                                o => o,
                            };
                            active_label.get_or_insert(format!("{} {noun}…", if st == "streaming" { "Preparing" } else { "Running" }));
                        }
                        if name == "execute" {
                            let calls: Vec<&Value> = if st == "streaming" {
                                vec![]
                            } else {
                                t.state.metadata.as_ref().and_then(|m| m.get("toolCalls")).and_then(|c| c.as_array()).map(|a| a.iter().filter(|c| matches!(str_of(c, "status"), Some("completed") | Some("error"))).collect()).unwrap_or_default()
                            };
                            for c in &calls {
                                if canonical_tool(str_of(c, "tool").unwrap_or("")) == "read" {
                                    counts[3] += 1;
                                } else {
                                    counts[4] += 1;
                                }
                            }
                            if calls.is_empty() && !is_active {
                                counts[4] += 1;
                            }
                            continue;
                        }
                        if is_active {
                            continue;
                        }
                        match name {
                            "shell" => counts[0] += 1,
                            "read" => counts[3] += 1,
                            "edit" | "write" | "patch" => counts[1] += 1,
                            _ => counts[4] += 1,
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    counts[5] = files.len();
    let names = ["command", "edit", "thought", "read", "tool", "instruction"];
    let label: Vec<String> = counts.iter().zip(names).filter(|(c, _)| **c > 0).map(|(c, n)| format!("{c} {n}{}", if *c == 1 { "" } else { "s" })).collect();
    let label = label.join(", ");
    (if label.is_empty() { active_label.unwrap_or_default() } else { label }, active)
}

fn render_turn_usage(ctx: &RCtx, ids: &[String]) -> Vec<SLine> {
    let th = ctx.t();
    let steps: Vec<&Message> = ids.iter().filter_map(|id| ctx.message(id)).collect();
    let Some(last) = steps.last() else { return vec![] };
    let t = last.tokens.clone().unwrap_or_default();
    let new = t.input + t.cache.write;
    let total = t.total();
    let text = format!(
        "{} step{} · latest: {} new · {} cached · {} total",
        steps.len(),
        if steps.len() == 1 { "" } else { "s" },
        locale::grouped(new as i64),
        locale::grouped(t.cache.read as i64),
        locale::grouped(total as i64)
    );
    vec![SLine::new(vec![pad(3), s("+ ", th.text.muted), Seg::new("Tokens", bold(fg(th.text.muted))), s(format!(": {text}"), th.text.muted)])]
}

pub fn render_entry(ctx: &RCtx, e: &Entry, light: bool) -> Vec<SLine> {
    match e {
        Entry::Message(id) => render_message(ctx, id, light),
        Entry::Part(r) => render_part(ctx, r),
        Entry::Footer(id) => render_footer(ctx, id),
        Entry::CompactionQueued(_) => {
            let th = ctx.t();
            vec![rule_line(ctx, vec![s("◇ Compaction queued", th.text.muted)], th.border)]
        }
        Entry::TurnUsage(ids) => render_turn_usage(ctx, ids),
    }
}

pub fn render_row(ctx: &RCtx, row: &Row, light: bool) -> Vec<SLine> {
    match row {
        Row::Entry(e) => render_entry(ctx, e, light),
        Row::Group { kind, children, completed, pending } => group_lines(ctx, *kind, children, *completed, pending, 0, light),
    }
}

/// Revert banner (`RevertMessage`).
pub fn revert_banner(ctx: &RCtx, count: usize, files: &[FileDiff]) -> Vec<SLine> {
    let th = ctx.t();
    let bg = th.background.raised.base.tui();
    let bar = vec![Seg::new("┃", Style::default().fg(th.background.raised.base.tui()).bg(bg)), pad(2)];
    let mut out = vec![SLine::new(vec![bar[0].clone()])];
    out.push(SLine::new(vec![s(format!("{count} message{} reverted", if count == 1 { "" } else { "s" }), th.text.muted)]).prefixed(&bar));
    if !files.is_empty() {
        out.push(SLine::new(vec![bar[0].clone()]));
        for f in files {
            let label = match f.status.as_deref() {
                Some("added") => "A",
                Some("deleted") => "D",
                _ => "M",
            };
            let extra = if f.additions > 0 { sw(&format!("+{}", f.additions)) + 1 } else { 0 } + if f.deletions > 0 { sw(&format!("-{}", f.deletions)) + 1 } else { 0 };
            let mut segs = vec![s(label, th.text.muted), pad(1), s(locale::truncate_file_path(&f.file, ctx.width.saturating_sub(5 + extra).max(2)), th.text.base)];
            if f.additions > 0 {
                segs.push(s(format!(" +{}", f.additions), th.diff.text_added));
            }
            if f.deletions > 0 {
                segs.push(s(format!(" -{}", f.deletions), th.diff.text_removed));
            }
            out.push(SLine::new(segs).prefixed(&bar));
        }
        out.push(SLine::new(vec![bar[0].clone()]));
    }
    out.push(SLine::new(vec![s(&ctx.redo_key, th.text.base), s(" or /redo to restore", th.text.muted)]).prefixed(&bar));
    out.push(SLine::new(vec![bar[0].clone()]));
    out.into_iter().map(|l| l.with_bg(Some(bg)).on_click(Some(Click::RevertClear))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_collapse() {
        let (i, o, of) = collapse_shell("$ ls", "a\nb\nc", 10, 200);
        assert_eq!((i.as_str(), o.as_str(), of), ("$ ls", "a\nb\nc", false));
        let out: String = (0..30).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n");
        let (_, o, of) = collapse_shell("$ ls", &out, 10, 2000);
        assert!(of);
        assert!(o.starts_with("(23 earlier lines)"), "{o}");
        assert_eq!(o.lines().count(), 8);
    }

    #[test]
    fn rows_group() {
        let msgs: Vec<Message> = serde_json::from_value(serde_json::json!([
            {"id":"msg_1","type":"user","text":"hi","time":{"created":1}},
            {"id":"msg_2","type":"assistant","agent":"build","model":{"id":"m","providerID":"p"},"time":{"created":2,"completed":5},"finish":"stop",
             "content":[
                {"type":"reasoning","text":"think","time":{"created":2,"completed":3}},
                {"type":"tool","id":"t1","name":"read","state":{"status":"completed","input":{"path":"a"},"content":[{"type":"text","text":"x"}]},"time":{"created":3}},
                {"type":"tool","id":"t2","name":"grep","state":{"status":"completed","input":{"pattern":"a"},"content":[{"type":"text","text":"x"}]},"time":{"created":3}},
                {"type":"text","text":"done"}
             ]}
        ]))
        .unwrap();
        let rows = build_rows(&msgs, &HashSet::new(), &[], false, "medium", &HashSet::new());
        assert_eq!(rows.len(), 5, "{rows:?}");
        assert!(matches!(rows[1], Row::Group { kind: GroupKind::Reasoning, completed: true, .. }));
        assert!(matches!(&rows[2], Row::Group { kind: GroupKind::Exploration, children, completed: true, .. } if children.len() == 2));
        assert!(matches!(rows[4], Row::Entry(Entry::Footer(_))));
        let low = build_rows(&msgs, &HashSet::new(), &[], false, "low", &HashSet::new());
        assert!(matches!(low[1], Row::Group { kind: GroupKind::Activity, .. }));
        assert_eq!(low.len(), 4);
    }

    #[test]
    fn labels() {
        let m = ModelRef { id: "x".into(), provider_id: "p".into(), variant: Some("high".into()) };
        assert_eq!(switch_label(&m, &[], None), "Switched model to p/x/high");
        assert_eq!(switch_label(&m, &[], Some(&ModelRef { id: "x".into(), provider_id: "p".into(), variant: None })), "Switched variant to high");
        assert_eq!(subagent_model_label("anthropic/claude#high", &[]), "anthropic/claude (high)");
        assert_eq!(reasoning_title("**Inspecting PR**\n\nbody"), Some("Inspecting PR".into()));
    }
}
