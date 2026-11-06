//! The 2.2.5 catch-up of the terminal UI: diff padding (#60614), the cursor style and color (#60302), persistent
//! terminal refreshes and strict messages (#60346), missing-session eviction (#60459), undo / revert / queued undo
//! giving the prompt back (#60116), and the session-tab pulse (#60338). Flows run against a small mock server.

use super::tests::{app, render, session};
use super::*;
use crate::keymap::Stroke;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

type Handler = Arc<dyn Fn(&str, &str) -> (u16, Value) + Send + Sync>;

#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<(String, String, Value)>>>);

impl Log {
    fn calls(&self) -> Vec<String> {
        self.0.lock().unwrap().iter().map(|(m, p, _)| format!("{m} {p}")).collect()
    }
}

/// A one-request-per-connection HTTP server answering with `route(method, path)` and logging every request.
async fn serve(route: Handler, log: Log) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let (route, log) = (route.clone(), log.clone());
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = vec![0u8; 8192];
                let (head, body) = loop {
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..end]).to_string();
                        let len = head.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0))).unwrap_or(0);
                        while buf.len() < end + 4 + len {
                            let n = sock.read(&mut chunk).await.unwrap_or(0);
                            if n == 0 {
                                break;
                            }
                            buf.extend_from_slice(&chunk[..n]);
                        }
                        break (head, buf[end + 4..].to_vec());
                    }
                };
                let line = head.lines().next().unwrap_or("").to_string();
                let mut it = line.split(' ');
                let method = it.next().unwrap_or("").to_string();
                let target = it.next().unwrap_or("").to_string();
                let path = target.split('?').next().unwrap_or("").to_string();
                log.0.lock().unwrap().push((method.clone(), path.clone(), serde_json::from_slice(&body).unwrap_or(Value::Null)));
                let (status, out) = route(&method, &path);
                let text = if out.is_null() { String::new() } else { out.to_string() };
                let resp = format!("HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}", text.len());
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });
    url
}

async fn flow(route: Handler) -> (App, tokio::sync::mpsc::UnboundedReceiver<AppMsg>, Log) {
    let log = Log::default();
    let url = serve(route, log.clone()).await;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut a = app();
    a.tx = tx;
    a.client = crate::api::Client::new(&url, None, "/work/proj");
    (a, rx, log)
}

async fn drive_until(a: &mut App, rx: &mut tokio::sync::mpsc::UnboundedReceiver<AppMsg>, ms: u64, done: impl Fn(&App) -> bool) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(ms);
    while !done(a) {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(AppMsg::Apply(f))) => f(a),
            Ok(Some(_)) => {}
            _ => break,
        }
    }
}

fn ok_routes() -> Handler {
    Arc::new(|method: &str, path: &str| match (method, path) {
        ("POST", p) if p.ends_with("/interrupt") => (200, json!({ "data": true })),
        ("POST", p) if p.ends_with("/revert/stage") => (200, json!({ "data": { "messageID": "msg_1" } })),
        _ => (200, json!({ "data": null })),
    })
}

fn event(kind: &str, data: Value) -> Event {
    Event { id: format!("evt_{}", crate::util::id_body(false)), created: 1, kind: kind.into(), data, ..Default::default() }
}

// ---- #60614: edit and patch diffs keep 3 cells of right padding --------------------------------------------

#[test]
fn edit_diffs_leave_the_block_background_at_both_edges() {
    let mut a = app();
    session(&mut a);
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 40)).unwrap();
    term.draw(|f| a.draw(f)).unwrap();
    let buf = term.backend().buffer().clone();
    let t = a.theme().clone();
    let block = t.background.raised.base.tui();
    let y = (0..40).find(|y| (0..100).map(|x| buf[(x, *y)].symbol()).collect::<String>().contains("- b")).expect("removed row");
    let row: String = (0..100).map(|x| buf[(x, y)].symbol()).collect();
    let bar = row.find('┃').unwrap() as u16;
    // `┃`, 2 cells of BlockTool padding and the box's `paddingLeft`: the block's background.
    for x in bar + 1..=bar + 3 {
        assert_eq!(buf[(x, y)].bg, block, "x={x} {row:?}");
    }
    // The gutter: `paddingLeft` 1, the number, ` -`, a space, all on the line-number background.
    let gutter: String = (bar + 4..bar + 9).map(|x| buf[(x, y)].symbol()).collect();
    assert_eq!(gutter, " 2 - ");
    for x in bar + 4..bar + 9 {
        assert_eq!(buf[(x, y)].bg, t.diff.line_number_bg_removed.tui(), "x={x}");
    }
    assert_eq!(buf[(bar + 9, y)].bg, t.diff.bg_removed.tui());
    // The last 3 cells before the transcript's edge show the block, not the diff row.
    let end = (bar..100).rev().find(|x| buf[(*x, y)].bg == t.diff.bg_removed.tui()).unwrap();
    for x in end + 1..=end + 3 {
        assert_eq!(buf[(x, y)].bg, block, "x={x} {row:?}");
    }
}

// ---- #60302: DECSCUSR and the cursor color ---------------------------------------------------------------------

#[test]
fn the_cursor_takes_the_configured_style_and_the_focused_input_color() {
    let mut a = app();
    let t = a.theme().clone();
    render(&mut a, 100, 30);
    // A missing `cursor` key is a blinking block (1); the prompt's cursor is `text.base`.
    assert_eq!(a.cursor_look, Some((1, t.text.base)));
    // The command palette's search input: `text.formfield.focused` of the dialog surface.
    a.run_command("command.palette.show", None);
    render(&mut a, 100, 30);
    assert_eq!(a.cursor_look, Some((1, t.dialog().text.formfield.focused)));
    a.dialogs.clear();
    for (cursor, code) in [(json!({ "style": "line", "blinking": false }), 6), (json!({ "style": "underline" }), 3), (json!({ "style": "default" }), 0), (json!({ "blinking": false }), 2)] {
        a.config.set("cursor", Some(cursor));
        render(&mut a, 100, 30);
        assert_eq!(a.cursor_look.map(|l| l.0), Some(code));
    }
    // Deleting the key on a live reload brings back the blinking block.
    a.config.set("cursor", None);
    render(&mut a, 100, 30);
    assert_eq!(a.cursor_look.map(|l| l.0), Some(1));
}

#[test]
fn cursor_sequences_are_sent_only_when_they_change() {
    let mut sent = CursorSent::default();
    let base = crate::theme::Rgba::rgb(0xee, 0xee, 0xee);
    let focused = crate::theme::Rgba::rgb(0xfa, 0xb2, 0x83);
    assert_eq!(sent.update(Some((1, base))), "\x1b]12;#eeeeee\x07\x1b[1 q");
    assert_eq!(sent.update(Some((1, base))), "");
    assert_eq!(sent.update(None), "");
    assert_eq!(sent.update(Some((1, focused))), "\x1b]12;#fab283\x07");
    assert_eq!(sent.update(Some((6, focused))), "\x1b[6 q");
    assert_eq!(CURSOR_RESET, "\x1b]12;default\x07\x1b]112\x07\x1b[0 q");
}

// ---- #60346: persistent terminals ---------------------------------------------------------------------------------

#[test]
fn terminal_messages_that_do_not_match_their_schema_are_ignored() {
    use super::terminal::valid_message;
    assert!(valid_message(&json!({ "type": "attached", "inputProtocol": 1, "info": { "size": { "cols": 80, "rows": 24 } }, "role": "controller" })));
    assert!(valid_message(&json!({ "type": "attached", "info": { "size": { "cols": 80, "rows": 24 }, "extra": 1 } })));
    assert!(!valid_message(&json!({ "type": "attached", "inputProtocol": 1 })));
    assert!(!valid_message(&json!({ "type": "attached", "inputProtocol": 1, "info": { "size": { "cols": 0, "rows": 24 } } })));
    assert!(!valid_message(&json!({ "type": "attached", "inputProtocol": 1, "info": { "size": { "cols": 8.5, "rows": 24 } } })));
    assert!(!valid_message(&json!({ "type": "attached", "inputProtocol": 1, "info": { "size": { "cols": 80, "rows": 24 } }, "role": "owner" })));
    assert!(!valid_message(&json!({ "type": "attached", "inputProtocol": "1", "info": { "size": { "cols": 80, "rows": 24 } } })));
    assert!(valid_message(&json!({ "type": "resized", "cols": 100, "rows": 30, "checkpoint": "" })));
    assert!(!valid_message(&json!({ "type": "resized", "cols": 100, "rows": 30 })));
    assert!(valid_message(&json!({ "type": "controller_changed" })));
    assert!(valid_message(&json!({ "type": "controller_changed", "attachmentID": null })));
    assert!(!valid_message(&json!({ "type": "controller_changed", "attachmentID": 3 })));
    assert!(valid_message(&json!({ "type": "exited", "exitCode": null })));
    assert!(!valid_message(&json!({ "type": "exited", "exitCode": "1" })));
    assert!(valid_message(&json!({ "type": "replay_complete" })));
    assert!(valid_message(&json!({ "type": "title_changed", "title": "vim" })));
    assert!(!valid_message(&json!({ "type": "foreground_process_changed" })));
    assert!(!valid_message(&json!({ "type": "unknown" })));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_persistent_terminal_update_refreshes_the_session_terminals() {
    let route: Handler = Arc::new(|_: &str, path: &str| {
        if path.ends_with("/terminal") {
            return (200, json!({ "data": [{ "id": "pty_1", "title": "Renamed", "sessionID": "ses_test" }] }));
        }
        (200, json!({ "data": null }))
    });
    let (mut a, mut rx, log) = flow(route).await;
    session(&mut a);
    a.terminal_lists.insert("ses_test".into(), vec![json!({ "id": "pty_1", "title": "Terminal", "sessionID": "ses_test" })]);
    a.on_event(event("persistent-pty.updated", json!({ "sessionID": "ses_test", "id": "pty_1" })));
    drive_until(&mut a, &mut rx, 3000, |a| a.terminal_lists.get("ses_test").is_some_and(|l| l[0]["title"] == "Renamed")).await;
    assert_eq!(a.terminal_lists["ses_test"][0]["title"], "Renamed");
    assert!(log.calls().contains(&"GET /api/experimental/session/ses_test/terminal".to_string()));
    // A session whose terminals were never listed is left alone.
    a.on_event(event("persistent-pty.updated", json!({ "sessionID": "ses_other", "id": "pty_2" })));
    assert!(!a.terminal_lists.contains_key("ses_other"));
}

// ---- #60459: sessions the server no longer has leave the store ---------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_not_found_answer_evicts_the_session() {
    let route: Handler = Arc::new(|_: &str, path: &str| {
        if path.starts_with("/api/session/ses_gone") {
            return (404, json!({ "_tag": "SessionNotFoundError", "sessionID": "ses_gone", "message": "Session not found: ses_gone" }));
        }
        (200, json!({ "data": [] }))
    });
    let (mut a, mut rx, _) = flow(route).await;
    a.store.remember(SessionInfo { id: "ses_gone".into(), ..Default::default() });
    a.fetch_messages("ses_gone");
    drive_until(&mut a, &mut rx, 3000, |a| a.store.is_missing("ses_gone")).await;
    assert!(a.store.is_missing("ses_gone"));
    assert!(a.store.session("ses_gone").is_none());
    // `session.created` clears the mark.
    a.store.apply(&event("session.created", json!({ "sessionID": "ses_gone" })));
    assert!(!a.store.is_missing("ses_gone"));
}

// ---- #60116: undo, revert and queued undo give the prompt back ---------------------------------------------------

#[test]
fn nothing_to_undo_is_an_error() {
    let mut a = app();
    let sid = session(&mut a);
    a.store.messages.insert(sid, vec![]);
    a.run_command("session.undo", None);
    let toast = a.toast.clone().unwrap();
    assert_eq!((toast.variant.as_str(), toast.message.as_str(), toast.duration), ("error", "Nothing to undo", 3000));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undo_interrupts_waits_reverts_and_appends_the_whole_prompt() {
    let (mut a, mut rx, log) = flow(ok_routes()).await;
    let sid = session(&mut a);
    for c in "draft".chars() {
        a.on_key(Stroke::key(&c.to_string()));
    }
    a.run_command("session.undo", None);
    drive_until(&mut a, &mut rx, 3000, |a| a.editor.text.contains("parser")).await;
    // Interrupted even though the session is idle, then waited for and reverted, in that order.
    let calls: Vec<String> = log.calls().into_iter().filter(|c| c.starts_with("POST")).collect();
    assert_eq!(calls, [format!("POST /api/session/{sid}/interrupt"), format!("POST /api/experimental/session/{sid}/wait"), format!("POST /api/session/{sid}/revert/stage")]);
    // The prompt follows the draft after a blank line, with its file mention.
    assert_eq!(a.editor.text, "draft\n\nFix the bug in parser.ts");
    let info = a.editor.info();
    assert_eq!(info.mode.as_deref(), Some("normal"));
    // (the projected file has no mention range in this fixture, so it is not a chip)
    assert!(a.dialogs.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undo_cancels_a_prompt_still_pending_delivery() {
    let (mut a, mut rx, log) = flow(ok_routes()).await;
    let sid = session(&mut a);
    a.store.admit_local(InboxItem { id: "msg_9".into(), session_id: sid.clone(), kind: "user".into(), payload: json!({ "text": "pending one" }), delivery: "steer".into(), time: MessageTime { created: 5, ..Default::default() } });
    a.run_command("session.undo", None);
    drive_until(&mut a, &mut rx, 3000, |a| a.editor.text == "pending one").await;
    assert_eq!(a.editor.text, "pending one");
    assert_eq!(log.calls(), [format!("DELETE /api/session/{sid}/inbox/msg_9")]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_revert_leaves_the_draft_alone() {
    let route: Handler = Arc::new(|_: &str, path: &str| {
        if path.ends_with("/revert/stage") {
            return (400, json!({ "_tag": "InvalidRequestError", "message": "cannot revert" }));
        }
        (200, json!({ "data": null }))
    });
    let (mut a, mut rx, _) = flow(route).await;
    let sid = session(&mut a);
    for c in "keep".chars() {
        a.on_key(Stroke::key(&c.to_string()));
    }
    a.revert_message(&sid, "msg_1");
    assert!(a.dialogs.is_empty());
    drive_until(&mut a, &mut rx, 3000, |a| a.toast.is_some()).await;
    let toast = a.toast.clone().unwrap();
    assert_eq!((toast.variant.as_str(), toast.message.as_str(), toast.duration), ("error", "cannot revert", 5000));
    assert_eq!(a.editor.text, "keep");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_revert_for_another_session_goes_to_its_draft() {
    let (mut a, mut rx, _) = flow(ok_routes()).await;
    let sid = session(&mut a);
    a.revert_message(&sid, "msg_1");
    a.navigate(Route::Home);
    drive_until(&mut a, &mut rx, 3000, |a| a.drafts.contains_key("ses_test")).await;
    assert_eq!(a.drafts["ses_test"].text, "Fix the bug in parser.ts");
    assert_eq!(a.editor.text, "");
    a.navigate(Route::Session(sid));
    assert_eq!(a.editor.text, "Fix the bug in parser.ts");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_undo_needs_normal_mode_and_reports_failures() {
    let route: Handler = Arc::new(|method: &str, _: &str| if method == "DELETE" { (500, json!({ "message": "boom" })) } else { (200, json!({ "data": null })) });
    let (mut a, mut rx, _) = flow(route).await;
    let sid = session(&mut a);
    a.store.admit_local(InboxItem { id: "msg_q".into(), session_id: sid, kind: "user".into(), payload: json!({ "text": "queued" }), delivery: "queue".into(), time: MessageTime { created: 5, ..Default::default() } });
    a.editor.shell = true;
    a.editor.text = "ls".into();
    a.undo_queued("msg_q");
    assert_eq!(a.toast.as_ref().map(|t| t.message.as_str()), Some("Leave shell mode before undoing a queued prompt"));
    a.toast = None;
    a.editor.shell = false;
    a.undo_queued("msg_q");
    drive_until(&mut a, &mut rx, 3000, |a| a.toast.is_some()).await;
    let toast = a.toast.clone().unwrap();
    assert_eq!((toast.title.as_deref(), toast.message.as_str()), (Some("Failed to undo pending prompt"), "boom"));
    assert_eq!(a.editor.text, "ls");
    assert!(a.pending_actions.is_empty());
}

// ---- #60338: the session-tab pulse spans the rail --------------------------------------------------------------------

#[test]
fn a_running_tab_pulses_across_the_whole_rail() {
    let mut a = app();
    a.config = crate::config::Config::from_value(json!({ "tabs": { "mode": "on", "layout": "vertical" }, "animations": true }));
    let sid = session(&mut a);
    a.store.remember(SessionInfo { id: "ses_two".into(), title: Some("Second".into()), ..Default::default() });
    a.tabs = vec!["ses_two".into(), sid.clone()];
    a.store.active.insert(sid.clone(), true);
    render(&mut a, 120, 30);
    std::thread::sleep(std::time::Duration::from_millis(500));
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();
    term.draw(|f| a.draw(f)).unwrap();
    let buf = term.backend().buffer().clone();
    let w = a.rail_width();
    // The separator above the running (second) tab draws `▄` across the full rail width.
    let y = (0..30).find(|y| buf[(0, *y)].symbol() == "▄").expect("separator pulse");
    assert!((0..w).all(|x| buf[(x, y)].symbol() == "▄"));
    assert!(a.animating());
    // A prompt queued into a background tab bumps its pulse counter; the routed one does not.
    a.on_event(event("session.inbox.enqueued", json!({ "sessionID": "ses_two", "inboxID": "msg_x", "item": { "type": "user", "payload": { "text": "hi" }, "delivery": "steer" } })));
    a.on_event(event("session.inbox.enqueued", json!({ "sessionID": sid, "inboxID": "msg_y", "item": { "type": "user", "payload": { "text": "hi" }, "delivery": "steer" } })));
    assert_eq!(a.prompt_pulses.get("ses_two"), Some(&1));
    assert_eq!(a.prompt_pulses.get("ses_test"), None);
}
