//! The experimental todo list in the TUI: the sidebar section (`feature-plugins/sidebar/todo.tsx`) and the
//! `todowrite` transcript rendering (`routes/session/index.tsx` `TodoWrite`, `component/todo-item.tsx`).

use super::tests::{app, render};
use super::*;
use crate::ui::text::Click;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::{Value, json};

fn contains(lines: &[String], needle: &str) -> bool {
    lines.iter().any(|l| l.contains(needle))
}

fn todo_call(id: &str, status: &str, todos: Value) -> Value {
    let mut state = json!({ "status": status, "input": { "todos": todos } });
    if status == "error" {
        state["error"] = json!({ "type": "tool.execution", "message": "Invalid arguments for tool \"todowrite\"" });
    }
    json!({ "type": "tool", "id": id, "name": "todowrite", "state": state, "time": { "created": 1200 } })
}

fn assistant(id: &str, content: Vec<Value>) -> Value {
    json!({ "id": id, "type": "assistant", "agent": "build", "model": { "id": "claude-sonnet", "providerID": "anthropic" },
            "time": { "created": 1100, "completed": 1300 }, "finish": "stop", "content": content })
}

fn three() -> Value {
    json!([
        { "content": "Read the code", "status": "completed" },
        { "content": "Write tests", "status": "in_progress" },
        { "content": "Ship it", "status": "pending" },
    ])
}

fn four() -> Value {
    json!([
        { "content": "Read the code", "status": "completed" },
        { "content": "Write tests for the parser, the lexer and the pretty printer, including the error recovery paths", "status": "completed" },
        { "content": "Ship it", "status": "in_progress" },
        { "content": "Rewrite everything", "status": "cancelled" },
    ])
}

/// A session whose messages are `messages` (after one user prompt).
fn with_messages(a: &mut App, messages: Vec<Value>) -> String {
    let id = "ses_todo".to_string();
    a.store.remember(SessionInfo {
        id: id.clone(),
        title: Some("Plan the work".into()),
        agent: Some("build".into()),
        location: LocationRef { directory: "/work/proj".into(), workspace_id: None },
        ..Default::default()
    });
    let mut all = vec![json!({ "id": "msg_0", "type": "user", "text": "plan it", "time": { "created": 1000 } })];
    all.extend(messages);
    a.store.messages.insert(id.clone(), serde_json::from_value(Value::Array(all)).unwrap());
    a.store.loaded.insert(format!("session.message:{id}"));
    a.store.loaded.insert(format!("session.pending:{id}"));
    a.route = Route::Session(id.clone());
    id
}

fn sidebar(lines: &[String]) -> Vec<String> {
    lines.iter().map(|l| l.chars().skip(98).collect::<String>().trim_end().to_string()).collect()
}

#[test]
fn sidebar_lists_the_latest_completed_todos_and_collapses() {
    let mut a = app();
    with_messages(
        &mut a,
        vec![
            assistant("msg_1", vec![todo_call("t1", "completed", three())]),
            assistant("msg_2", vec![todo_call("t2", "completed", four()), todo_call("t3", "error", json!([{ "content": "Bad", "status": "done" }]))]),
        ],
    );
    let side = sidebar(&render(&mut a, 140, 40));
    let joined = side.join("\n");
    let head = side.iter().position(|l| l.contains("▼ Todo")).unwrap_or_else(|| panic!("{joined}"));
    assert_eq!(
        side[head..head + 7].to_vec(),
        vec![
            "  ▼ Todo",
            "  [✓] Read the code",
            "  [✓] Write tests for the parser, the",
            "      lexer and the pretty printer,",
            "      including the error recovery",
            "      paths",
            "  [•] Ship it",
        ],
        "{joined}"
    );
    assert!(contains(&side, "[-] Rewrite everything"), "{joined}");

    // The header toggles the section and persists it in the plugin's storage.
    a.on_click(Click::Custom(super::sidebar_todo::TOGGLE.into()));
    assert_eq!(a.keyed.get("plugin.opencode.sidebar.todo.view"), Some(json!({ "open": false })));
    let side = sidebar(&render(&mut a, 140, 40));
    assert!(contains(&side, "  ▶ Todo (1 remaining)"), "{}", side.join("\n"));
    assert!(!contains(&side, "Ship it"));
    a.on_click(Click::Custom(super::sidebar_todo::TOGGLE.into()));
    assert_eq!(a.keyed.get("plugin.opencode.sidebar.todo.view"), Some(json!({ "open": true })));
    assert!(contains(&sidebar(&render(&mut a, 140, 40)), "  ▼ Todo"));
}

#[test]
fn sidebar_colors_and_short_lists() {
    let mut a = app();
    with_messages(&mut a, vec![assistant("msg_1", vec![todo_call("t1", "completed", json!([
        { "content": "Ship it", "status": "in_progress" },
        { "content": "Rewrite", "status": "cancelled" },
    ]))])]);
    let mut term = Terminal::new(TestBackend::new(140, 40)).unwrap();
    term.draw(|f| a.draw(f)).unwrap();
    let buf = term.backend().buffer().clone();
    let row = |needle: &str| (0..40u16).find(|y| (98..140u16).map(|x| buf[(x, *y)].symbol()).collect::<String>().contains(needle)).unwrap();
    // Two items: no arrow and no collapsing.
    let y = row("Todo");
    assert_eq!((100..106u16).map(|x| buf[(x, y)].symbol()).collect::<String>(), "Todo  ");
    assert!(buf[(100, y)].modifier.contains(ratatui::style::Modifier::BOLD));
    let t = a.theme().clone();
    let y = row("Ship it");
    assert_eq!(buf[(100, y)].fg, t.text.feedback.warning.base.tui());
    assert_eq!(buf[(104, y)].fg, t.text.feedback.warning.base.tui());
    let y = row("Rewrite");
    assert_eq!(buf[(104, y)].fg, t.text.faint.tui());
    assert!(buf[(104, y)].modifier.contains(ratatui::style::Modifier::CROSSED_OUT));
    assert!(!buf[(100, y)].modifier.contains(ratatui::style::Modifier::CROSSED_OUT));
}

#[test]
fn sidebar_hides_cleared_and_reverted_lists() {
    let mut a = app();
    let sid = with_messages(
        &mut a,
        vec![
            assistant("msg_1", vec![todo_call("t1", "completed", three())]),
            json!({ "id": "msg_2", "type": "user", "text": "clear", "time": { "created": 2000 } }),
            assistant("msg_3", vec![todo_call("t2", "completed", json!([])), todo_call("t3", "running", four())]),
        ],
    );
    // The latest completed call cleared the list.
    assert!(!contains(&sidebar(&render(&mut a, 140, 40)), "Todo"));
    // Reverted to before the clearing prompt: the earlier list is current again.
    if let Some(s) = a.store.sessions.get_mut(&sid) {
        s.revert = Some(crate::api::types::Revert { message_id: "msg_2".into(), ..Default::default() });
    }
    assert!(contains(&sidebar(&render(&mut a, 140, 40)), "▼ Todo"));
    // The `opencode.sidebar.todo` built-in can be turned off.
    a.disabled_builtins.insert(super::sidebar_todo::ID.to_string());
    assert!(!contains(&sidebar(&render(&mut a, 140, 40)), "Todo"));
}

#[test]
fn transcript_renders_todowrite() {
    let mut a = app();
    with_messages(
        &mut a,
        vec![assistant(
            "msg_1",
            vec![
                todo_call("t1", "completed", four()),
                todo_call("t2", "error", json!([{ "content": "Bad", "status": "done" }])),
                todo_call("t3", "completed", json!([])),
            ],
        )],
    );
    let lines = render(&mut a, 100, 40);
    let joined = lines.join("\n");
    let at = lines.iter().position(|l| l.contains("# Todos")).unwrap_or_else(|| panic!("{joined}"));
    assert_eq!(
        lines[at..at + 7].iter().map(|l| l.trim_end()).collect::<Vec<_>>(),
        vec![
            "  ┃  # Todos",
            "  ┃",
            "  ┃  [✓] Read the code",
            "  ┃  [✓] Write tests for the parser, the lexer and the pretty printer, including the error",
            "  ┃      recovery paths",
            "  ┃  [•] Ship it",
            "  ┃  [-] Rewrite everything",
        ],
        "{joined}"
    );
    assert!(contains(&lines, "   ⚙ Todo update failed"), "{joined}");
    assert!(contains(&lines, "   ⚙ Cleared todos"), "{joined}");
}

#[test]
fn transcript_shows_a_pending_todowrite_inline() {
    let mut a = app();
    with_messages(&mut a, vec![assistant("msg_1", vec![todo_call("t1", "running", three())])]);
    let lines = render(&mut a, 100, 40);
    assert!(contains(&lines, "Updating todos…"), "{}", lines.join("\n"));
    assert!(!contains(&lines, "# Todos"));
}
