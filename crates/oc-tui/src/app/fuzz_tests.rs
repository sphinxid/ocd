//! Fuzz-style robustness tests: random, unicode-heavy server events for the session on screen and
//! for other sessions, applied through the SSE path and rendered at several terminal sizes
//! (including tiny ones). Any panic here would kill a TUI process (the event loop runs on the
//! main thread), and since every TUI attached to a server receives the same events, it would kill
//! all of them at once.
//!
//! Longer runs: `OC_TUI_FUZZ_SEEDS=1500 OC_TUI_FUZZ_STEPS=150 [OC_TUI_FUZZ_START=n]`,
//! `OC_TUI_SWEEP_FULL=1`; add `CARGO_PROFILE_DEV_OVERFLOW_CHECKS=false` to match release arithmetic.

use super::tests::{app, render, session};
use super::*;
use serde_json::json;

/// Deterministic xorshift PRNG (no external dependency, reproducible seeds).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.next() % 100 < pct
    }
    fn pick<'a, T>(&mut self, v: &'a [T]) -> &'a T {
        &v[self.below(v.len())]
    }
}

const PIECES: &[&str] = &[
    "",
    "a",
    " ",
    "  ",
    "hello world",
    "漢字テスト",
    "全角ＡＢＣ",
    "한국어",
    "👨‍👩‍👧‍👦",
    "🏳️‍🌈",
    "😀",
    "e\u{301}\u{301}\u{301}",
    "\u{200b}",
    "\u{200d}",
    "\u{feff}",
    "\u{202e}rtl",
    "\u{fe0f}",
    "\t",
    "\t\t",
    "\n",
    "\r\n",
    "\r",
    "\n\n\n",
    "\x00",
    "\x07",
    "\x1b[31m",
    "\x1b]0;x\x07",
    "\x1b",
    "\u{7f}",
    "\u{85}",
    "\u{2028}",
    "\u{e000}",
    "**",
    "**bold**",
    "*",
    "_",
    "`",
    "```",
    "```rust\nfn x() {}\n```",
    "```\n",
    "~~~",
    "# ",
    "## heading",
    "> ",
    "> > > > > > > > > >",
    "- ",
    "- [ ] ",
    "1. ",
    "* * *",
    "|a|b|\n|-|-|\n|1|2|",
    "|",
    "[link](http://x)",
    "[",
    "](",
    "<b>",
    "</b>",
    "<!--",
    "\\",
    "$$",
    "@@ -1,3 +1,3 @@\n a\n-b\n+c\n d\n",
    "@@ -999999999,0 +0,99999 @@\n",
    "@@",
    "+",
    "-",
    "--- a/x\n+++ b/x\n",
    "*** Begin Patch\n*** Update File: 漢字.rs\n@@\n-a\n+b\n*** End Patch",
    "*** Add File: ",
    "/work/proj/src/main.rs",
    "/",
    "~/",
    "../../..",
    "C:\\x\\y",
    ".",
    "x.",
    ".x",
    "a/b/c/d/e/f/g/h/i/j/k/l/m/n/o/p/q/r/s/t/u/v/w/x/y/z.rs",
    "0",
    "-1",
    "NaN",
    "9999999999999999999999",
];

fn text(r: &mut Rng) -> String {
    let n = r.below(6);
    let mut s = String::new();
    for _ in 0..n {
        s.push_str(r.pick(PIECES));
    }
    if r.chance(5) {
        let unit = r.pick(PIECES).to_string();
        s.push_str(&unit.repeat(200 + r.below(800)));
    }
    if r.chance(3) {
        s.push_str(&"> ".repeat(300));
    }
    if r.chance(3) {
        s.push_str(&"- ".repeat(300));
        s.push('x');
    }
    if r.chance(3) {
        s.push_str(&"[".repeat(500));
    }
    s
}

fn time(r: &mut Rng) -> i64 {
    *r.pick(&[0, 1, 1000, 80_000, -1, -1_000_000, 1_700_000_000_000, 4_000_000_000_000, i64::MAX / 4, i64::MIN / 4])
}

fn num(r: &mut Rng) -> Value {
    r.pick(&[json!(0), json!(1), json!(-1), json!(2.5), json!(1e300), json!(-1e300), json!(u64::MAX), json!(i64::MIN), json!(null), json!("3")]).clone()
}

fn value(r: &mut Rng, depth: usize) -> Value {
    match r.below(if depth > 3 { 4 } else { 7 }) {
        0 => Value::String(text(r)),
        1 => num(r),
        2 => Value::Bool(r.chance(50)),
        3 => Value::Null,
        4 => Value::Array((0..r.below(4)).map(|_| value(r, depth + 1)).collect()),
        _ => {
            let mut o = serde_json::Map::new();
            for _ in 0..r.below(5) {
                let k = r.pick(KEYS).to_string();
                o.insert(k, value(r, depth + 1));
            }
            Value::Object(o)
        }
    }
}

const KEYS: &[&str] = &[
    "path", "filePath", "file", "command", "description", "pattern", "url", "query", "agent", "subagent_type", "id", "name", "content", "patchText",
    "oldString", "newString", "todos", "status", "output", "metadata", "diff", "files", "patch", "additions", "deletions", "matches", "count",
    "questions", "question", "options", "label", "text", "title", "exit", "truncated", "sessionID", "toolCalls", "tool", "preview", "lines",
    "offset", "limit", "include", "results", "error", "message", "before", "after", "type", "header", "multiple", "answers", "priority",
];

fn tool_input(r: &mut Rng) -> Value {
    let mut o = serde_json::Map::new();
    for _ in 0..r.below(6) {
        let k = r.pick(KEYS).to_string();
        let v = if r.chance(60) { Value::String(text(r)) } else { value(r, 1) };
        o.insert(k, v);
    }
    if r.chance(10) { value(r, 0) } else { Value::Object(o) }
}

fn metadata(r: &mut Rng) -> Value {
    let mut o = serde_json::Map::new();
    if r.chance(40) {
        let files: Vec<Value> = (0..r.below(3))
            .map(|_| json!({"file": text(r), "status": r.pick(&["modified","added","deleted","x"]), "additions": num(r), "deletions": num(r), "patch": text(r), "before": text(r), "after": text(r)}))
            .collect();
        o.insert("files".into(), Value::Array(files));
    }
    if r.chance(30) {
        o.insert("diff".into(), Value::String(text(r)));
    }
    if r.chance(30) {
        o.insert("output".into(), Value::String(text(r)));
    }
    if r.chance(20) {
        o.insert("todos".into(), json!([{"content": text(r), "status": r.pick(&["pending","in_progress","completed","cancelled", ""]), "priority": "high"}]));
    }
    if r.chance(20) {
        o.insert("sessionID".into(), json!(r.pick(&["ses_child", "ses_other", "ses_test", "ses_missing"])));
    }
    if r.chance(20) {
        o.insert("toolCalls".into(), json!([{"tool": text(r), "status": "running"}, {"tool": text(r), "status": "completed"}]));
    }
    for _ in 0..r.below(3) {
        o.insert(r.pick(KEYS).to_string(), value(r, 1));
    }
    if r.chance(5) { value(r, 0) } else { Value::Object(o) }
}

const TOOLS: &[&str] = &[
    "read", "write", "edit", "bash", "shell", "patch", "apply_patch", "grep", "glob", "list", "webfetch", "websearch", "task", "subagent", "todowrite", "todoread",
    "question", "skill", "execute", "codesearch", "lsp", "mcp_server_tool", "", "漢字",
];

const SIDS: &[&str] = &["ses_test", "ses_test", "ses_test", "ses_other", "ses_child", "ses_missing"];
const MIDS: &[&str] = &["msg_a", "msg_b", "msg_c", "msg_2", "msg_9", ""];
const TIDS: &[&str] = &["t1", "t2", "t3", "t9", ""];

fn content(r: &mut Rng) -> Value {
    let items: Vec<Value> = (0..r.below(3))
        .map(|_| match r.below(3) {
            0 => json!({"type": "text", "text": text(r)}),
            1 => json!({"type": "image", "uri": text(r), "mime": "image/png"}),
            _ => json!({"type": r.pick(&["file", "resource", "x"]), "uri": text(r), "name": text(r), "text": text(r)}),
        })
        .collect();
    Value::Array(items)
}

fn error(r: &mut Rng) -> Value {
    json!({"type": r.pick(&["provider.auth", "x", "", "aborted"]), "message": text(r), "status": num(r)})
}

fn event(r: &mut Rng) -> Event {
    let sid = r.pick(SIDS).to_string();
    let mid = r.pick(MIDS).to_string();
    let tid = r.pick(TIDS).to_string();
    let kinds = [
        "session.step.started",
        "session.step.streamed",
        "session.step.ended",
        "session.step.failed",
        "session.text.started",
        "session.text.delta",
        "session.text.delta",
        "session.text.ended",
        "session.reasoning.started",
        "session.reasoning.delta",
        "session.reasoning.ended",
        "session.tool.input.started",
        "session.tool.input.delta",
        "session.tool.input.ended",
        "session.tool.called",
        "session.tool.progress",
        "session.tool.success",
        "session.tool.failed",
        "session.retry.scheduled",
        "session.execution.started",
        "session.execution.succeeded",
        "session.execution.failed",
        "session.execution.interrupted",
        "session.compaction.started",
        "session.compaction.delta",
        "session.compaction.ended",
        "session.compaction.failed",
        "session.inbox.enqueued",
        "session.inbox.delivered",
        "session.inbox.delivery.changed",
        "session.inbox.cancelled",
        "session.renamed",
        "session.usage.updated",
        "session.agent.selected",
        "session.model.selected",
        "session.moved",
        "session.instructions.updated",
        "session.synthetic",
        "session.skill.activated",
        "session.shell.started",
        "session.shell.ended",
        "session.revert.staged",
        "session.revert.cleared",
        "session.revert.committed",
        "session.viewed",
        "session.created",
        "session.deleted",
        "permission.asked",
        "permission.replied",
        "form.created",
        "form.replied",
        "tui.toast.show",
        "vcs.branch.updated",
        "shell.created",
        "shell.exited",
        "server.connected",
    ];
    let kind = r.pick(&kinds).to_string();
    let mut d = json!({ "sessionID": sid, "assistantMessageID": mid, "id": tid });
    let o = d.as_object_mut().unwrap();
    let mut put = |k: &str, v: Value| {
        o.insert(k.into(), v);
    };
    match kind.as_str() {
        "session.step.started" => {
            put("agent", json!(r.pick(&["build", "plan", "explore", "", "漢字"])));
            put("model", json!({"id": r.pick(&["claude-sonnet", "x", ""]), "providerID": r.pick(&["anthropic", "y"])}));
            put("started", json!(time(r)));
        }
        "session.step.ended" | "session.step.failed" => {
            put("finish", json!(r.pick(&["stop", "tool-calls", "error", "length", ""])));
            put("cost", num(r));
            put("tokens", json!({"input": num(r), "output": num(r), "reasoning": num(r), "cache": {"read": num(r), "write": num(r)}}));
            put("error", error(r));
        }
        "session.text.delta" | "session.reasoning.delta" | "session.tool.input.delta" => put("delta", json!(text(r))),
        "session.text.ended" | "session.reasoning.ended" | "session.tool.input.ended" | "session.compaction.delta" => put("text", json!(text(r))),
        "session.tool.input.started" => put("name", json!(r.pick(TOOLS))),
        "session.tool.called" => {
            put("input", tool_input(r));
            put("executed", json!(r.chance(50)));
        }
        "session.tool.progress" => put("metadata", metadata(r)),
        "session.tool.success" | "session.tool.failed" => {
            put("metadata", metadata(r));
            put("content", content(r));
            put("error", error(r));
            put("executed", json!(r.chance(50)));
        }
        "session.retry.scheduled" => {
            put("attempt", num(r));
            put("at", json!(time(r)));
            put("error", error(r));
        }
        "session.execution.failed" => put("error", error(r)),
        "session.execution.interrupted" => put("reason", json!(r.pick(&["user", "shutdown", ""]))),
        "session.compaction.started" | "session.compaction.failed" => {
            put("reason", json!(r.pick(&["auto", "manual", ""])));
            if r.chance(50) {
                put("inputID", json!(r.pick(&["msg_u", "msg_c"])));
            }
            put("error", error(r));
        }
        "session.compaction.ended" => {
            put("text", json!(text(r)));
            put("tokens", json!({"input": num(r), "output": num(r)}));
            put("cost", num(r));
        }
        "session.inbox.enqueued" => {
            put("inboxID", json!(r.pick(&["msg_u", "msg_v", "msg_c"])));
            let kind = *r.pick(&["user", "synthetic", "compaction", "x"]);
            let mut payload = json!({"text": text(r)});
            if r.chance(30) {
                payload["files"] = json!([{"uri": text(r), "name": text(r), "mime": "text/plain", "mention": {"start": num(r), "end": num(r), "text": text(r)}}]);
            }
            if r.chance(20) {
                payload["agents"] = json!([{"name": text(r), "mention": {"start": num(r), "end": num(r), "text": text(r)}}]);
            }
            put("item", json!({"type": kind, "payload": payload, "delivery": r.pick(&["steer", "queue", ""])}));
        }
        "session.inbox.delivered" | "session.inbox.cancelled" => put("inboxID", json!(r.pick(&["msg_u", "msg_v", "msg_c", "msg_1"]))),
        "session.inbox.delivery.changed" => {
            put("inboxID", json!(r.pick(&["msg_u", "msg_v"])));
            put("delivery", json!(r.pick(&["steer", "queue"])));
        }
        "session.renamed" => put("title", json!(text(r))),
        "session.usage.updated" => {
            put("cost", num(r));
            put("tokens", json!({"input": num(r), "output": num(r)}));
        }
        "session.agent.selected" => put("agent", json!(text(r))),
        "session.model.selected" => put("model", json!({"id": text(r), "providerID": text(r), "variant": text(r)})),
        "session.moved" => {
            put("location", json!({"directory": text(r)}));
            put("projectID", json!(text(r)));
            put("subpath", json!(text(r)));
        }
        "session.instructions.updated" => {
            put("text", json!(text(r)));
            put("delta", json!({ text(r): 1 }));
        }
        "session.synthetic" => {
            put("text", json!(text(r)));
            put("description", json!(text(r)));
            put("metadata", metadata(r));
        }
        "session.skill.activated" => {
            put("name", json!(text(r)));
            put("text", json!(text(r)));
        }
        "session.shell.started" | "session.shell.ended" => {
            put("shell", json!({"id": r.pick(&["sh_1", "sh_2"]), "command": text(r), "status": r.pick(&["running", "exited", ""]), "exit": num(r), "metadata": {"background": r.chance(30)}}));
            put("output", json!({"output": text(r), "cursor": num(r), "size": num(r), "truncated": r.chance(50)}));
        }
        "session.revert.staged" => put("revert", json!({"messageID": r.pick(MIDS), "files": [{"file": text(r), "patch": text(r), "additions": num(r), "deletions": num(r)}]})),
        "session.revert.committed" => put("to", json!(r.pick(&["msg_0", "msg_2", "msg_b", "msg_z", ""]))),
        "permission.asked" => {
            put("id", json!(r.pick(&["per_1", "per_2"])));
            put("action", json!(r.pick(&["edit", "bash", "read", "webfetch", "external_directory", "x", ""])));
            put("resources", json!([text(r), text(r)]));
            put("metadata", metadata(r));
            if r.chance(50) {
                put("tool", json!({"messageID": mid, "callID": tid}));
            }
        }
        "permission.replied" => put("requestID", json!(r.pick(&["per_1", "per_2"]))),
        "form.created" => {
            let fields: Vec<Value> = (0..r.below(3))
                .map(|_| {
                    json!({"key": text(r), "type": r.pick(&["boolean", "string", "select", "multiselect", "number", "x"]), "title": text(r), "description": text(r),
                           "options": [{"value": text(r), "label": text(r)}, {"value": text(r)}], "default": value(r, 2)})
                })
                .collect();
            put("form", json!({"id": r.pick(&["frm_1", "frm_2"]), "sessionID": r.pick(&["ses_test", "ses_other", "global"]), "title": text(r), "description": text(r), "fields": fields}));
        }
        "form.replied" => put("id", json!(r.pick(&["frm_1", "frm_2"]))),
        "tui.toast.show" => {
            put("message", json!(text(r)));
            put("title", json!(text(r)));
            put("variant", json!(r.pick(&["info", "error", "warning", "success", "x"])));
            put("duration", num(r));
        }
        "vcs.branch.updated" => put("branch", json!(text(r))),
        "shell.created" => put("info", json!({"id": "sh_1", "command": text(r), "status": "running"})),
        _ => {}
    }
    let location = if r.chance(20) { Some(LocationRef { directory: r.pick(&["/work/proj", "/other"]).to_string(), workspace_id: None }) } else { None };
    Event { id: format!("evt_{}", r.pick(&["a", "b", "c", "d", "e"])), created: time(r), kind, location, metadata: if r.chance(10) { Some(metadata(r)) } else { None }, data: d }
}

/// A coherent burst: a step with a streamed text/reasoning part and a tool call that runs and
/// finishes, so render paths for finished tools (diffs, outputs, todos, questions) are reached.
fn scenario(r: &mut Rng) -> Vec<Event> {
    let sid = *r.pick(SIDS);
    let mid = format!("msg_s{}", r.below(4));
    let tid = format!("t_s{}", r.below(4));
    let mk = |kind: &str, mut data: Value, created: i64| {
        data["sessionID"] = json!(sid);
        data["assistantMessageID"] = json!(mid);
        Event { id: format!("evt_s{created}"), created, kind: kind.into(), data, ..Default::default() }
    };
    let t0 = time(r);
    let mut v = vec![mk("session.execution.started", json!({}), t0)];
    v.push(mk("session.step.started", json!({"agent": "build", "model": {"id": "claude-sonnet", "providerID": "anthropic"}, "started": t0}), t0));
    if r.chance(50) {
        v.push(mk("session.reasoning.started", json!({}), t0));
        v.push(mk("session.reasoning.delta", json!({"delta": text(r)}), t0));
        if r.chance(70) {
            v.push(mk("session.reasoning.ended", json!({"text": text(r)}), time(r)));
        }
    }
    if r.chance(60) {
        v.push(mk("session.text.started", json!({}), t0));
        v.push(mk("session.text.delta", json!({"delta": text(r)}), t0));
        v.push(mk("session.text.delta", json!({"delta": text(r)}), t0));
        if r.chance(70) {
            v.push(mk("session.text.ended", json!({"text": text(r)}), t0));
        }
    }
    for _ in 0..1 + r.below(3) {
        let name = *r.pick(TOOLS);
        v.push(mk("session.tool.input.started", json!({"id": tid, "name": name}), t0));
        v.push(mk("session.tool.input.delta", json!({"id": tid, "delta": text(r)}), t0));
        if r.chance(85) {
            v.push(mk("session.tool.called", json!({"id": tid, "input": tool_input(r), "executed": true}), t0));
            if r.chance(50) {
                v.push(mk("session.tool.progress", json!({"id": tid, "metadata": metadata(r)}), t0));
            }
            if r.chance(80) {
                let kind = if r.chance(75) { "session.tool.success" } else { "session.tool.failed" };
                v.push(mk(kind, json!({"id": tid, "metadata": metadata(r), "content": content(r), "error": error(r)}), time(r)));
            }
        }
    }
    if r.chance(40) {
        v.push(mk("session.step.streamed", json!({}), time(r)));
    }
    if r.chance(60) {
        v.push(mk("session.step.ended", json!({"finish": "stop", "tokens": {"input": num(r), "output": num(r)}}), time(r)));
    }
    if r.chance(40) {
        v.push(mk(r.pick(&["session.execution.succeeded", "session.execution.failed", "session.execution.interrupted"]), json!({"error": error(r)}), time(r)));
    }
    v
}

const SIZES: &[(u16, u16)] = &[(1, 1), (2, 5), (20, 3), (5, 40), (30, 10), (44, 14), (80, 24), (121, 40), (200, 50)];

fn setup(r: &mut Rng) -> App {
    let mut a = app();
    if r.chance(50) {
        a.config.set("tabs", Some(json!({"mode": "on", "layout": *r.pick(&["horizontal", "vertical"])})));
    }
    if r.chance(30) {
        a.config.set("animations", Some(json!(true)));
    }
    if r.chance(50) {
        a.config.set(
            "session",
            Some(json!({
                "thinking": *r.pick(&["show", "hide", "auto"]),
                "verbosity": *r.pick(&["low", "medium", "high"]),
                "markdown": *r.pick(&["rendered", "raw"]),
                "grouping": *r.pick(&["auto", "off", "on"]),
                "scrollbar": r.chance(50),
                "tps": r.chance(50),
            })),
        );
    }
    if r.chance(30) {
        a.config.set("diffs", Some(json!({"view": *r.pick(&["auto", "split", "unified"]), "wrap": *r.pick(&["word", "none", "char"])})));
    }
    let sid = session(&mut a);
    a.store.remember(SessionInfo { id: "ses_other".into(), title: Some(text(r)), location: LocationRef { directory: "/work/proj".into(), workspace_id: None }, ..Default::default() });
    a.store.remember(SessionInfo { id: "ses_child".into(), parent_id: Some(sid.clone()), title: Some(text(r)), ..Default::default() });
    a.tabs = vec![sid.clone(), "ses_other".into()];
    if r.chance(30) {
        a.views.entry(sid.clone()).or_default().sidebar_open = Some(true);
    }
    if r.chance(15) {
        a.route = Route::Home;
    }
    a
}

/// What a reconnect (or loading older history) does to the current session: the message list is
/// replaced by a fresh page from the server (latest page only, server copies without local
/// revisions), older pages are prepended, the session info and the inbox are re-read.
fn reload(a: &mut App, r: &mut Rng) {
    let Some(sid) = a.session_id().map(str::to_string) else { return };
    let cur: Vec<Message> = a.store.messages(&sid).iter().map(|m| Message { rev: Default::default(), ..m.clone() }).collect();
    match r.below(7) {
        6 => {
            // Location catalogs re-read after `*.updated` events or a reconnect.
            let loc = a.store.location_mut("/work/proj");
            let mut agents = vec![AgentInfo { id: "build".into(), name: "build".into(), mode: "primary".into(), ..Default::default() }];
            for _ in 0..r.below(4) {
                agents.push(AgentInfo {
                    id: text(r),
                    name: text(r),
                    mode: (*r.pick(&["primary", "subagent", "all", ""])).into(),
                    hidden: r.chance(20),
                    color: Some((*r.pick(&["#aé1234", "#ff0000", "red", "", "#zzzzzz", "#12"])).into()),
                    description: Some(text(r)),
                    ..Default::default()
                });
            }
            loc.agents = Some(agents);
            loc.models = Some(vec![ModelInfo {
                id: "claude-sonnet".into(),
                provider_id: "anthropic".into(),
                name: text(r),
                variants: vec![ModelVariant { id: text(r) }],
                limit: ModelLimit { context: *r.pick(&[0.0, 1.0, 200000.0, 1e300]), output: 0.0 },
                status: "active".into(),
                enabled: true,
                ..Default::default()
            }]);
            loc.providers = Some(vec![ProviderInfo { id: "anthropic".into(), name: text(r), ..Default::default() }]);
            loc.mcp = Some((0..r.below(3)).map(|_| McpServer { name: text(r), status: McpStatus { status: (*r.pick(&["connected", "failed", "pending", "x"])).into(), error: Some(text(r)) }, ..Default::default() }).collect());
            loc.vcs = Some(json!({"branch": {"current": text(r), "default": "main"}}));
        }
        0 => {
            // Latest page only.
            let keep = r.below(cur.len() + 1);
            let page: Vec<Message> = cur[cur.len() - keep..].iter().rev().cloned().collect();
            a.store.apply_messages(&sid, page, if r.chance(50) { Some("cursor".into()) } else { None });
        }
        1 => {
            // Same messages, fresh copies.
            a.store.apply_messages(&sid, cur.iter().rev().cloned().collect(), None);
        }
        2 => {
            // Some messages gone (reverted / compacted on the server), maybe in a different order.
            let mut page: Vec<Message> = cur.iter().filter(|_| r.chance(70)).cloned().collect();
            if r.chance(30) && page.len() > 1 {
                let i = r.below(page.len());
                let j = r.below(page.len());
                page.swap(i, j);
            }
            page.reverse();
            a.store.apply_messages(&sid, page, None);
        }
        3 => {
            // An older page.
            let older: Vec<Message> = (0..r.below(4))
                .map(|k| Message { id: format!("msg_0{k}"), kind: (*r.pick(&["user", "assistant", "idle", "synthetic"])).into(), text: Some(text(r)), ..Default::default() })
                .collect();
            a.store.prepend_messages(&sid, older, None);
        }
        4 => {
            let mut info = a.store.session(&sid).cloned().unwrap_or_default();
            info.title = Some(text(r));
            info.revert = if r.chance(50) { Some(Revert { message_id: (*r.pick(MIDS)).into(), ..Default::default() }) } else { None };
            a.store.remember(info);
        }
        _ => {
            a.store.apply_pending(&sid, Vec::new());
            a.store.evict(&sid);
        }
    }
    a.dirty = true;
}

/// Feed one seed's events and render after every few, catching the first panic with context.
fn run_seed(seed: u64, steps: usize) -> Result<(), String> {
    let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut a = setup(&mut r);
    let mut log: Vec<String> = Vec::new();
    let mut queue: std::collections::VecDeque<Event> = Default::default();
    let tail = |log: &[String], n: usize| log.iter().rev().take(n).cloned().collect::<Vec<_>>().join("\n");
    for step in 0..steps {
        if queue.is_empty() && r.chance(25) {
            queue.extend(scenario(&mut r));
        }
        let ev = queue.pop_front().unwrap_or_else(|| event(&mut r));
        log.push(format!("{} {}", ev.kind, ev.data));
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| a.on_sse(crate::api::sse::SseMsg::Event(Box::new(ev)))));
        if res.is_err() {
            return Err(format!("seed {seed} step {step}: panic applying event\n{}", tail(&log, 5)));
        }
        if r.chance(8) {
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| reload(&mut a, &mut r)));
            if res.is_err() {
                return Err(format!("seed {seed} step {step}: panic reloading messages\n{}", tail(&log, 5)));
            }
        }
        if r.chance(3) {
            a.on_sse(crate::api::sse::SseMsg::Status { status: crate::api::sse::ConnStatus::Reconnecting, attempt: 1, error: Some(text(&mut r)) });
        } else if r.chance(3) {
            a.on_sse(crate::api::sse::SseMsg::Status { status: crate::api::sse::ConnStatus::Connected, attempt: 0, error: None });
        }
        if step % 3 == 2 {
            let (w, h) = *r.pick(SIZES);
            if r.chance(5) {
                a.tick();
            }
            if r.chance(10)
                && let Some(sid) = a.session_id().map(str::to_string)
            {
                a.views.entry(sid).or_default().scroll_from_bottom = r.below(200);
            }
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| render(&mut a, w, h)));
            if res.is_err() {
                return Err(format!("seed {seed} step {step}: panic rendering {w}x{h}\n{}", tail(&log, 8)));
            }
            // Expand / collapse something that was drawn (tool details, groups, errors, thinking).
            let toggles: Vec<Click> = a
                .hits
                .iter()
                .map(|(_, c)| c.clone())
                .filter(|c| matches!(c, Click::ToggleGroup(_) | Click::ToggleTool(_) | Click::ToggleError(_) | Click::ToggleThinking(_)))
                .collect();
            if !toggles.is_empty() && r.chance(60) {
                let c = r.pick(&toggles).clone();
                let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| a.on_click(c.clone())));
                if res.is_err() {
                    return Err(format!("seed {seed} step {step}: panic on click {c:?}"));
                }
            }
        }
    }
    Ok(())
}

#[test]
fn fuzz_sse_events_and_render() {
    let seeds: u64 = std::env::var("OC_TUI_FUZZ_SEEDS").ok().and_then(|v| v.parse().ok()).unwrap_or(12);
    let steps: usize = std::env::var("OC_TUI_FUZZ_STEPS").ok().and_then(|v| v.parse().ok()).unwrap_or(80);
    let start: u64 = std::env::var("OC_TUI_FUZZ_START").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
    let mut failures = Vec::new();
    for seed in start..start + seeds {
        if let Err(e) = run_seed(seed, steps) {
            failures.push(e);
            if failures.len() >= 5 {
                break;
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// Rich screens rendered at every terminal size up to 130x45 (a window resize hits every TUI in it
/// at once).
fn sweep_states() -> Vec<(&'static str, App)> {
    let mut out = Vec::new();
    let ev = |kind: &str, data: Value| Event { id: format!("evt_{}", crate::util::id_body(false)), created: 80_000, kind: kind.into(), data, ..Default::default() };
    // Busy session with a running shell, a streaming answer, tabs, sidebar and a toast.
    let mut a = app();
    a.config.set("tabs", Some(json!({"mode": "on"})));
    let sid = session(&mut a);
    a.store.remember(SessionInfo { id: "ses_other".into(), title: Some("全角のタイトル 👨‍👩‍👧‍👦 with a long tail".into()), location: LocationRef { directory: "/work/proj".into(), workspace_id: None }, ..Default::default() });
    a.tabs = vec![sid.clone(), "ses_other".into()];
    a.on_event(ev("session.execution.started", json!({"sessionID": sid})));
    a.on_event(ev("session.step.started", json!({"sessionID": sid, "assistantMessageID": "msg_9", "agent": "build", "model": {"id":"claude-sonnet","providerID":"anthropic"}, "started": 80000})));
    a.on_event(ev("session.text.started", json!({"sessionID": sid, "assistantMessageID": "msg_9"})));
    a.on_event(ev("session.text.delta", json!({"sessionID": sid, "assistantMessageID": "msg_9", "delta": "漢字 **bold** `code` |a|b|\n|-|-|\n|全角|x|\n\n```rust\nfn main() {}\n```\n> quote 👍"})));
    a.on_event(ev("session.tool.input.started", json!({"sessionID": sid, "assistantMessageID": "msg_9", "id": "t9", "name": "bash"})));
    a.on_event(ev("session.tool.called", json!({"sessionID": sid, "assistantMessageID": "msg_9", "id": "t9", "input": {"command": "echo 漢字\tand tabs\nsecond line"}})));
    a.toast(Toast::new("error", "Something 全角 failed\nbadly").title("Failed"));
    a.views.entry(sid.clone()).or_default().sidebar_open = Some(true);
    out.push(("busy", a));
    // Vertical tabs and a permission prompt.
    let mut a = app();
    a.config.set("tabs", Some(json!({"mode": "on", "layout": "vertical"})));
    let sid = session(&mut a);
    a.tabs = vec![sid.clone()];
    a.store.permissions.insert(
        sid.clone(),
        vec![PermissionRequest {
            id: "per_1".into(),
            session_id: sid.clone(),
            action: "bash".into(),
            resources: vec!["rm -rf 漢字/build".into()],
            save: Some(vec!["rm *".into()]),
            metadata: Some(json!({"command": "rm -rf 漢字/build"})),
            ..Default::default()
        }],
    );
    out.push(("permission", a));
    // A form prompt.
    let mut a = app();
    let sid = session(&mut a);
    a.store.forms.insert(
        sid.clone(),
        vec![(
            FormInfo {
                id: "frm_1".into(),
                session_id: sid.clone(),
                title: "Setup 全角".into(),
                fields: vec![
                    json!({"key":"lang","type":"string","title":"Language","description":"Which language?","options":[{"value":"ts","label":"TypeScript"},{"value":"rs","label":"Rust 🦀"}],"custom":true}),
                    json!({"key":"tests","type":"boolean","title":"Tests"}),
                ],
                ..Default::default()
            },
            None,
        )],
    );
    out.push(("form", a));
    // Home with the command palette open.
    let mut a = app();
    a.run_command("command.palette.show", None);
    out.push(("palette", a));
    // Session list dialog.
    let mut a = app();
    session(&mut a);
    a.store.remember(SessionInfo { id: "ses_other".into(), title: Some("👨‍👩‍👧‍👦".repeat(30)), location: LocationRef { directory: "/work/proj".into(), workspace_id: None }, ..Default::default() });
    a.run_command("session.list", None);
    out.push(("sessions", a));
    out
}

/// Every width at a few heights and every height at a few widths (`OC_TUI_SWEEP_FULL=1`: every
/// size up to 130x45). Narrower than 10 columns, dialogs used to compute `width - 10` in u16.
#[test]
fn render_every_size() {
    let full = std::env::var("OC_TUI_SWEEP_FULL").is_ok();
    let mut sizes: Vec<(u16, u16)> = Vec::new();
    for w in 0..=130u16 {
        for h in 0..=45u16 {
            let edge_h = [0, 1, 2, 3, 4, 5, 7, 10, 14, 16, 24, 40].contains(&h);
            let edge_w = [0, 1, 2, 5, 9, 10, 43, 44, 64, 80, 120, 121].contains(&w);
            if full || edge_h || edge_w {
                sizes.push((w, h));
            }
        }
    }
    for (name, mut a) in sweep_states() {
        for &(w, h) in &sizes {
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| render(&mut a, w, h)));
            assert!(res.is_ok(), "{name}: panic rendering at {w}x{h}");
        }
    }
}


/// The embedded terminal (vt100 on the UI thread) on PTY output and resizes, as full-screen
/// programs produce them, rendered into small and normal panes.
#[test]
fn fuzz_terminal_emulator() {
    let theme = crate::theme::ThemeManager::new(None, crate::theme::Mode::Dark, None).current;
    let chunks: &[&[u8]] = &[
        b"hello", "漢字👨‍👩‍👧‍👦e\u{301}".as_bytes(), b"\x1b[31m", b"\x1b[2J", b"\x1b[H", b"\x1b[999;999H", b"\x1b[5;2r", b"\x1b[0;0r", b"\x1b[?1049h", b"\x1b[?1049l",
        b"\x1b[10L", b"\x1b[10M", b"\x1b[10@", b"\x1b[10P", b"\x1b[10X", b"\x1b[S", b"\x1b[T", b"\x1bD", b"\x1bM", b"\x1bE", b"\x1b7", b"\x1b8", b"\x1bc",
        b"\r\n", b"\t\t\t", b"\x08\x08", b"\x1b[?25l", b"\x1b[38;2;1;2;3m", b"\x1b[48;5;200m", b"\x1b[7m", b"\x1b[4h", b"\x1b[?7l", b"\x1b[?7h", b"\xff\xfe", b"\x1b[",
        "全角全角全角全角".as_bytes(), b"\x1b[1;1H\x1b[2K", b"\x1b[J", b"\x1b[1J", b"\x1b[K", b"\x1b[1K", b"\x1b[?6h", b"\x1b[?6l", b"\x1b[3g", b"\x1bH", b"\x1b[100G",
    ];
    for seed in 1..300u64 {
        let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let mut st = super::terminal::TermState::new("pty_fuzz");
        st.set_size(*r.pick(&[0u16, 1, 2, 5, 24, 80]), *r.pick(&[0u16, 1, 2, 5, 80, 200]));
        for _ in 0..60 {
            if r.chance(10) {
                st.set_size(*r.pick(&[1u16, 2, 3, 24, 40, 80]), *r.pick(&[1u16, 2, 3, 40, 80, 200]));
            }
            st.process(r.pick(chunks));
            if r.chance(20) {
                let (w, h) = *r.pick(&[(1u16, 1u16), (3, 2), (40, 10), (100, 30)]);
                let mut buf = ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, w, h));
                super::terminal::render_screen(&mut buf, ratatui::layout::Rect::new(1, 0, w.saturating_sub(1), h), st.parser.screen(), &theme, true, true);
            }
        }
    }
}
