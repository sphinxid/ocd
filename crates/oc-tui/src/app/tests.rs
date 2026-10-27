//! Snapshot-style tests of key screens using ratatui's `TestBackend`.

use super::*;
use crate::api::types::FileDiff;
use crate::keymap::Stroke;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::json;

pub(super) fn app() -> App {
    let opts = TuiOptions {
        url: "http://127.0.0.1:1".into(),
        directory: "/work/proj".into(),
        version: "2.0.26".into(),
        channel: "test".into(),
        ..Default::default()
    };
    let mut cfg = crate::config::Config::from_value(json!({ "tabs": { "mode": "off" }, "animations": false }));
    cfg.terminal_suspend = true;
    let mut app = App::headless(opts, cfg);
    // Isolate from the user's state files.
    app.history = crate::state::History::default();
    app.stash = crate::state::Stash::default();
    app.models = crate::state::ModelPrefs::default();
    app.pins = crate::state::SessionPins::default();
    app.keyed = crate::state::KeyedStore { dir: std::env::temp_dir().join(format!("oc-tui-keyed-{}", crate::util::id_body(false))) };
    app.placeholder = 1;
    app.connection = crate::api::sse::ConnStatus::Connected;
    let loc = app.store.location_mut("/work/proj");
    loc.agents = Some(vec![
        AgentInfo { id: "build".into(), name: "build".into(), mode: "primary".into(), ..Default::default() },
        AgentInfo { id: "plan".into(), name: "plan".into(), mode: "primary".into(), ..Default::default() },
        AgentInfo { id: "explore".into(), name: "explore".into(), mode: "subagent".into(), ..Default::default() },
    ]);
    loc.models = Some(vec![ModelInfo {
        id: "claude-sonnet".into(),
        provider_id: "anthropic".into(),
        name: "Claude Sonnet 4.5".into(),
        variants: vec![ModelVariant { id: "high".into() }],
        limit: ModelLimit { context: 200000.0, output: 8000.0 },
        status: "active".into(),
        enabled: true,
        ..Default::default()
    }]);
    loc.providers = Some(vec![ProviderInfo { id: "anthropic".into(), name: "Anthropic".into(), ..Default::default() }]);
    app.default_model = Some(ModelRef { id: "claude-sonnet".into(), provider_id: "anthropic".into(), variant: None });
    app
}

pub(super) fn render(app: &mut App, w: u16, h: u16) -> Vec<String> {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| app.draw(f)).unwrap();
    let buf = term.backend().buffer().clone();
    (0..h)
        .map(|y| {
            let mut line = String::new();
            for x in 0..w {
                line.push_str(buf[(x, y)].symbol());
            }
            line.trim_end().to_string()
        })
        .collect()
}

fn contains(lines: &[String], needle: &str) -> bool {
    lines.iter().any(|l| l.contains(needle))
}

pub(super) fn session(app: &mut App) -> String {
    let id = "ses_test".to_string();
    app.store.remember(SessionInfo {
        id: id.clone(),
        title: Some("Fix parser bug".into()),
        agent: Some("build".into()),
        model: Some(ModelRef { id: "claude-sonnet".into(), provider_id: "anthropic".into(), variant: None }),
        location: LocationRef { directory: "/work/proj".into(), workspace_id: None },
        cost: 0.12,
        ..Default::default()
    });
    let msgs: Vec<Message> = serde_json::from_value(json!([
        {"id":"msg_1","type":"user","text":"Fix the bug in parser.ts","time":{"created":1000},
         "files":[{"name":"src/parser.ts","mime":"text/plain","data":"","source":{"type":"uri","uri":"file:///work/proj/src/parser.ts"}}]},
        {"id":"msg_2","type":"assistant","agent":"build","model":{"id":"claude-sonnet","providerID":"anthropic"},
         "time":{"created":1100,"streamed":1200,"completed":73100},"finish":"stop",
         "tokens":{"input":12000,"output":300,"reasoning":0,"cache":{"read":0,"write":0}},
         "content":[
            {"type":"reasoning","text":"**Inspecting parser**\n\nLooking at it","time":{"created":1100,"completed":3500}},
            {"type":"tool","id":"t1","name":"read","state":{"status":"completed","input":{"path":"/work/proj/src/parser.ts"},"content":[{"type":"text","text":"x"}]},"time":{"created":1200,"completed":1300}},
            {"type":"tool","id":"t2","name":"grep","state":{"status":"completed","input":{"pattern":"parse"},"metadata":{"matches":3},"content":[{"type":"text","text":"x"}]},"time":{"created":1300,"completed":1400}},
            {"type":"tool","id":"t3","name":"bash","state":{"status":"completed","input":{"command":"bun test"},"content":[{"type":"text","text":"3 pass\n0 fail"}]},"time":{"created":1400,"completed":1500}},
            {"type":"tool","id":"t4","name":"edit","state":{"status":"completed","input":{"path":"src/parser.ts"},
              "metadata":{"files":[{"file":"src/parser.ts","status":"modified","additions":1,"deletions":1,"patch":"@@ -1,3 +1,3 @@\n a\n-b\n+c\n d\n"}]},
              "content":[{"type":"text","text":"ok"}]},"time":{"created":1500,"completed":1600}},
            {"type":"tool","id":"t5","name":"todowrite","state":{"status":"completed","input":{"count":2},"content":[{"type":"text","text":"ok"}]},"time":{"created":1600,"completed":1700}},
            {"type":"text","text":"I fixed the **bug**.\n\n- updated parser\n- added test"}
         ]},
        {"id":"msg_3","type":"idle","outcome":"succeeded","time":{"created":73200}}
    ]))
    .unwrap();
    app.store.messages.insert(id.clone(), msgs);
    app.store.loaded.insert(format!("session.message:{id}"));
    app.store.loaded.insert(format!("session.pending:{id}"));
    app.route = Route::Session(id.clone());
    id
}

#[test]
fn home_wide_logo_and_prompt() {
    let mut a = app();
    let lines = render(&mut a, 100, 30);
    let joined = lines.join("\n");
    // "opencode-d": "open" + "code" + hyphen + "d".
    assert!(joined.contains("█▀▀█ █▀▀█ █▀▀█ █▀▀▄ █▀▀▀ █▀▀█ █▀▀█ █▀▀█     █▀▀█"), "{joined}");
    assert!(joined.contains("▀▀▀▀ █▀▀▀ ▀▀▀▀ ▀▀▀▀ ▀▀▀▀ ▀▀▀▀ ▀▀▀▀ ▀▀▀▀     ▀▀▀▀"));
    assert!(joined.contains("█  █ █  █ █▀▀▀ █  █ █    █  █ █  █ █▀▀▀ ▀▀▀ █  █"), "{joined}");
    assert!(contains(&lines, "┃  Ask anything… \"What is the tech stack of this project?\""), "{joined}");
    assert!(contains(&lines, "┃  Build · Claude Sonnet 4.5 Anthropic"), "{joined}");
    assert!(contains(&lines, "╹▀▀▀"));
    assert!(contains(&lines, "shift+tab agents  ctrl+p commands"), "{joined}");
    assert!(contains(&lines, "2.0.26"));
    // Logo is horizontally centered: 48 wide in 100 columns.
    let row = lines.iter().find(|l| l.contains("█▀▀█ █▀▀█")).unwrap();
    assert_eq!(row.chars().position(|c| c == '█').unwrap(), (100 - 48) / 2);
}

#[test]
fn home_narrow_stacked_logo() {
    let mut a = app();
    let lines = render(&mut a, 40, 24);
    let joined = lines.join("\n");
    assert!(joined.contains("█▀▀█ █▀▀█ █▀▀█ █▀▀▄\n"), "{joined}");
    assert!(joined.contains("▀▀▀▀ ▀▀▀▀ ▀▀▀▀ ▀▀▀▀     ▀▀▀▀"), "{joined}");
    // Narrow: agent label and provider hidden.
    assert!(contains(&lines, "┃ Claude Sonnet 4.5"), "{joined}");
    assert!(!contains(&lines, "shift+tab agents"));
}

#[test]
fn home_tiny_go_logo() {
    let mut a = app();
    let lines = render(&mut a, 20, 14);
    assert!(contains(&lines, "█▀▀█"));
    assert!(!contains(&lines, "█▀▀▄"));
}

#[test]
fn session_transcript_narrow() {
    let mut a = app();
    session(&mut a);
    let lines = render(&mut a, 80, 60);
    let joined = lines.join("\n");
    assert!(contains(&lines, "┃  Fix the bug in parser.ts"), "{joined}");
    assert!(contains(&lines, " file  src/parser.ts"), "{joined}");
    assert!(contains(&lines, "+ Thought: Inspecting parser · 2.4s"), "{joined}");
    assert!(contains(&lines, "→ Explored: 1 read, 1 search"), "{joined}");
    assert!(contains(&lines, "┃  $ bun test"), "{joined}");
    assert!(contains(&lines, "┃  3 pass"), "{joined}");
    assert!(contains(&lines, "┃  ← Edit src/parser.ts"), "{joined}");
    assert!(contains(&lines, "2 - b"), "{joined}");
    assert!(contains(&lines, "2 + c"), "{joined}");
    assert!(contains(&lines, "✓ todowrite [count=2]"), "{joined}");
    assert!(contains(&lines, "I fixed the bug."), "{joined}");
    assert!(contains(&lines, "- updated parser"), "{joined}");
    assert!(contains(&lines, "Build · Claude Sonnet 4.5 · 1m 12s · 3000.0 tok/s"), "{joined}");
    // Session prompt shows no placeholder and the usage in the footer.
    assert!(contains(&lines, "12.3K (6%) · $0.12"), "{joined}");
    // No sidebar at 80 columns.
    assert!(!contains(&lines, "Fix parser bug"));
}

#[test]
fn session_wide_with_sidebar_and_split_diff() {
    let mut a = app();
    session(&mut a);
    let lines = render(&mut a, 170, 70);
    let joined = lines.join("\n");
    assert!(contains(&lines, "Fix parser bug"), "{joined}");
    assert!(contains(&lines, "Context"), "{joined}");
    assert!(contains(&lines, "12,300 tokens"), "{joined}");
    assert!(contains(&lines, "6% used"));
    assert!(contains(&lines, "$0.12 spent"));
    // Split diff: both sides on one row.
    let row = lines.iter().find(|l| l.contains("- b")).unwrap();
    assert!(row.contains("+ c"), "{row}");
}

#[test]
fn thinking_show_mode_and_verbosity_low() {
    let mut a = app();
    session(&mut a);
    a.config.set("session.thinking", Some(json!("show")));
    let lines = render(&mut a, 80, 60);
    assert!(contains(&lines, "┃ Thought: 2.4s"), "{}", lines.join("\n"));
    a.config.set("session.thinking", Some(json!("hide")));
    a.config.set("session.verbosity", Some(json!("low")));
    let lines = render(&mut a, 80, 60);
    assert!(contains(&lines, "+ 1 command, 1 edit, 1 thought, 1 read, 2 tools"), "{}", lines.join("\n"));
}

#[test]
fn palette_dialog() {
    let mut a = app();
    a.on_key(Stroke::ctrl("p"));
    assert_eq!(a.dialogs.len(), 1);
    let lines = render(&mut a, 100, 40);
    let joined = lines.join("\n");
    assert!(contains(&lines, "Commands"), "{joined}");
    assert!(contains(&lines, "esc"));
    assert!(contains(&lines, "Search"));
    assert!(contains(&lines, "Suggested"));
    assert!(contains(&lines, "Switch model"), "{joined}");
    assert!(contains(&lines, "ctrl+x m"), "{joined}");
    // Title at x = dialog left + 4.
    let row = lines.iter().find(|l| l.contains("Commands")).unwrap();
    assert_eq!(row.find("Commands").unwrap(), (100 - 60) / 2 + 4);
    // Typing filters.
    for c in "theme".chars() {
        a.on_key(Stroke::key(&c.to_string()));
    }
    let lines = render(&mut a, 100, 40);
    assert!(contains(&lines, "Switch theme"), "{}", lines.join("\n"));
    a.on_key(Stroke::key("escape"));
    assert!(a.dialogs.is_empty());
}

#[test]
fn leader_sequences() {
    let mut a = app();
    session(&mut a);
    a.on_key(Stroke::ctrl("x"));
    assert_eq!(a.pending_keys.len(), 1);
    a.on_key(Stroke::key("n"));
    assert_eq!(a.route, Route::Home);
    assert!(a.pending_keys.is_empty());
    // Leader then an unbound key: typed into the prompt.
    a.on_key(Stroke::ctrl("x"));
    a.on_key(Stroke::key("z"));
    assert_eq!(a.editor.text, "z");
    // Leader then escape clears without typing.
    a.on_key(Stroke::ctrl("x"));
    a.on_key(Stroke::key("escape"));
    assert!(a.pending_keys.is_empty());
    assert_eq!(a.editor.text, "z");
}

/// A hand-edited `leader.timeout` too large for `Instant` used to panic on the leader key.
#[test]
fn huge_leader_timeout_does_not_overflow() {
    let mut a = app();
    a.config.set("leader", Some(json!({ "timeout": 1e300 })));
    a.keybinds = a.config.keybinds();
    a.on_key(Stroke::ctrl("x"));
    assert_eq!(a.pending_keys.len(), 1);
    a.tick();
    assert_eq!(a.pending_keys.len(), 1);
    a.on_key(Stroke::key("escape"));
    assert!(a.pending_keys.is_empty());
}

/// Agent colors come from the server's agent catalog and are parsed on every frame (prompt bar);
/// a non-ASCII value used to panic in the hex parser in every attached TUI at once.
#[test]
fn non_ascii_agent_color_renders() {
    let mut a = app();
    session(&mut a);
    if let Some(agents) = a.store.location_mut("/work/proj").agents.as_mut() {
        agents[0].color = Some("#aé1234".into());
    }
    let lines = render(&mut a, 100, 30);
    assert!(contains(&lines, "Build"), "{}", lines.join("\n"));
}

#[test]
fn prompt_typing_shell_mode_and_clear() {
    let mut a = app();
    a.on_key(Stroke::key("!"));
    assert!(a.editor.shell);
    let lines = render(&mut a, 100, 30);
    assert!(contains(&lines, "Run a command… \""), "{}", lines.join("\n"));
    assert!(contains(&lines, "esc exit shell mode"));
    assert!(contains(&lines, "Shell"));
    a.on_key(Stroke::key("escape"));
    assert!(!a.editor.shell);
    for c in "hello".chars() {
        a.on_key(Stroke::key(&c.to_string()));
    }
    assert_eq!(a.editor.text, "hello");
    a.on_key(Stroke::ctrl("c"));
    assert_eq!(a.editor.text, "");
    // ctrl+c on an empty prompt exits.
    a.on_key(Stroke::ctrl("c"));
    assert!(a.quit);
}

#[test]
fn slash_autocomplete() {
    let mut a = app();
    a.on_key(Stroke::key("/"));
    let ac = a.autocomplete.clone().expect("autocomplete open");
    assert_eq!(ac.mode, "command");
    assert!(ac.items.iter().any(|i| i.display.trim() == "/models"));
    for c in "them".chars() {
        a.on_key(Stroke::key(&c.to_string()));
    }
    let ac = a.autocomplete.clone().unwrap();
    assert_eq!(ac.items[0].display.trim(), "/themes");
    let lines = render(&mut a, 100, 30);
    assert!(contains(&lines, "/themes"), "{}", lines.join("\n"));
    a.on_key(Stroke::key("return"));
    assert!(a.autocomplete.is_none());
    assert_eq!(a.dialogs.last().map(|d| d.purpose.as_str()), Some("theme"));
}

#[test]
fn permission_prompt() {
    let mut a = app();
    let sid = session(&mut a);
    a.store.permissions.insert(
        sid.clone(),
        vec![PermissionRequest {
            id: "per_1".into(),
            session_id: sid.clone(),
            action: "bash".into(),
            resources: vec!["rm -rf build".into()],
            save: Some(vec!["rm *".into()]),
            metadata: Some(json!({"command": "rm -rf build"})),
            ..Default::default()
        }],
    );
    let lines = render(&mut a, 100, 50);
    let joined = lines.join("\n");
    assert!(contains(&lines, "△ Permission required"), "{joined}");
    assert!(contains(&lines, "$ rm -rf build"), "{joined}");
    assert!(contains(&lines, " Allow once  Always allow  Reject "), "{joined}");
    assert!(contains(&lines, "ctrl+f fullscreen  ⇆ select  enter confirm"), "{joined}");
    a.on_key(Stroke::key("right"));
    let lines = render(&mut a, 100, 50);
    assert!(contains(&lines, "This will always allow the following patterns for this project."), "{}", lines.join("\n"));
    assert!(contains(&lines, "- rm *"));
}

#[test]
fn form_prompt() {
    let mut a = app();
    let sid = session(&mut a);
    a.store.forms.insert(
        sid.clone(),
        vec![(
            FormInfo {
                id: "frm_1".into(),
                session_id: sid.clone(),
                title: "Setup".into(),
                fields: vec![
                    json!({"key":"lang","type":"string","title":"Language","description":"Which language?","options":[{"value":"ts","label":"TypeScript"},{"value":"rs","label":"Rust"}],"custom":true}),
                    json!({"key":"tests","type":"boolean","title":"Tests"}),
                ],
                ..Default::default()
            },
            None,
        )],
    );
    let lines = render(&mut a, 100, 50);
    let joined = lines.join("\n");
    assert!(contains(&lines, "Setup"), "{joined}");
    assert!(contains(&lines, "Language  Tests  Submit"), "{joined}");
    assert!(contains(&lines, "Which language?"));
    assert!(contains(&lines, "1. TypeScript"));
    assert!(contains(&lines, "3. Type your own answer"));
    assert!(contains(&lines, "⇆ tab  ↑↓ select  enter confirm  esc dismiss"), "{joined}");
    a.on_key(Stroke::key("2"));
    let ui = a.form_ui.clone().unwrap();
    assert_eq!(ui.answers.get("lang"), Some(&json!("rs")));
    assert_eq!(ui.tab, 1);
}

#[test]
fn toast_rendering() {
    let mut a = app();
    a.toast(Toast::new("error", "Something failed badly").title("Failed to load theme: x"));
    let lines = render(&mut a, 100, 30);
    let joined = lines.join("\n");
    assert!(contains(&lines, "Failed to load theme: x"), "{joined}");
    assert!(contains(&lines, "Something failed badly"));
    let row = lines.iter().find(|l| l.contains("Something failed")).unwrap();
    assert!(row.trim_end().ends_with('┃'));
}

#[test]
fn model_and_settings_dialogs() {
    let mut a = app();
    a.run_command("model.list", None);
    let lines = render(&mut a, 100, 40);
    let joined = lines.join("\n");
    assert!(contains(&lines, "Select model"), "{joined}");
    assert!(contains(&lines, "Anthropic"));
    assert!(contains(&lines, "● Claude Sonnet 4.5") || contains(&lines, "●    Claude Sonnet 4.5"), "{joined}");
    a.close_dialog();
    a.run_command("opencode.settings", None);
    let lines = render(&mut a, 100, 60);
    let joined = lines.join("\n");
    assert!(contains(&lines, "Settings"), "{joined}");
    assert!(contains(&lines, "Appearance"));
    assert!(contains(&lines, "change ←/→"), "{joined}");
}

#[test]
fn streaming_events_update_view() {
    let mut a = app();
    let sid = session(&mut a);
    let ev = |kind: &str, data: serde_json::Value| Event { id: format!("evt_{}", crate::util::id_body(false)), created: 80000, kind: kind.into(), data, ..Default::default() };
    a.on_event(ev("session.execution.started", json!({"sessionID": sid})));
    a.on_event(ev("session.step.started", json!({"sessionID": sid, "assistantMessageID": "msg_9", "agent": "build", "model": {"id":"claude-sonnet","providerID":"anthropic"}, "started": 80000})));
    a.on_event(ev("session.text.started", json!({"sessionID": sid, "assistantMessageID": "msg_9", "ordinal": 0})));
    a.on_event(ev("session.text.delta", json!({"sessionID": sid, "assistantMessageID": "msg_9", "ordinal": 0, "delta": "Streaming reply"})));
    let lines = render(&mut a, 80, 60);
    assert!(contains(&lines, "Streaming reply"), "{}", lines.join("\n"));
    assert!(contains(&lines, "esc interrupt"), "{}", lines.join("\n"));
    a.on_key(Stroke::key("escape"));
    let lines = render(&mut a, 80, 60);
    assert!(contains(&lines, "esc again to interrupt"));
}

#[test]
fn vertical_rail_and_tab_menu() {
    let mut a = app();
    a.config.set("tabs", Some(json!({"mode": "on", "layout": "vertical"})));
    let sid = session(&mut a);
    a.tabs = vec![sid.clone()];
    let lines = render(&mut a, 140, 70);
    let joined = lines.join("\n");
    assert!(lines[1].contains("Fix parser bug"), "{joined}");
    assert!(lines[2].contains("proj"), "{joined}");
    assert!(lines[4].contains("+ New session"), "{joined}");
    // Content starts right of the 42-column rail.
    let row = lines.iter().find(|l| l.contains("Fix the bug in parser.ts")).unwrap_or_else(|| panic!("{joined}"));
    assert!(row.find("Fix the bug").unwrap() > 42);
    // Right click opens the context menu.
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    a.on_mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Right), column: 5, row: 1, modifiers: KeyModifiers::NONE });
    assert!(a.tab_menu.is_some());
    let lines = render(&mut a, 140, 70);
    assert!(contains(&lines, "Copy session ID"), "{}", lines.join("\n"));
    assert!(contains(&lines, "Rename"));
    a.on_key(Stroke::key("escape"));
    assert!(a.tab_menu.is_none());
    // Compact rail.
    a.rail_pref = 8;
    let lines = render(&mut a, 140, 70);
    assert!(lines[1].contains('⌕'), "{}", lines.join("\n"));
}

#[test]
fn diff_viewer_route() {
    let mut a = app();
    session(&mut a);
    // Inside a session /diff opens the side panel; the route stays on the session.
    a.run_command("diff.open", None);
    assert_eq!(a.route, Route::Session("ses_test".into()));
    assert!(a.active_panel().is_some());
    let d = a.diff.as_mut().unwrap();
    d.loading = false;
    d.base = Some("main".into());
    d.files = vec![
        FileDiff { file: "src/a.ts".into(), patch: "@@ -1,2 +1,2 @@\n-x\n+y\n z\n".into(), additions: 1, deletions: 1, status: Some("modified".into()) },
        FileDiff { file: "README.md".into(), patch: "@@ -0,0 +1 @@\n+hello\n".into(), additions: 1, deletions: 0, status: Some("added".into()) },
    ];
    let lines = render(&mut a, 140, 40);
    let joined = lines.join("\n");
    assert!(contains(&lines, "All · vs main"), "{joined}");
    assert!(contains(&lines, "0/2"), "{joined}");
    assert!(contains(&lines, "src/a.ts"), "{joined}");
    assert!(contains(&lines, "+1 -1"), "{joined}");
    assert!(contains(&lines, "+ hello"), "{joined}");
    a.on_key(Stroke::key("m"));
    let lines = render(&mut a, 140, 40);
    assert!(contains(&lines, "1/2"), "{}", lines.join("\n"));
    a.on_key(Stroke::key("g"));
    a.on_key(Stroke::key("g"));
    assert_eq!(a.diff.as_ref().unwrap().scroll, 0);
    a.on_key(Stroke::key("q"));
    assert_eq!(a.route, Route::Session("ses_test".into()));
    assert!(a.panel.is_none());
    // Narrow: header line instead of tree.
    a.run_command("diff.open", None);
    a.diff.as_mut().unwrap().loading = false;
    let lines = render(&mut a, 80, 20);
    assert!(contains(&lines, "No changes to show"), "{}", lines.join("\n"));
}

#[test]
fn stats_route() {
    let mut a = app();
    a.run_command("stats.open", None);
    let lines = render(&mut a, 100, 40);
    assert!(contains(&lines, "Gathering your stats…"));
    a.stats.as_mut().unwrap().data = Some(json!({
        "range": {"from": 1767225600000i64, "to": 1770000000000i64},
        "sessions": 12, "subagents": 0, "prompts": 30, "steps": 50,
        "tokens": {"input": 1200000, "output": 30000, "reasoning": 0, "cache": {"read": 0, "write": 0}},
        "cost": 1.5, "tools": {"mode": "none"}, "activeDays": 9, "streak": 4,
        "activity": [{"date": "2026-01-05", "steps": 3}], "models": []
    }));
    let lines = render(&mut a, 120, 40);
    let joined = lines.join("\n");
    assert!(contains(&lines, "ocd / stats"), "{joined}");
    assert!(contains(&lines, "TOKENS"));
    assert!(contains(&lines, "4 days"));
    assert!(contains(&lines, "active days"));
    assert!(contains(&lines, "opencode.ai"));
    a.on_key(Stroke::key("escape"));
    assert_eq!(a.route, Route::Home);
}

#[test]
fn composer_drawer_and_message_navigation() {
    let mut a = app();
    let sid = session(&mut a);
    a.store.remember(SessionInfo { id: "ses_child".into(), parent_id: Some(sid.clone()), agent: Some("explore".into()), title: Some("find config loader".into()), ..Default::default() });
    a.store.active.insert("ses_child".into(), true);
    a.on_key(Stroke::key("down"));
    assert!(a.composer.is_some());
    let lines = render(&mut a, 100, 60);
    let joined = lines.join("\n");
    assert!(contains(&lines, "Subagents  Shell"), "{joined}");
    assert!(contains(&lines, "Explore: find config loader"), "{joined}");
    assert!(contains(&lines, "Running"), "{joined}");
    assert!(contains(&lines, "interrupt ctrl+d  show inactive ctrl+a  tabs ←/→"), "{joined}");
    a.on_key(Stroke::key("escape"));
    assert!(a.composer.is_none());
    // Message navigation: previous user message brings it to the top.
    let _ = render(&mut a, 100, 20);
    assert!(a.navigate_message("session.message.user.previous"));
    let lines = render(&mut a, 100, 20);
    assert!(lines[1].contains("Fix the bug in parser.ts"), "{}", lines.join("\n"));
}

#[test]
fn export_options_dialog() {
    let mut a = app();
    session(&mut a);
    a.run_command("session.export", None);
    let lines = render(&mut a, 100, 40);
    let joined = lines.join("\n");
    assert!(contains(&lines, "Export session"), "{joined}");
    assert!(contains(&lines, "Export as:  ◉ Markdown   ○ JSON"), "{joined}");
    assert!(contains(&lines, "[x] Include thinking"));
    assert!(contains(&lines, "Copy") && contains(&lines, "Export"));
    a.on_key(Stroke::key("tab"));
    a.on_key(Stroke::key("return"));
    let lines = render(&mut a, 100, 40);
    assert!(contains(&lines, "[ ] Sanitize sensitive data"), "{}", lines.join("\n"));
}

#[test]
fn open_worktrees_and_pair_dialogs() {
    let mut a = app();
    a.store.remember(SessionInfo { id: "ses_other".into(), title: Some("Refactor tabs".into()), location: LocationRef { directory: "/work/proj".into(), workspace_id: None }, ..Default::default() });
    a.run_command("open.menu", None);
    a.refresh_open_options(vec![json!({ "id": "prj_1", "canonical": "/work/other", "name": "other", "vcs": "git" })]);
    let lines = render(&mut a, 100, 40);
    let joined = lines.join("\n");
    assert!(contains(&lines, "Open"), "{joined}");
    assert!(contains(&lines, "Search sessions and projects…"), "{joined}");
    assert!(contains(&lines, "Sessions") && contains(&lines, "Projects"), "{joined}");
    assert!(contains(&lines, "Refactor tabs"), "{joined}");
    assert!(contains(&lines, "/work/other →"), "{joined}");
    // Right on a git project opens its worktrees view; left goes back.
    for c in "other".chars() {
        a.on_key(Stroke::key(&c.to_string()));
    }
    a.on_key(Stroke::key("right"));
    assert_eq!(a.dialogs.last().unwrap().purpose, "open-worktrees");
    let lines = render(&mut a, 100, 40);
    assert!(contains(&lines, "other / Worktrees"), "{}", lines.join("\n"));
    a.on_key(Stroke::ctrl("n"));
    let lines = render(&mut a, 100, 40);
    let joined = lines.join("\n");
    assert!(contains(&lines, "other / New worktree"), "{joined}");
    assert!(contains(&lines, "Leave blank for a random name."), "{joined}");
    a.on_key(Stroke::key("tab"));
    let generated = match &a.dialogs.last().unwrap().body {
        crate::ui::dialog::Body::Prompt(p) => p.value.clone(),
        _ => panic!(),
    };
    assert!(generated.contains('-'));
    a.dialogs.clear();

    // Worktrees dialog without project info reports an error toast.
    a.run_command("session.move", None);
    assert!(a.dialogs.is_empty());
    a.store.location_mut("/work/proj").info = Some(LocationInfo { directory: "/work/proj".into(), project: ProjectRef { id: "prj_1".into(), canonical: "/work/proj".into(), ..Default::default() } });
    a.run_command("session.move", None);
    let lines = render(&mut a, 140, 40);
    let joined = lines.join("\n");
    assert!(contains(&lines, "Worktrees"), "{joined}");
    assert!(contains(&lines, "Loading worktrees…"), "{joined}");
    assert!(contains(&lines, "new ctrl+a"), "{joined}");
    a.dialogs.clear();

    // Pair dialog.
    a.run_command("server.pair", None);
    let lines = render(&mut a, 100, 40);
    assert!(contains(&lines, "Pair"), "{}", lines.join("\n"));
    assert!(contains(&lines, "Loading server information…"));
}

#[test]
fn mouse_drag_select_copies() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    let mk = |kind, column, row| MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE };
    let mut a = app();
    let lines = render(&mut a, 100, 30);
    let (row, line) = lines.iter().enumerate().find(|(_, l)| l.contains("ctrl+p commands")).map(|(i, l)| (i as u16, l.clone())).unwrap();
    let col = line.find("ctrl+p").unwrap() as u16;
    crate::clipboard::TEST_CLIPBOARD.with(|c| *c.borrow_mut() = None);
    a.on_mouse(mk(MouseEventKind::Down(MouseButton::Left), col, row));
    a.on_mouse(mk(MouseEventKind::Drag(MouseButton::Left), col + 5, row));
    let _ = render(&mut a, 100, 30);
    a.on_mouse(mk(MouseEventKind::Up(MouseButton::Left), col + 5, row));
    let copied = crate::clipboard::TEST_CLIPBOARD.with(|c| c.borrow().clone());
    assert_eq!(copied.as_deref(), Some("ctrl+p"));
    assert_eq!(a.toast.as_ref().map(|t| t.message.as_str()), Some("Copied to clipboard"));
    assert!(a.selection.is_some());
    // escape clears the selection (consumed because it had text).
    a.on_key(Stroke::key("escape"));
    assert!(a.selection.is_none());
    // Plain click: no copy.
    crate::clipboard::TEST_CLIPBOARD.with(|c| *c.borrow_mut() = None);
    a.on_mouse(mk(MouseEventKind::Down(MouseButton::Left), col, row));
    a.on_mouse(mk(MouseEventKind::Up(MouseButton::Left), col, row));
    assert!(crate::clipboard::TEST_CLIPBOARD.with(|c| c.borrow().is_none()));
    // Manual mode: right-click copies.
    a.config = crate::config::Config::from_value(json!({ "tabs": { "mode": "off" }, "terminal": { "copy": "manual" } }));
    a.on_mouse(mk(MouseEventKind::Down(MouseButton::Left), col, row));
    a.on_mouse(mk(MouseEventKind::Drag(MouseButton::Left), col + 5, row));
    a.on_mouse(mk(MouseEventKind::Up(MouseButton::Left), col + 5, row));
    assert!(crate::clipboard::TEST_CLIPBOARD.with(|c| c.borrow().is_none()));
    a.on_mouse(mk(MouseEventKind::Down(MouseButton::Right), col, row));
    assert_eq!(crate::clipboard::TEST_CLIPBOARD.with(|c| c.borrow().clone()).as_deref(), Some("ctrl+p"));
}

#[test]
fn mini_scrollback_and_footer() {
    use super::mini::{MiniState, lines_to_ansi, plain, row_to_ansi};
    let mut a = app();
    a.mini = true;
    a.size = (100, 30);
    let header = plain(&[a.mini_header(100)]).join("");
    assert!(header.starts_with("▪ oc mini v2.0.26 · /work/proj"), "{header}");
    session(&mut a);
    // New (not resumed) session: the whole finished transcript is committed once.
    let mut st = MiniState { replay: true, replay_limit: 200, ..Default::default() };
    let (commit, tail) = a.mini_transcript(&mut st, 100);
    let text = plain(&commit).join("\n");
    assert!(text.contains("Fix the bug in parser.ts"), "{text}");
    assert!(text.contains("I fixed the bug."), "{text}");
    assert!(tail.is_empty());
    let (again, _) = a.mini_transcript(&mut st, 100);
    assert!(again.is_empty());
    // Footer is the prompt dock only.
    let (buf, cursor) = a.mini_footer(100, 29, &tail);
    let rows: Vec<String> = (0..buf.area.height).map(|y| (0..100).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>().trim_end().to_string()).collect();
    assert!(rows.iter().any(|r| r.contains("Build · Claude Sonnet 4.5")), "{rows:?}");
    assert!(buf.area.height < 12, "{}", buf.area.height);
    assert!(cursor.is_some());
    assert!(row_to_ansi(&buf, 0).contains("\x1b["));
    // Resumed with --no-replay: history is skipped.
    let mut st = MiniState { replay: false, replay_limit: 200, resumed: true, ..Default::default() };
    a.store.message_cursor.insert("ses_test".into(), None);
    let (commit, _) = a.mini_transcript(&mut st, 100);
    assert!(commit.is_empty());
    // Exit banner.
    let banner = plain(&a.mini_banner()).join("\n");
    assert!(banner.contains("Session   Fix parser bug"), "{banner}");
    assert!(banner.contains("Continue  ocd mini -s ses_test"), "{banner}");
    assert_eq!(lines_to_ansi(&a.mini_banner(), 80).len(), 3);
}

#[tokio::test]
async fn enter_key_submits_from_home() {
    let mut a = app();
    for c in "hi".chars() {
        a.on_key(Stroke::key(&c.to_string()));
    }
    let ev = crossterm::event::KeyEvent::new(crossterm::event::KeyCode::Enter, crossterm::event::KeyModifiers::NONE);
    a.on_key(crate::keymap::stroke_from_event(&ev).unwrap());
    assert_eq!(a.editor.text, "");
    assert!(matches!(a.route, Route::Session(_)), "{:?}", a.route);
}

// ---- v1 → v2 migration badge (upstream component/migration-overlay.tsx) ----

pub(super) fn render_buf(app: &mut App, w: u16, h: u16) -> ratatui::buffer::Buffer {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| app.draw(f)).unwrap();
    term.backend().buffer().clone()
}

fn running(label: &str, n: Option<u64>, d: Option<u64>) -> migration::MigrationStatus {
    let mut progress = json!({ "label": label });
    if let Some(n) = n {
        progress["numerator"] = json!(n);
    }
    if let Some(d) = d {
        progress["denominator"] = json!(d);
    }
    migration::MigrationStatus::parse(&json!({ "status": "running", "progress": progress }))
}

#[test]
fn migration_status_parse_and_count() {
    use migration::{MigrationStatus as S, badge_text};
    assert_eq!(S::parse(&json!({"status": "required"})), S::Required);
    assert_eq!(S::parse(&json!({"status": "completed"})), S::Completed);
    assert_eq!(S::parse(&json!({"status": "error", "error": "boom"})), S::Error("boom".into()));
    assert_eq!(S::parse(&json!({"status": "weird"})), S::Other);
    let text = |s: S| match s {
        S::Running(Some(p)) => badge_text(&p),
        other => panic!("{other:?}"),
    };
    assert_eq!(text(running("Clearing old events", None, None)), "Clearing old events");
    assert_eq!(text(running("Migrating sessions", Some(3), None)), "Migrating sessions 3");
    assert_eq!(text(running("Migrating sessions", Some(2419), Some(20000))), "Migrating sessions 2419/20000");
    // Template-string formatting of explicit nulls (`${null}`); a missing numerator hides the counts.
    let p = json!({"status": "running", "progress": {"label": "X", "numerator": null, "denominator": 4}});
    assert_eq!(text(S::parse(&p)), "X null/4");
    let p = json!({"status": "running", "progress": {"label": "X", "denominator": 4}});
    assert_eq!(text(S::parse(&p)), "X");
    assert_eq!(S::parse(&json!({"status": "running"})), S::Running(None));
}

#[test]
fn migration_badge_running_layout_and_colors() {
    let mut a = app();
    a.config.set("animations", Some(json!(true)));
    a.on_migration_status(running("Migrating sessions", Some(2419), Some(20000)));
    assert!(a.animating());
    let buf = render_buf(&mut a, 100, 30);
    let row = |y: u16| (0..100).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>();
    // Same cells as upstream at 100x30: border at col 62, spinner at 65, text 67..=95, 2-column margin.
    assert_eq!(row(1).trim_end(), format!("{}┃", " ".repeat(62)));
    let r2 = row(2);
    assert!(r2.ends_with("Migrating sessions 2419/20000    "), "{r2:?}");
    assert_eq!(buf[(62, 2)].symbol(), "┃");
    assert!(crate::ui::text::SPINNER_FRAMES.contains(&buf[(65, 2)].symbol()));
    assert_eq!(buf[(67, 2)].symbol(), "M");
    assert_eq!(buf[(95, 2)].symbol(), "0");
    assert_eq!(row(3).trim_end(), format!("{}┃", " ".repeat(62)));
    let t = a.theme().clone();
    let info = t.text.feedback.info.base.tui();
    let high = t.background.raised.high.tui();
    for (x, y) in [(62, 1), (62, 2), (62, 3), (65, 2), (67, 2), (95, 2)] {
        assert_eq!(buf[(x, y)].fg, info, "fg at {x},{y}");
        assert_eq!(buf[(x, y)].bg, high, "bg at {x},{y}");
    }
    for (x, y) in [(63, 1), (97, 3), (96, 2)] {
        assert_eq!(buf[(x, y)].bg, high, "bg at {x},{y}");
    }
    for (x, y) in [(61, 2), (98, 2), (99, 1), (70, 0), (70, 4)] {
        assert_ne!(buf[(x, y)].bg, high, "outside at {x},{y}");
    }
}

#[test]
fn migration_badge_without_counts_and_without_animations() {
    let mut a = app(); // animations off
    a.on_migration_status(running("Clearing old events", None, None));
    let lines = render(&mut a, 100, 30);
    assert_eq!(lines[2], format!("{}┃  ⋯ Clearing old events", " ".repeat(100 - 2 - 26)));
    assert_eq!(lines[1].trim_start(), "┃");
}

#[test]
fn migration_badge_narrow_terminal() {
    use migration::Badge;
    let p = match running("Migrating sessions", Some(1643), Some(20000)) {
        migration::MigrationStatus::Running(Some(p)) => p,
        _ => unreachable!(),
    };
    // Measured from upstream: at 32 columns the text keeps "Migrating sessions 1643/" and the border sits one
    // column off-screen; at 25 columns "Migrating sessions " and three columns off-screen.
    assert_eq!(Badge::layout(&p, true, 32), Badge { x: -1, width: 31, spinner: true, text: "Migrating sessions 1643/".into() });
    assert_eq!(Badge::layout(&p, true, 25), Badge { x: -3, width: 26, spinner: true, text: "Migrating sessions ".into() });
    assert_eq!(Badge::layout(&p, true, 38).x, 0);
    let mut a = app();
    a.config.set("animations", Some(json!(true)));
    a.on_migration_status(migration::MigrationStatus::Running(Some(p)));
    let lines = render(&mut a, 32, 20);
    assert_eq!(&lines[2][lines[2].char_indices().nth(3).unwrap().0..], " Migrating sessions 1643/");
    assert_eq!(lines[1], "");
}

#[test]
fn migration_badge_hides_and_reports_errors() {
    use migration::MigrationStatus as S;
    let mut a = app();
    a.on_migration_status(running("Migrating sessions", Some(1), Some(2)));
    assert!(contains(&render(&mut a, 100, 30), "⋯ Migrating sessions 1/2"));
    // The spinner phase restarts only when the badge reappears.
    let since = a.migration.as_ref().unwrap().1;
    a.on_migration_status(running("Migrating sessions", Some(2), Some(2)));
    assert_eq!(a.migration.as_ref().unwrap().1, since);
    for hidden in [S::Required, S::Other, S::Running(None), S::Completed] {
        a.on_migration_status(running("Migrating sessions", Some(1), Some(2)));
        a.on_migration_status(hidden);
        assert!(a.migration.is_none());
        assert!(!contains(&render(&mut a, 100, 30), "Migrating sessions"));
    }
    assert!(a.toast.is_none());
    a.on_migration_status(running("Migrating sessions", Some(1), Some(2)));
    a.on_migration_status(S::Error("Incompatible opencode-next.db: project is missing required columns: worktree".into()));
    assert!(a.migration.is_none());
    let toast = a.toast.clone().unwrap();
    assert_eq!(toast.title.as_deref(), Some("Data migration failed"));
    assert_eq!(toast.variant, "error");
    assert_eq!(toast.duration, 10_000);
    let lines = render(&mut a, 100, 30);
    assert!(contains(&lines, "Data migration failed"));
    assert!(contains(&lines, "Incompatible opencode-next.db: project is missing"), "{}", lines.join("\n"));
    // `new Error("")` falls back to the error name.
    a.toast = None;
    a.on_migration_status(S::Error(String::new()));
    assert_eq!(a.toast.clone().unwrap().message, "Error");
}

#[test]
fn migration_badge_draws_over_toasts() {
    let mut a = app();
    a.toast(Toast::new("info", "Saved"));
    a.on_migration_status(running("Migrating sessions", Some(5), Some(9)));
    let lines = render(&mut a, 100, 30);
    assert!(lines[2].ends_with("┃  ⋯ Migrating sessions 5/9"), "{:?}", lines[2]);
}

#[test]
fn toast_border_keeps_underlying_background() {
    let mut a = app();
    a.toast(Toast::new("error", "Something failed badly"));
    let buf = render_buf(&mut a, 100, 30);
    let y = (0..30).find(|&y| (0..100).any(|x| buf[(x, y)].symbol() == "S")).unwrap();
    let left = (0..100).find(|&x| buf[(x, y)].symbol() == "┃").unwrap();
    let t = a.theme().clone();
    assert_eq!(buf[(left, y)].bg, t.background.base.tui_or_reset());
    assert_eq!(buf[(left + 1, y)].bg, t.background.raised.high.tui());
}

/// Minimal HTTP server answering `GET /api/experimental/migration/v1` with `bodies` in order (the last one
/// repeats). Returns the base URL and the request counter.
async fn migration_server(bodies: Vec<&'static str>) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let hits = std::sync::Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let mut buf = vec![0u8; 4096];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            assert!(req.starts_with("GET /api/experimental/migration/v1 "), "{req}");
            let i = counter.fetch_add(1, Ordering::SeqCst);
            let body = bodies[i.min(bodies.len() - 1)];
            let resp = format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
            let _ = sock.write_all(resp.as_bytes()).await;
        }
    });
    (url, hits)
}

async fn drive(app: &mut App, rx: &mut tokio::sync::mpsc::UnboundedReceiver<AppMsg>, secs: u64) -> Vec<Option<String>> {
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(secs);
    while let Ok(Some(msg)) = tokio::time::timeout_at(deadline, rx.recv()).await {
        if let AppMsg::Apply(f) = msg {
            f(app);
            seen.push(app.migration.as_ref().map(|(p, _)| migration::badge_text(p)));
        }
    }
    seen
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migration_poll_until_completed() {
    let (url, hits) = migration_server(vec![
        r#"{"status":"running","progress":{"label":"Clearing old events"}}"#,
        r#"{"status":"running","progress":{"label":"Migrating sessions","numerator":1,"denominator":2}}"#,
        r#"{"status":"completed"}"#,
    ])
    .await;
    let (tx, mut rx) = unbounded_channel();
    let mut a = app();
    a.tx = tx;
    a.client = crate::api::Client::new(&url, None, "/work/proj");
    let started = std::time::Instant::now();
    a.start_migration_poll();
    let seen = drive(&mut a, &mut rx, 5).await;
    assert_eq!(seen, vec![Some("Clearing old events".into()), Some("Migrating sessions 1/2".into()), None]);
    // First poll after 1 s, then every second; nothing after `completed`.
    assert!(started.elapsed() >= std::time::Duration::from_millis(3000));
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 3);
    assert!(a.toast.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migration_poll_stops_on_error() {
    let (url, hits) = migration_server(vec![
        r#"{"status":"required"}"#,
        r#"{"status":"error","error":"disk full\nCaused by: boom"}"#,
    ])
    .await;
    let (tx, mut rx) = unbounded_channel();
    let mut a = app();
    a.tx = tx;
    a.client = crate::api::Client::new(&url, None, "/work/proj");
    a.start_migration_poll();
    let seen = drive(&mut a, &mut rx, 4).await;
    assert_eq!(seen, vec![None, None]);
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2);
    let toast = a.toast.clone().unwrap();
    assert_eq!((toast.title.as_deref(), toast.message.as_str()), (Some("Data migration failed"), "disk full\nCaused by: boom"));
}

fn busy_ev(kind: &str, data: serde_json::Value) -> Event {
    Event { id: format!("evt_{}", crate::util::id_body(false)), created: 80000, kind: kind.into(), data, ..Default::default() }
}

/// A running session whose busy turn started 12.4 s ago.
fn busy_fixture(a: &mut App) -> String {
    let sid = session(a);
    a.on_event(busy_ev("session.execution.started", json!({"sessionID": sid})));
    let now = a.now();
    a.busy.get_mut(&sid).unwrap().started = now - 12_000;
    sid
}

/// The status row, with the elapsed time normalised (a debug-build render can take over a second).
fn busy_row(lines: &[String]) -> String {
    let row = lines.iter().find(|l| l.contains("Waiting for") || l.contains("Running shell") || l.contains("Sending")).cloned().unwrap_or_default();
    regex::Regex::new(r"\(1[2-4]s").unwrap().replace(&row, "(12s").to_string()
}

#[test]
fn busy_status_line_wide_and_narrow() {
    let mut a = app();
    let sid = busy_fixture(&mut a);
    // Request out, no tokens yet: animations are off in the fixture, so the glyph is static.
    let lines = render(&mut a, 100, 40);
    assert_eq!(busy_row(&lines).trim(), "⋯ Waiting for Claude Sonnet 4.5… (12s · esc to interrupt)", "{}", lines.join("\n"));
    // The line sits one blank row above the prompt box; upstream's footer bar stays.
    let row = lines.iter().position(|l| l.contains("Waiting for")).unwrap();
    assert!(lines[row + 1].trim().is_empty(), "{}", lines.join("\n"));
    assert!(lines[row + 2].trim_start().starts_with('┃'), "{}", lines.join("\n"));
    assert!(contains(&lines, "[⋯] esc interrupt"), "{}", lines.join("\n"));
    let lines = render(&mut a, 40, 30);
    assert_eq!(busy_row(&lines).trim(), "⋯ Waiting for Claude Sonnet 4.5… (12s)", "{}", lines.join("\n"));
    // A running tool.
    a.on_event(busy_ev("session.step.started", json!({"sessionID": sid, "assistantMessageID": "msg_9", "agent": "build", "model": {"id":"claude-sonnet","providerID":"anthropic"}, "started": 80000})));
    a.on_event(busy_ev("session.tool.input.started", json!({"sessionID": sid, "assistantMessageID": "msg_9", "id": "t9", "name": "bash"})));
    a.on_event(busy_ev("session.tool.called", json!({"sessionID": sid, "assistantMessageID": "msg_9", "id": "t9", "input": {"command": "npm test"}})));
    // The command goes on its own row under the status, then a blank row, then the prompt box.
    let lines = render(&mut a, 100, 40);
    assert_eq!(busy_row(&lines).trim(), "⋯ Running shell… (12s · ↓ ~6 tokens · esc to interrupt)", "{}", lines.join("\n"));
    let row = lines.iter().position(|l| l.contains("Running shell…")).unwrap();
    assert_eq!(lines[row + 1].trim(), "$ npm test", "{}", lines.join("\n"));
    assert!(lines[row + 2].trim().is_empty(), "{}", lines.join("\n"));
    assert!(lines[row + 3].trim_start().starts_with('┃'), "{}", lines.join("\n"));
    let lines = render(&mut a, 40, 30);
    assert_eq!(busy_row(&lines).trim(), "⋯ Running shell… (12s · ↓ ~6 tokens)", "{}", lines.join("\n"));
    // Armed interrupt.
    a.on_key(Stroke::key("escape"));
    let lines = render(&mut a, 100, 40);
    assert!(busy_row(&lines).ends_with("esc again to interrupt)"), "{}", lines.join("\n"));
    // Idle: gone.
    a.on_event(busy_ev("session.execution.succeeded", json!({"sessionID": sid})));
    let lines = render(&mut a, 100, 40);
    assert!(!lines.iter().any(|l| l.contains("Running shell…") || l.contains("$ npm test") || l.contains("to interrupt")), "{}", lines.join("\n"));
    assert!(a.busy.is_empty());
}

#[test]
fn busy_status_replaces_the_live_thinking_row() {
    let mut a = app();
    let sid = busy_fixture(&mut a);
    a.on_event(busy_ev("session.step.started", json!({"sessionID": sid, "assistantMessageID": "msg_9", "agent": "build", "model": {"id":"claude-sonnet","providerID":"anthropic"}, "started": 80000})));
    a.on_event(busy_ev("session.reasoning.started", json!({"sessionID": sid, "assistantMessageID": "msg_9", "ordinal": 0})));
    a.on_event(busy_ev("session.reasoning.delta", json!({"sessionID": sid, "assistantMessageID": "msg_9", "ordinal": 0, "delta": "**Planning the tests**\n\nFirst"})));
    let lines = render(&mut a, 100, 40);
    // Thinking shows once: in the status line, with the thought's title, between blank rows.
    let rows: Vec<usize> = lines.iter().enumerate().filter(|(_, l)| l.contains("Thinking")).map(|(i, _)| i).collect();
    assert_eq!(rows.len(), 1, "{}", lines.join("\n"));
    assert!(lines[rows[0]].trim().starts_with("⋯ Thinking: Planning the tests… (12s"), "{}", lines.join("\n"));
    assert!(lines[rows[0] - 1].trim().is_empty() && lines[rows[0] + 1].trim().is_empty(), "{}", lines.join("\n"));
    // Once the thought completes, the transcript shows it as usual.
    a.on_event(busy_ev("session.reasoning.ended", json!({"sessionID": sid, "assistantMessageID": "msg_9", "ordinal": 0, "text": "**Planning the tests**\n\nFirst"})));
    a.on_event(busy_ev("session.text.started", json!({"sessionID": sid, "assistantMessageID": "msg_9", "ordinal": 0})));
    let lines = render(&mut a, 100, 40);
    assert!(contains(&lines, "+ Thought: Planning the tests"), "{}", lines.join("\n"));
    assert!(!lines.iter().any(|l| l.contains("Thinking")), "{}", lines.join("\n"));
}

#[test]
fn busy_status_animated_spinner_and_mini() {
    let mut a = app();
    a.config.set("animations", Some(json!(true)));
    busy_fixture(&mut a);
    let lines = render(&mut a, 100, 40);
    let row = busy_row(&lines);
    let glyph = row.trim().chars().next().unwrap().to_string();
    assert!(crate::ui::text::SPINNER_FRAMES.contains(&glyph.as_str()), "{row}");
    // Mini mode: the same line sits above the prompt in the live footer.
    let (buf, _) = a.mini_footer(80, 30, &[]);
    let rows: Vec<String> = (0..buf.area.height).map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>()).collect();
    assert!(busy_row(&rows).contains("Waiting for Claude Sonnet 4.5… (12s · esc to interrupt)"), "{}", rows.join("\n"));
}

#[test]
fn busy_turn_from_submit_until_idle() {
    let mut a = app();
    let sid = session(&mut a);
    a.turn_begin(&sid);
    let lines = render(&mut a, 100, 40);
    assert!(contains(&lines, "⋯ Sending… (0s)"), "{}", lines.join("\n"));
    a.turn_acked(&sid, true, false);
    assert!(a.busy[&sid].acked.is_some());
    // A failed submission while idle ends the turn.
    a.turn_acked(&sid, false, false);
    assert!(a.busy.is_empty());
    // Acknowledged turns whose execution never starts are dropped.
    a.turn_begin(&sid);
    a.busy.get_mut(&sid).unwrap().acked = Some(a.now() - super::busy::STALE_MS - 1);
    a.tick();
    assert!(a.busy.is_empty());
    // A run observed without a turn (resumed session) is adopted.
    a.store.active.insert(sid.clone(), true);
    a.tick();
    assert!(a.busy.contains_key(&sid));
}

/// Managed-service hooks handing out `url`; counts restarts.
fn managed(url: &str) -> (ServiceHooks, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = calls.clone();
    let endpoint = crate::api::Endpoint { url: url.to_string(), password: Some("next".into()) };
    let e2 = endpoint.clone();
    let hooks = ServiceHooks {
        reconnect: std::sync::Arc::new(move || {
            let e = endpoint.clone();
            Box::pin(async move { Ok(e) })
        }),
        restart: std::sync::Arc::new(move || {
            seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let e = e2.clone();
            Box::pin(async move { Ok(e) })
        }),
    };
    (hooks, calls)
}

/// `service.restart` (`/restart`) exists only with a managed service (`...(client.restart ? [...] : [])`).
#[test]
fn service_restart_only_when_managed() {
    let mut a = app();
    assert!(!a.commands().iter().any(|c| c.id == "service.restart"));
    assert!(!a.run_command("service.restart", None));
    let (hooks, _) = managed("http://127.0.0.1:2");
    a.opts.service = Some(hooks);
    let cmds = a.commands();
    let pos = |id: &str| cmds.iter().position(|c| c.id == id).unwrap();
    let c = &cmds[pos("service.restart")];
    assert_eq!((c.title.as_str(), c.group, c.slash), ("Restart service", "System", Some("restart")));
    // Between "Pair device" and "Reload configuration", as upstream lists it.
    assert_eq!(pos("service.restart"), pos("server.pair") + 1);
    assert_eq!(pos("location.reload"), pos("service.restart") + 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_restart_toasts_and_moves_client() {
    let (tx, mut rx) = unbounded_channel();
    let mut a = app();
    a.tx = tx;
    let (hooks, calls) = managed("http://127.0.0.1:3");
    a.opts.service = Some(hooks);
    a.run_command("help.show", None);
    assert!(!a.dialogs.is_empty());
    assert!(a.run_command("service.restart", None));
    assert!(a.dialogs.is_empty());
    let t = a.toast.clone().unwrap();
    assert_eq!((t.variant.as_str(), t.message.as_str(), t.duration), ("info", "Restarting service…", 30000));
    let msg = tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv()).await.unwrap().unwrap();
    let AppMsg::Apply(f) = msg else { panic!("expected apply") };
    f(&mut a);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    let t = a.toast.clone().unwrap();
    assert_eq!((t.variant.as_str(), t.message.as_str()), ("success", "Service restarted"));
    assert_eq!(a.client.base(), "http://127.0.0.1:3");
}

/// `<Reconnecting managed>`: a managed service says it is restarting, otherwise the connection is lost.
#[test]
fn reconnecting_banner_managed_text() {
    let mut a = app();
    a.connection = crate::api::sse::ConnStatus::Reconnecting;
    a.ever_connected = true;
    a.connection_since = std::time::Instant::now() - std::time::Duration::from_secs(2);
    let lines = render(&mut a, 100, 30);
    assert!(contains(&lines, "Connection lost…"), "{}", lines.join("\n"));
    assert!(contains(&lines, "Reconnecting to the server automatically."));
    let (hooks, _) = managed("http://127.0.0.1:2");
    a.opts.service = Some(hooks);
    let lines = render(&mut a, 100, 30);
    assert!(contains(&lines, "Restarting service…"), "{}", lines.join("\n"));
    assert!(contains(&lines, "Your session will resume automatically."));
}
