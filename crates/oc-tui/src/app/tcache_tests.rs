//! Equivalence of the incremental transcript cache with the reference render.

use super::super::perf::{delta, ev, long_session, perf_app, start_stream};
use super::*;
use serde_json::json;

/// The cache must produce exactly the reference render: every line, every mark.
fn check(app: &mut App, sid: &str, width: usize, what: &str) {
    app.transcript_sync(sid, width);
    let (lines, marks) = app.render_transcript_lines(sid, width);
    let got = app.tcache.to_vec();
    for (i, (a, b)) in got.iter().zip(lines.iter()).enumerate() {
        assert_eq!(a, b, "{what}: line {i} differs");
    }
    assert_eq!(got.len(), lines.len(), "{what}: line count");
    assert_eq!(app.tcache.marks(), &marks[..], "{what}: marks");
    let n = lines.len();
    for off in [0, n / 3, n / 2, n.saturating_sub(6)] {
        let end = (off + 9).min(n);
        assert_eq!(app.tcache.range(off, end), lines[off..end].to_vec(), "{what}: window at {off}");
    }
}

fn e(app: &mut App, kind: &str, data: serde_json::Value) {
    app.on_event(ev(kind, data));
}

/// Toggle expandable rows (groups, tools, errors), checking after each.
fn toggle_all(app: &mut App, sid: &str, width: usize, what: &str) {
    let (lines, _) = app.render_transcript_lines(sid, width);
    let mut keys: Vec<String> = lines
        .iter()
        .filter_map(|l| match &l.click {
            Some(Click::ToggleGroup(id)) | Some(Click::ToggleTool(id)) => Some(id.clone()),
            Some(Click::ToggleError(id)) => Some(format!("error:{id}")),
            _ => None,
        })
        .collect();
    keys.dedup();
    for k in keys.iter().rev().take(6) {
        let v = app.view(sid);
        let cur = v.expanded.get(k).copied().unwrap_or(false);
        v.expanded.insert(k.clone(), !cur);
        check(app, sid, width, &format!("{what}: toggle {k}"));
    }
}

#[test]
fn incremental_render_matches_reference() {
    let mut a = perf_app();
    let t0 = 2_000_000_000_000;
    a.clock = Some(t0);
    let sid = long_session(&mut a, 8);
    let s = sid.as_str();
    let w = 100;
    check(&mut a, s, w, "initial");
    start_stream(&mut a, s);
    check(&mut a, s, w, "stream started");
    for k in 0..12 {
        a.on_event(delta(s, k));
        check(&mut a, s, w, &format!("delta {k}"));
    }
    let live = |extra: serde_json::Value| {
        let mut d = json!({"sessionID": s, "assistantMessageID": "msg_zz_live"});
        d.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        d
    };
    // Reasoning.
    e(&mut a, "session.reasoning.started", live(json!({})));
    e(&mut a, "session.reasoning.delta", live(json!({"delta": "**Planning**\n\nthinking hard"})));
    check(&mut a, s, w, "reasoning streaming");
    e(&mut a, "session.reasoning.ended", live(json!({"text": "**Planning**\n\nthinking hard about it"})));
    check(&mut a, s, w, "reasoning ended");
    // A shell tool that goes to the background.
    e(&mut a, "session.tool.input.started", live(json!({"id": "tl_sh", "name": "bash"})));
    e(&mut a, "session.tool.input.delta", live(json!({"id": "tl_sh", "delta": "{\"command\":"})));
    check(&mut a, s, w, "tool input streaming");
    e(&mut a, "session.tool.called", live(json!({"id": "tl_sh", "input": {"command": "npm test"}})));
    check(&mut a, s, w, "tool running");
    a.clock = Some(t0 + 5000);
    check(&mut a, s, w, "background hint");
    e(&mut a, "session.tool.progress", live(json!({"id": "tl_sh", "metadata": {"shellID": "sh_1"}})));
    check(&mut a, s, w, "tool progress");
    a.shell_output.insert("sh_1".into(), "line 1\nline 2".into());
    check(&mut a, s, w, "shell output");
    a.store.location_mut("/work/proj").shells.push(ShellInfo { id: "sh_1".into(), ..Default::default() });
    check(&mut a, s, w, "shell listed");
    e(&mut a, "session.tool.success", live(json!({"id": "tl_sh", "metadata": {"shellID": "sh_1", "status": "running"}, "content": [{"type": "text", "text": "started"}]})));
    check(&mut a, s, w, "tool backgrounded");
    // Exploration tools and a failing edit.
    for (id, name, input) in [("tl_r", "read", json!({"path": "/work/proj/a.ts"})), ("tl_g", "grep", json!({"pattern": "foo"}))] {
        e(&mut a, "session.tool.input.started", live(json!({"id": id, "name": name})));
        e(&mut a, "session.tool.called", live(json!({"id": id, "input": input})));
        check(&mut a, s, w, &format!("{name} running"));
        e(&mut a, "session.tool.success", live(json!({"id": id, "content": [{"type": "text", "text": "ok"}]})));
        check(&mut a, s, w, &format!("{name} done"));
    }
    e(&mut a, "session.tool.input.started", live(json!({"id": "tl_e", "name": "edit"})));
    e(&mut a, "session.tool.failed", live(json!({"id": "tl_e", "error": {"type": "x", "message": "boom: file changed"}})));
    check(&mut a, s, w, "edit failed");
    // A subagent whose session runs.
    e(&mut a, "session.tool.input.started", live(json!({"id": "tl_sa", "name": "task"})));
    e(&mut a, "session.tool.called", live(json!({"id": "tl_sa", "input": {"description": "look around", "agent": "explore"}})));
    e(&mut a, "session.tool.progress", live(json!({"id": "tl_sa", "metadata": {"sessionID": "ses_child"}})));
    check(&mut a, s, w, "subagent running");
    a.store.active.insert("ses_child".into(), true);
    check(&mut a, s, w, "subagent session active");
    toggle_all(&mut a, s, w, "live turn");
    // Permission on a tool (blocks it), auto-accept, reply.
    e(&mut a, "session.tool.input.started", live(json!({"id": "tl_w", "name": "write"})));
    e(&mut a, "session.tool.called", live(json!({"id": "tl_w", "input": {"path": "b.ts", "content": "x"}})));
    e(&mut a, "permission.asked", json!({"id": "per_1", "sessionID": s, "action": "edit", "resources": ["b.ts"], "source": {"id": "tl_w"}}));
    check(&mut a, s, w, "permission");
    a.autoaccept = true;
    check(&mut a, s, w, "autoaccept");
    a.autoaccept = false;
    e(&mut a, "permission.replied", json!({"requestID": "per_1", "sessionID": s, "reply": "once"}));
    check(&mut a, s, w, "permission replied");
    // Retry countdown (time-dependent footer).
    e(&mut a, "session.retry.scheduled", live(json!({"attempt": 1, "at": t0 + 9500, "error": {"type": "x", "message": "rate limited"}})));
    check(&mut a, s, w, "retry");
    for dt in [5400, 6100, 7000, 9400, 9600] {
        a.clock = Some(t0 + dt);
        check(&mut a, s, w, &format!("retry at +{dt}"));
    }
    e(&mut a, "session.step.ended", live(json!({"finish": "stop", "cost": 0.1, "tokens": {"input": 10, "output": 20, "reasoning": 0, "cache": {"read": 0, "write": 0}}})));
    check(&mut a, s, w, "step ended");
    e(&mut a, "session.execution.succeeded", json!({"sessionID": s}));
    a.store.active.insert("ses_child".into(), false);
    check(&mut a, s, w, "idle");
    // Next turn: optimistic prompt, delivery, streaming (partial rebuilds).
    let partial = a.tcache.stats.partial;
    a.store.admit_prompt(s, "msg_zz_user2", "next question please", json!([]), "steer");
    check(&mut a, s, w, "prompt admitted");
    e(&mut a, "session.inbox.enqueued", json!({"sessionID": s, "inboxID": "msg_zz_user2", "item": {"type": "user", "payload": {"text": "next question please"}, "delivery": "steer"}}));
    check(&mut a, s, w, "enqueued");
    e(&mut a, "session.execution.started", json!({"sessionID": s}));
    e(&mut a, "session.inbox.delivered", json!({"sessionID": s, "inboxID": "msg_zz_user2"}));
    check(&mut a, s, w, "delivered");
    e(&mut a, "session.step.started", json!({"sessionID": s, "assistantMessageID": "msg_zz_live2", "agent": "build", "model": {"id": "claude-sonnet", "providerID": "anthropic"}}));
    e(&mut a, "session.text.started", json!({"sessionID": s, "assistantMessageID": "msg_zz_live2", "ordinal": 0}));
    for k in 0..10 {
        e(&mut a, "session.text.delta", json!({"sessionID": s, "assistantMessageID": "msg_zz_live2", "ordinal": 0, "delta": format!("more{k} ")}));
        check(&mut a, s, w, &format!("turn 2 delta {k}"));
    }
    assert!(a.tcache.stats.partial > partial + 5, "streaming should rebuild from the turn start: {:?}", a.tcache.stats);
    // A queued prompt is hidden; then it is cancelled.
    a.store.admit_prompt(s, "msg_zz_q", "queued one", json!([]), "queue");
    check(&mut a, s, w, "queued prompt");
    e(&mut a, "session.inbox.cancelled", json!({"sessionID": s, "inboxID": "msg_zz_q"}));
    check(&mut a, s, w, "queued cancelled");
    // Compaction and a user shell command.
    e(&mut a, "session.compaction.started", json!({"sessionID": s, "reason": "manual"}));
    check(&mut a, s, w, "compaction started");
    e(&mut a, "session.compaction.delta", json!({"sessionID": s, "text": "Summary so far"}));
    check(&mut a, s, w, "compaction delta");
    e(&mut a, "session.compaction.ended", json!({"sessionID": s, "text": "Summary so far, done", "tokens": {"input": 5, "output": 6, "reasoning": 0, "cache": {"read": 0, "write": 0}}}));
    check(&mut a, s, w, "compaction ended");
    e(&mut a, "session.shell.started", json!({"sessionID": s, "shell": {"id": "sh_2", "command": "ls -la", "status": "running"}}));
    check(&mut a, s, w, "shell started");
    e(&mut a, "session.shell.ended", json!({"sessionID": s, "shell": {"id": "sh_2", "command": "ls -la", "status": "exited", "exit": 1}, "output": {"output": "total 0", "cursor": 7, "size": 7, "truncated": false}}));
    check(&mut a, s, w, "shell ended");
    e(&mut a, "session.execution.failed", json!({"sessionID": s, "error": {"type": "x", "message": "provider down"}}));
    check(&mut a, s, w, "failed");
    // Revert boundary in the middle of the history, then cleared.
    let mid = a.store.messages(s).iter().filter(|m| m.kind == "user").nth(1).unwrap().id.clone();
    e(&mut a, "session.revert.staged", json!({"sessionID": s, "revert": {"messageID": mid, "files": []}}));
    check(&mut a, s, w, "revert staged");
    e(&mut a, "session.revert.cleared", json!({"sessionID": s}));
    check(&mut a, s, w, "revert cleared");
    // An in-place refetch of one message and an older page.
    let mut m = a.store.messages(s)[6].clone();
    m.rev = Rev(0);
    if let Some(Content::Text { text, .. }) = m.content.last_mut() {
        text.push_str(" (edited)");
    }
    a.store.messages.get_mut(s).unwrap()[6] = m;
    check(&mut a, s, w, "message replaced");
    let older: Vec<Message> = serde_json::from_value(json!([
        {"id": "msg_0_b", "type": "assistant", "agent": "build", "time": {"created": 2, "completed": 3}, "finish": "stop", "content": [{"type": "text", "text": "older answer"}]},
        {"id": "msg_0_a", "type": "user", "text": "older question", "time": {"created": 1}}
    ]))
    .unwrap();
    a.store.prepend_messages(s, older, None);
    check(&mut a, s, w, "older page");
    toggle_all(&mut a, s, w, "history");
    // Settings that affect every row.
    for width in [60, 140, 100] {
        check(&mut a, s, width, &format!("width {width}"));
    }
    a.size = (90, 40);
    check(&mut a, s, w, "terminal width");
    for (k, v) in [
        ("session.verbosity", json!("low")),
        ("session.verbosity", json!("high")),
        ("session.thinking", json!("show")),
        ("debug.turn_tokens", json!(true)),
        ("session.tps", json!(false)),
        ("session.grouping", json!("none")),
    ] {
        a.config.set(k, Some(v));
        check(&mut a, s, w, &format!("config {k}"));
    }
    // Streaming with turn tokens on (a restart needs an idle marker right before the turn).
    e(&mut a, "session.execution.started", json!({"sessionID": s}));
    e(&mut a, "session.step.started", json!({"sessionID": s, "assistantMessageID": "msg_zz_live3", "agent": "plan", "model": {"id": "claude-sonnet", "providerID": "anthropic"}}));
    e(&mut a, "session.text.started", json!({"sessionID": s, "assistantMessageID": "msg_zz_live3", "ordinal": 0}));
    for k in 0..5 {
        e(&mut a, "session.text.delta", json!({"sessionID": s, "assistantMessageID": "msg_zz_live3", "ordinal": 0, "delta": format!("x{k} ")}));
        check(&mut a, s, w, &format!("turn 3 delta {k}"));
    }
    let names = a.themes.names();
    let other = names.iter().find(|n| **n != a.themes.active).cloned().unwrap();
    a.set_theme(&other, false);
    check(&mut a, s, w, "theme");
}

/// After one text delta only the streaming part is rendered again.
#[test]
fn delta_renders_one_row() {
    let mut a = perf_app();
    let sid = long_session(&mut a, 1500);
    a.tcache.budget = Some(std::time::Duration::ZERO);
    start_stream(&mut a, &sid);
    a.on_event(delta(&sid, 0));
    a.transcript_sync_need(&sid, 100, Some(40));
    for k in 1..20 {
        let before = a.tcache.stats;
        a.on_event(delta(&sid, k));
        a.transcript_sync_need(&sid, 100, Some(40));
        let after = a.tcache.stats;
        assert_eq!(after.full, before.full, "no full rebuild");
        assert!(after.rendered - before.rendered <= 1, "rendered {} rows", after.rendered - before.rendered);
    }
    // A frame without changes renders nothing.
    let before = a.tcache.stats.rendered;
    a.transcript_sync_need(&sid, 100, Some(40));
    assert_eq!(a.tcache.stats.rendered, before);
}

/// While rows above the window are still unrendered, the window (addressed from the bottom) is
/// exactly the reference's, and the final result is too.
#[test]
fn lazy_window_matches_reference() {
    let mut a = perf_app();
    a.clock = Some(2_000_000_000_000);
    let sid = long_session(&mut a, 24);
    let s = sid.as_str();
    let w = 90;
    a.tcache.budget = Some(std::time::Duration::ZERO);
    let (lines, marks) = a.render_transcript_lines(s, w);
    let n = lines.len();
    for (fb, h) in [(0usize, 30usize), (37, 20), (n / 2, 25), (n - 10, 40)] {
        a.transcript_sync_need(s, w, Some(fb + h));
        let win = a.tcache.window_bottom(fb, h, 0);
        let end = n - fb;
        let expect: Vec<&SLine> = lines[end.saturating_sub(h)..end].iter().collect();
        assert_eq!(win, expect, "window fb={fb} h={h}");
    }
    // Slack lines sit below the transcript.
    let win = a.tcache.window_bottom(0, 5, 2);
    assert_eq!(win.len(), 5);
    assert_eq!(win[3], &SLine::blank());
    assert_eq!(win[2], &lines[n - 1]);
    // Completing yields the reference.
    a.tcache.budget = None;
    a.transcript_sync(s, w);
    assert!(a.tcache.is_complete());
    assert_eq!(a.tcache.to_vec(), lines);
    assert_eq!(a.tcache.marks(), &marks[..]);
}

/// A drawn frame is the same whether the rows above the window are rendered yet or not.
#[test]
fn lazy_frame_is_pixel_identical() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut a = perf_app();
    a.clock = Some(2_000_000_000_000);
    a.config.set("animations", Some(json!(false)));
    let sid = long_session(&mut a, 24);
    a.tcache.budget = Some(std::time::Duration::ZERO);
    let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
    term.draw(|f| a.draw(f)).unwrap();
    assert!(!a.tcache.is_complete(), "first frame should leave rows for later");
    let lazy = term.backend().buffer().clone();
    a.transcript_complete();
    a.dirty = true;
    term.draw(|f| a.draw(f)).unwrap();
    assert_eq!(&lazy, term.backend().buffer());
    // Scrolled up: the same frame from a complete cache and from a fresh lazy one.
    a.view(&sid).scroll_from_bottom = 57;
    term.draw(|f| a.draw(f)).unwrap();
    let complete = term.backend().buffer().clone();
    a.tcache = Default::default();
    a.tcache.budget = Some(std::time::Duration::ZERO);
    term.draw(|f| a.draw(f)).unwrap();
    assert!(!a.tcache.is_complete());
    assert_eq!(&complete, term.backend().buffer());
}
