//! Dialog layout (2.1's shared `DialogLayout`) and the 2.1 TUI quick wins, checked against
//! upstream 2.1.4 screen captures at 100x30.

use super::tests::{app, render, session};
use super::*;
use crate::keymap::Stroke;
use serde_json::json;

fn row<'a>(lines: &'a [String], needle: &str) -> &'a str {
    lines.iter().find(|l| l.contains(needle)).map(|s| s.as_str()).unwrap_or_else(|| panic!("no row with {needle:?}:\n{}", lines.join("\n")))
}

fn col(line: &str, needle: &str) -> usize {
    let byte = line.find(needle).unwrap_or_else(|| panic!("{needle:?} not in {line:?}"));
    crate::util::locale::width(&line[..byte])
}

/// The cells of a row under a medium dialog (x = 20..80 at 100 columns).
fn inside(line: &str) -> String {
    line.chars().skip(20).take(60).collect()
}

fn clipboard() -> Option<String> {
    crate::clipboard::TEST_CLIPBOARD.with(|c| c.borrow().clone())
}

#[test]
fn help_and_alert_use_the_dialog_inset() {
    let mut a = app();
    a.run_command("help.show", None);
    let lines = render(&mut a, 100, 30);
    // Medium dialog at x=20: title and body at the inset of 3, `esc/enter` and `ok` end 3 cells in.
    let title = row(&lines, "Help");
    assert_eq!(col(title, "Help"), 23);
    assert_eq!(col(title, "esc/enter") + 9, 77);
    assert_eq!(col(row(&lines, "Press ctrl+p"), "Press"), 23);
    let ok = row(&lines, " ok");
    assert_eq!(col(ok, "ok") + 2 + 3, 77, "{ok:?}");
    // Header, blank, two text rows, blank, button.
    let y = lines.iter().position(|l| l.contains("esc/enter")).unwrap();
    assert!(lines[y + 1].trim().is_empty());
    assert!(lines[y + 2].contains("Press ctrl+p"));
    assert!(lines[y + 3].contains("in any context."));
    assert!(lines[y + 4].trim().is_empty());
    assert!(lines[y + 5].contains("ok"));
}

#[test]
fn debug_dialog_footer_and_copy() {
    let mut a = app();
    let _ = render(&mut a, 100, 30);
    a.run_command("opencode.debug", None);
    let lines = render(&mut a, 100, 30);
    let joined = lines.join("\n");
    // Large dialog at x=6: rows at the inset of 3, labels padded to 10 plus a gap.
    assert_eq!(col(row(&lines, "Debug"), "Debug"), 9);
    assert!(contains(&lines, "Version    2.0.26 (test)"), "{joined}");
    assert!(contains(&lines, "Session ID n/a"), "{joined}");
    assert!(contains(&lines, "Model      anthropic/claude-sonnet"), "{joined}");
    let footer = row(&lines, "Share this when reporting an issue.");
    assert_eq!(col(footer, "Share"), 9);
    assert_eq!(col(footer, "copy enter") + "copy enter".len(), 94 - 3, "{footer:?}");
    crate::clipboard::TEST_CLIPBOARD.with(|c| *c.borrow_mut() = None);
    a.on_key(Stroke::key("return"));
    assert!(clipboard().is_some_and(|t| t.starts_with("Version: 2.0.26 (test)\nDate: ")), "{:?}", clipboard());
    assert_eq!(a.toast.as_ref().map(|t| t.message.as_str()), Some("Debug info copied to clipboard"));
    let lines = render(&mut a, 100, 30);
    assert!(contains(&lines, "✓ copied enter"), "{}", lines.join("\n"));
    assert_eq!(a.dialogs.len(), 1);
}

fn contains(lines: &[String], needle: &str) -> bool {
    lines.iter().any(|l| l.contains(needle))
}

#[test]
fn select_rows_and_footer_line_up_at_the_list_inset() {
    let mut a = app();
    a.run_command("opencode.settings", None);
    let lines = render(&mut a, 100, 30);
    // Title, rows and footer at x+4; the current marker takes the gutter at x+2.
    assert_eq!(col(row(&lines, "Settings"), "Settings"), 24);
    let theme = row(&lines, "Theme");
    assert_eq!(col(theme, "●"), 22, "{theme:?}");
    assert_eq!(col(theme, "Theme"), 24);
    // Footers end at the row padding (x + w - 4).
    assert_eq!(col(theme, "opencode") + "opencode".len(), 76, "{theme:?}");
    assert_eq!(col(row(&lines, "Color mode"), "Color mode"), 24);
    // Hints read label first, not bold.
    let hint = row(&lines, "change ←/→");
    assert_eq!(col(hint, "change"), 24);
    // Animations shows the setting's own default (off) and highlights has a row in Alerts.
    assert!(row(&lines, "Animations").ends_with("off"), "{}", lines.join("\n"));
    let keys: Vec<&str> = commands::settings().iter().map(|s| s.2).collect();
    let volume = keys.iter().position(|k| *k == "attention.volume").unwrap();
    assert_eq!(keys[volume + 1], "highlights");
}

#[test]
fn tab_no_longer_focuses_dialog_actions() {
    let mut a = app();
    session(&mut a);
    a.run_command("session.list", None);
    let before = a.dialogs.last().cloned().unwrap();
    a.on_key(Stroke::key("tab"));
    let after = a.dialogs.last().cloned().unwrap();
    match (&before.body, &after.body) {
        (crate::ui::dialog::Body::Select(x), crate::ui::dialog::Body::Select(y)) => assert_eq!(x.selected, y.selected),
        _ => panic!("not a select"),
    }
    let lines = render(&mut a, 100, 30);
    assert!(contains(&lines, "delete ctrl+d  rename ctrl+r"), "{}", lines.join("\n"));
}

fn model(provider: &str, id: &str, name: &str, free: bool) -> ModelInfo {
    let mut v = json!({ "id": id, "providerID": provider, "name": name, "status": "active", "enabled": true });
    if free {
        v["cost"] = json!([{ "input": 0, "output": 0 }]);
    } else {
        v["cost"] = json!([{ "input": 3, "output": 15 }]);
    }
    serde_json::from_value(v).unwrap()
}

#[test]
fn models_open_on_the_current_model_in_server_order() {
    let mut a = app();
    let loc = a.store.location_mut("/work/proj");
    loc.models =
        Some(vec![model("mock", "m1", "Mock One", false), model("opencode", "pickle", "Big Pickle", true), model("opencode", "zeta", "Zeta Free", true), model("opencode", "alpha", "Alpha", false)]);
    loc.providers = Some(vec![ProviderInfo { id: "mock".into(), name: "Mock".into(), ..Default::default() }]);
    a.default_model = Some(ModelRef { provider_id: "mock".into(), id: "m1".into(), variant: None });
    a.run_command("model.list", None);
    let lines = render(&mut a, 100, 30);
    let joined = lines.join("\n");
    // Nothing connected: no provider groups; free models first, otherwise the server's order.
    let order: Vec<usize> = ["Big Pickle", "Zeta Free", "Mock One", "Alpha"].iter().map(|n| lines.iter().position(|l| l.contains(n)).unwrap()).collect();
    assert!(order.windows(2).all(|w| w[0] < w[1]), "{joined}");
    assert!(contains(&lines, "● Mock One"), "{joined}");
    assert!(contains(&lines, "View all integrations ctrl+a"), "{joined}");
    assert!(!contains(&lines, "Favorite"), "{joined}");
    let sel = a.dialogs.last_mut().unwrap().sel().unwrap().selected_value();
    assert_eq!(sel.as_deref(), Some("mock/m1"));
    // A connection groups by provider (opencode first) and offers favorites.
    a.close_dialog();
    a.store.location_mut("/work/proj").integrations = Some(vec![json!({ "id": "mock", "connections": [{ "id": "c1" }] })]);
    a.run_command("model.list", None);
    let lines = render(&mut a, 100, 40);
    let joined = lines.join("\n");
    assert!(contains(&lines, "Connect an integration ctrl+a  Favorite ctrl+f"), "{joined}");
    let order: Vec<usize> = ["opencode", "Big Pickle", "Zeta Free", "Alpha", "Mock", "Mock One"]
        .iter()
        .map(|n| lines.iter().position(|l| inside(l).trim_start().trim_start_matches('●').trim_start().starts_with(n)).unwrap_or_else(|| panic!("{n}: {joined}")))
        .collect();
    assert!(order.windows(2).all(|w| w[0] < w[1]), "{joined}");
}

#[test]
fn copy_copies_the_last_assistant_message() {
    let mut a = app();
    let sid = session(&mut a);
    let slash: Vec<String> = a.commands().iter().filter(|c| c.slash == Some("copy")).map(|c| c.id.clone()).collect();
    assert_eq!(slash, vec!["messages.copy".to_string()]);
    crate::clipboard::TEST_CLIPBOARD.with(|c| *c.borrow_mut() = None);
    a.run_command("messages.copy", None);
    assert_eq!(clipboard().as_deref(), Some("I fixed the **bug**.\n\n- updated parser\n- added test"));
    let t = a.toast.clone().unwrap();
    assert_eq!((t.variant.as_str(), t.message.as_str()), ("success", "Message copied to clipboard!"));
    // A later completed compaction with a summary wins.
    let msgs = a.store.messages.get_mut(&sid).unwrap();
    msgs.push(serde_json::from_value(json!({"id":"msg_4","type":"compaction","status":"completed","reason":"manual","summary":"  Summary text \n","time":{"created":80000}})).unwrap());
    a.toast = None;
    a.run_command("messages.copy", None);
    assert_eq!(clipboard().as_deref(), Some("Summary text"));
    // An assistant message without text parts.
    msgs_push(&mut a, &sid, json!({"id":"msg_5","type":"assistant","time":{"created":90000},"content":[{"type":"reasoning","text":"hmm","time":{"created":1}}]}));
    a.toast = None;
    a.toast_queue.clear();
    a.run_command("messages.copy", None);
    let t = a.toast.clone().unwrap();
    assert_eq!((t.variant.as_str(), t.message.as_str()), ("error", "No text content found in last assistant message"));
    // Multiple text parts join with a newline.
    let m: Message = serde_json::from_value(json!({"id":"msg_6","type":"assistant","time":{"created":1},"content":[{"type":"text","text":"a "},{"type":"text","text":" b"}]})).unwrap();
    assert_eq!(commands::last_assistant_text(&[m], None).as_deref(), Some("a \n b"));
    assert_eq!(commands::last_assistant_text(&[], None), None);
}

fn msgs_push(a: &mut App, sid: &str, v: serde_json::Value) {
    a.store.messages.get_mut(sid).unwrap().push(serde_json::from_value(v).unwrap());
}

#[test]
fn session_sharing_is_gone() {
    let mut a = app();
    session(&mut a);
    assert!(!a.commands().iter().any(|c| c.id == "session.share" || c.slash == Some("share")));
    let desc = crate::keymap::defs::DEFINITIONS.iter().find(|d| d.0 == "session.share").map(|d| d.2);
    assert_eq!(desc, Some("Deprecated: session sharing was removed"));
    a.on_key(Stroke::ctrl("p"));
    let lines = render(&mut a, 100, 60);
    assert!(!contains(&lines, "Share session"), "{}", lines.join("\n"));
}

#[test]
fn slash_menu_opens_only_at_the_start() {
    let mut a = app();
    for c in "hi /mod".chars() {
        a.on_key(Stroke::key(&c.to_string()));
    }
    assert!(a.autocomplete.is_none());
    let mut a = app();
    a.on_key(Stroke::key("/"));
    assert_eq!(a.autocomplete.as_ref().map(|x| x.mode.as_str()), Some("command"));
}

#[test]
fn locale_number_never_shows_1000k() {
    assert_eq!(crate::util::locale::number(999_949.0), "999.9K");
    assert_eq!(crate::util::locale::number(999_950.0), "1.0M");
}

#[test]
fn export_options_legacy_layout() {
    let mut a = app();
    session(&mut a);
    a.run_command("session.export", None);
    let lines = render(&mut a, 100, 30);
    let joined = lines.join("\n");
    let y = lines.iter().position(|l| l.contains("Export session")).unwrap();
    assert_eq!(col(&lines[y], "Export session"), 23);
    assert_eq!(col(&lines[y + 2], "Export as:  ◉ Markdown   ○ JSON"), 23, "{joined}");
    assert!(lines[y + 4].contains("[x] Include thinking"), "{joined}");
    assert!(inside(&lines[y + 5]).trim().is_empty(), "{joined}");
    assert!(lines[y + 6].contains("[x] Include tools"), "{joined}");
    let buttons = &lines[y + 8];
    assert_eq!(col(buttons, "Copy") - 4, 77 - 12 - 1 - 14, "{buttons:?}");
    assert!(buttons.ends_with("Export"), "{buttons:?}");
}

#[test]
fn btw_answer_header_and_footer() {
    let mut a = app();
    let _ = render(&mut a, 100, 30);
    let d = commands::btw_answer(&a, "what  is\nthis?", "It is **fine**.", false);
    a.open_dialog(d);
    let lines = render(&mut a, 100, 30);
    let joined = lines.join("\n");
    let head = row(&lines, "/btw");
    assert_eq!(col(head, "/btw"), 9);
    assert_eq!(col(head, "what is this?"), 15, "{joined}");
    assert_eq!(col(head, "esc") + 3, 91);
    assert!(contains(&lines, "copy c  scroll j/k ↑/↓"), "{joined}");
    crate::clipboard::TEST_CLIPBOARD.with(|c| *c.borrow_mut() = None);
    a.on_key(Stroke::key("c"));
    assert_eq!(clipboard().as_deref(), Some("It is **fine**."));
    let lines = render(&mut a, 100, 30);
    assert!(contains(&lines, "✓ copied  scroll j/k ↑/↓"), "{}", lines.join("\n"));
}

#[test]
fn export_result_close_button() {
    let mut a = app();
    let mut td = crate::ui::dialog::TextDialog::new("Session exported", vec![crate::ui::text::SLine::new(vec![crate::ui::text::seg("/tmp/x.md", ratatui::style::Style::default())])]);
    td.button = Some("Close".into());
    a.open_dialog(crate::ui::dialog::Dialog::text("export-result", td));
    let lines = render(&mut a, 100, 30);
    let b = row(&lines, "Close");
    assert_eq!(col(b, "Close") + 5 + 3, 77, "{b:?}");
    a.on_key(Stroke::key("return"));
    assert!(a.dialogs.is_empty());
}

#[test]
fn fork_pending_view_uses_the_list_inset() {
    let mut a = app();
    let _sid = session(&mut a);
    a.open_fork(Some("msg_x".into()));
    let lines = render(&mut a, 100, 30);
    // Large dialog at x=6: "Fork session" and the spinner at the list inset of 4, `esc` 4 in from the right.
    let title = row(&lines, "Fork session");
    assert_eq!(col(title, "Fork session"), 10);
    assert_eq!(col(title, "esc") + 3, 90);
    let y = lines.iter().position(|l| l.contains("Fork session")).unwrap();
    assert!(lines[y + 1].chars().skip(6).take(88).all(|c| c == ' '));
    // Animations are off in tests, so the spinner is `⋯`.
    assert_eq!(col(&lines[y + 2], "\u{22ef} Forking session\u{2026}"), 10, "{}", lines[y + 2]);
    assert_eq!(a.dialogs.len(), 1);
}

#[test]
fn execute_detail_dialog() {
    let mut a = app();
    let sid = session(&mut a);
    let m: Message = serde_json::from_value(json!({
        "id": "msg_x", "type": "assistant", "agent": "build", "time": {"created": 1000},
        "content": [{"type": "tool", "id": "call_x", "name": "execute",
            "state": {"status": "completed", "input": {"code": "const a = 1\nreturn a"},
                      "content": [{"type": "text", "text": "{\"a\":1}\n\nLogs:\nhello"}]},
            "time": {"created": 1000, "ran": 1100, "completed": 2300}}]
    }))
    .unwrap();
    a.store.messages.entry(sid.clone()).or_default().push(m);
    a.on_click(crate::ui::text::Click::Execute("msg_x".into(), "call_x".into()));
    let lines = render(&mut a, 140, 40);
    let joined = lines.join("\n");
    // XLarge dialog at x=12: header at the inset of 3, status before `esc` (gap 2).
    let header = row(&lines, "execute");
    assert_eq!(col(header, "execute"), 15, "{joined}");
    assert!(header.contains("Completed · 1.2s  esc"), "{joined}");
    assert_eq!(col(row(&lines, "Code"), "Code"), 15);
    assert!(contains(&lines, "1 const a = 1"), "{joined}");
    assert!(contains(&lines, "2 return a"), "{joined}");
    assert!(contains(&lines, "1 {\"a\":1}"), "{joined}");
    assert_eq!(col(row(&lines, "Logs:"), "Logs:"), 17, "{joined}");
    assert!(contains(&lines, "scroll ↑/↓  copy code c  copy output o  back esc"), "{joined}");
    a.on_key(Stroke::key("c"));
    assert_eq!(clipboard().as_deref(), Some("const a = 1\nreturn a"));
    let lines = render(&mut a, 140, 40);
    assert!(contains(&lines, "scroll ↑/↓  ✓ copied  copy output o  back esc"), "{}", lines.join("\n"));
    a.on_key(Stroke::key("escape"));
    assert!(a.dialogs.is_empty());
}

#[test]
fn plugins_dialog() {
    let mut a = app();
    a.run_command("plugins.list", None);
    let lines = render(&mut a, 100, 30);
    let joined = lines.join("\n");
    assert!(contains(&lines, "No items available"), "{joined}");
    assert!(row(&lines, "show internal").contains("show internal ctrl+a"), "{joined}");
    // ctrl+a shows the built-in (internal) TUI entries.
    a.on_key(Stroke::ctrl("a"));
    let lines = render(&mut a, 100, 30);
    let joined = lines.join("\n");
    assert!(contains(&lines, "TUI") && contains(&lines, "opencode.btw") && contains(&lines, "opencode.sidebar.mcp"), "{joined}");
    assert!(row(&lines, "hide internal ctrl+a").trim_end().ends_with("disable enter"), "{joined}");
    // Enter disables the focused built-in for this run.
    a.on_key(Stroke::key("return"));
    assert!(!a.builtin_active("opencode.btw"));
    let lines = render(&mut a, 100, 30);
    assert!(row(&lines, "opencode.btw").trim_end().ends_with("inactive"), "{}", lines.join("\n"));
    assert!(row(&lines, "hide internal ctrl+a").trim_end().ends_with("enable enter"));
    assert!(!a.commands().iter().any(|c| c.id == "session.aside"));
    a.on_key(Stroke::key("return"));
    assert!(a.builtin_active("opencode.btw"));
    // Server plugins: a failed one shows its error.
    a.plugins.server = Some(vec![
        json!({"id": "acme", "source": {"type": "package", "target": "acme@1.0.0", "version": "1.0.0", "outdated": true}, "features": {}, "state": {"status": "active"}}),
        json!({"id": "broken", "source": {"type": "local", "path": "/work/proj/plugin.ts"}, "features": {}, "state": {"status": "failed", "error": "boom", "ref": "ref_1"}}),
    ]);
    a.on_key(Stroke::ctrl("a"));
    let lines = render(&mut a, 100, 30);
    let joined = lines.join("\n");
    assert!(row(&lines, "acme").trim_end().ends_with("1.0.0, update available"), "{joined}");
    assert!(row(&lines, "broken").trim_end().ends_with("failed, local"), "{joined}");
    assert!(row(&lines, "check for updates").contains("check for updates ctrl+r"), "{joined}");
    a.on_key(Stroke::key("down"));
    let lines = render(&mut a, 100, 30);
    assert!(row(&lines, "show internal ctrl+a").trim_end().ends_with("view error enter"), "{}", lines.join("\n"));
    a.on_key(Stroke::key("return"));
    let lines = render(&mut a, 100, 30);
    let joined = lines.join("\n");
    let y = lines.iter().position(|l| l.contains("Server plugin error")).unwrap_or_else(|| panic!("{joined}"));
    assert!(lines[y].contains("esc"));
    assert!(lines[y + 1].contains("plugin.ts"), "{joined}");
    assert!(lines[y + 3].contains("boom"), "{joined}");
    assert!(contains(&lines, "Reference: ref_1"), "{joined}");
    assert!(contains(&lines, "investigate i  copy details c"), "{joined}");
    a.on_key(Stroke::key("c"));
    let copied = clipboard().unwrap();
    assert!(copied.starts_with("Server plugin error\nPlugin: broken\nStatus: failed\nRuntime: server\nSource: "), "{copied}");
    assert!(copied.ends_with("Error: boom\nReference: ref_1"), "{copied}");
    a.on_key(Stroke::key("escape"));
    assert_eq!(a.dialogs.last().map(|d| d.purpose.as_str()), Some("plugins"));
}
