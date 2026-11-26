//! The TUI side of upstream's move to OpenTUI 0.9.4 (#63989): answers and reasoning drawn by the native Markdown
//! engine (reasoning in the reasoning colors, its source in source mode).

use super::tests::app;
use super::*;
use serde_json::json;

fn sid() -> String {
    "ses_opentuiopentuiopentuiopentu".into()
}

/// The current session showing an answer after a reasoning part.
fn transcript(a: &mut App, reasoning: &str, text: &str) {
    let id = sid();
    a.store.remember(serde_json::from_value(json!({ "id": id, "projectID": "prj_1", "title": "Current", "location": { "directory": "/work/proj" }, "time": { "created": 1, "updated": 1 } })).unwrap());
    let messages = json!([
        {"id": "msg_0001", "type": "user", "text": "think", "time": {"created": 1000}},
        {"id": "msg_0002", "type": "assistant", "agent": "build", "model": {"id": "claude-sonnet", "providerID": "anthropic"},
         "time": {"created": 2000, "completed": 3000}, "finish": "stop",
         "content": [{"type": "reasoning", "text": reasoning, "time": {"created": 2000, "completed": 2500}}, {"type": "text", "text": text}]}
    ]);
    a.store.messages.insert(id.clone(), serde_json::from_value::<Vec<Message>>(messages).unwrap());
    a.store.loaded.insert(format!("session:{id}"));
    a.store.loaded.insert(format!("session.message:{id}"));
    a.store.loaded.insert(format!("session.pending:{id}"));
    a.route = Route::Session(id);
}

fn texts(a: &App) -> Vec<String> {
    let (lines, _) = a.render_transcript_lines(&sid(), 80);
    lines.iter().map(|l| l.text().trim_end().to_string()).collect()
}

const THOUGHT: &str = "## Plan\n\n| step | risk |\n| --- | --- |\n| check | low |\n\n```ts\nconst probe = 1\n```";

/// `reasoning-body.test.tsx` "reasoning follows the markdown setting, also when it changes": rendered mode draws the
/// thought as Markdown (a grid table, the heading and fence markers concealed), source mode its Markdown source.
#[test]
fn reasoning_follows_the_markdown_setting() {
    let mut a = app();
    a.config.set("session.thinking", Some(json!("show")));
    a.config.set("session.verbosity", Some(json!("medium")));
    transcript(&mut a, THOUGHT, "Answer.");
    for (setting, shown, hidden) in [
        ("rendered", vec!["┌", "│ check ", "Plan", "const probe = 1"], vec!["## Plan", "| --- |", "```"]),
        ("source", vec!["## Plan", "| --- | --- |", "```ts", "const probe = 1"], vec!["┌"]),
        ("rendered", vec!["┌", "check"], vec!["## Plan", "```"]),
    ] {
        a.config.set("session.markdown", Some(json!(setting)));
        let joined = texts(&a).join("\n");
        for t in shown {
            assert!(joined.contains(t), "{setting}: {t:?} missing in\n{joined}");
        }
        for t in hidden {
            assert!(!joined.contains(t), "{setting}: {t:?} shown in\n{joined}");
        }
    }
}

/// The open thought's rows: the reasoning bar, then the Markdown in `text.muted` (its table borders `#888888`).
#[test]
fn reasoning_rows_are_markdown_in_the_reasoning_colors() {
    let mut a = app();
    a.config.set("session.thinking", Some(json!("show")));
    a.config.set("session.verbosity", Some(json!("medium")));
    transcript(&mut a, THOUGHT, "Answer.");
    let (lines, _) = a.render_transcript_lines(&sid(), 80);
    let plan = lines.iter().find(|l| l.text().contains("Plan")).unwrap();
    let seg = plan.segs.iter().find(|s| s.text.contains("Plan")).unwrap();
    assert_eq!(seg.style.fg, Some(a.theme().text.muted.tui()));
    assert!(seg.style.add_modifier.contains(ratatui::style::Modifier::BOLD));
    let row = lines.iter().find(|l| l.text().contains("│ step")).unwrap();
    assert!(row.text().trim_start().starts_with("┃ │ step"), "{:?}", row.text());
}
