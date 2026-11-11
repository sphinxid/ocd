//! The 2.2.7 catch-up of the terminal UI: Ctrl+Z with dialogs and forms (#62118), the clipboard read error
//! (#61094), keypad keys (#59770), the model picker (#62244, #62927), exports in the working directory (#62253),
//! the open menu's exact-ID lookup and archived sessions (#62262, #61270), dialog buttons (#62906), the toast over
//! the cursor (#60249), delivered pending prompts (#61679), message spacing (#62104) and "Scroll on send".
//! Server flows run against a small mock server that logs every request with its query.

use super::tests::{app, render, session};
use super::*;
use crate::keymap::{Match, Stroke};
use crate::ui::dialog::{Body, Dialog, DialogHit};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseEvent, MouseEventKind};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

type Handler = Arc<dyn Fn(&str, &str) -> (u16, Value) + Send + Sync>;

#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<(String, String)>>>);

impl Log {
    /// `METHOD path?query` of every request.
    fn calls(&self) -> Vec<String> {
        self.0.lock().unwrap().iter().map(|(m, t)| format!("{m} {t}")).collect()
    }
}

/// A one-request-per-connection HTTP server answering with `route(method, path)`.
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
                let head = loop {
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break String::from_utf8_lossy(&buf[..end]).to_string();
                    }
                };
                let line = head.lines().next().unwrap_or("").to_string();
                let mut it = line.split(' ');
                let method = it.next().unwrap_or("").to_string();
                let target = it.next().unwrap_or("").to_string();
                let path = target.split('?').next().unwrap_or("").to_string();
                log.0.lock().unwrap().push((method.clone(), target));
                let (status, out) = route(&method, &path);
                let text = if out.is_null() { String::new() } else { out.to_string() };
                let resp =
                    format!("HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}", text.len());
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

fn info(id: &str, title: &str, updated: i64) -> Value {
    json!({ "id": id, "projectID": "prj_1", "title": title, "location": { "directory": "/work/proj" }, "time": { "created": updated, "updated": updated } })
}

fn titles(a: &mut App) -> Vec<String> {
    a.dialogs.last_mut().and_then(|d| d.sel()).map(|s| s.visible().iter().map(|i| s.options[*i].title.clone()).collect()).unwrap_or_default()
}

// ---- #62118: Ctrl+Z suspends while a dialog or form is open ----------------------------------------------------

// Windows has no terminal suspend, so ctrl+z is not bound there.
#[cfg(not(windows))]
#[test]
fn ctrl_z_suspends_with_a_dialog_or_a_form_open() {
    let mut a = app();
    let sid = session(&mut a);
    a.run_command("command.palette.show", None);
    assert!(!a.dialogs.is_empty());
    a.on_key(Stroke::ctrl("z"));
    assert!(a.suspend_requested, "global binding");
    a.suspend_requested = false;
    a.dialogs.clear();
    a.store.forms.insert(
        sid.clone(),
        vec![(
            FormInfo {
                id: "frm_1".into(),
                session_id: sid,
                title: "Name".into(),
                fields: vec![json!({ "key": "name", "type": "string", "title": "Name" })],
                ..Default::default()
            },
            None,
        )],
    );
    render(&mut a, 100, 40);
    assert!(a.active_form().is_some());
    a.on_key(Stroke::ctrl("z"));
    assert!(a.suspend_requested, "a focused form text field does not swallow it");
}

// ---- #61094: a clipboard that cannot be read says so ----------------------------------------------------------

#[test]
fn an_unreadable_clipboard_shows_an_error_toast() {
    let mut a = app();
    crate::clipboard::TEST_READ.with(|c| *c.borrow_mut() = Some(Err(crate::clipboard::READ_UNSUPPORTED)));
    a.run_command("prompt.paste", None);
    let t = a.toast.clone().expect("toast");
    assert_eq!((t.variant.as_str(), t.message.as_str()), ("error", "Clipboard read is not supported on this system"));
    a.toast = None;
    crate::clipboard::TEST_READ.with(|c| *c.borrow_mut() = Some(Ok(Some("pasted".into()))));
    a.run_command("prompt.paste", None);
    assert!(a.toast.is_none());
    assert_eq!(a.editor.text, "pasted");
    crate::clipboard::TEST_READ.with(|c| *c.borrow_mut() = None);
}

// ---- #59770: keypad Home/End/PgUp/PgDn --------------------------------------------------------------------------

#[test]
fn keypad_keys_have_their_own_names_and_default_bindings() {
    let kp = |code| {
        crate::keymap::stroke_from_event(&KeyEvent::new_with_kind_and_state(code, KeyModifiers::NONE, KeyEventKind::Press, KeyEventState::KEYPAD)).unwrap()
    };
    let plain = |code| crate::keymap::stroke_from_event(&KeyEvent::new(code, KeyModifiers::NONE)).unwrap();
    assert_eq!(kp(KeyCode::Home).name, "kphome");
    assert_eq!(kp(KeyCode::End).name, "kpend");
    assert_eq!(kp(KeyCode::PageUp).name, "kppageup");
    assert_eq!(kp(KeyCode::PageDown).name, "kppagedown");
    assert_eq!(plain(KeyCode::Home).name, "home");
    let kb = crate::keymap::Keybinds::new(None, 2000, true);
    for (id, code) in
        [("session.first", KeyCode::Home), ("session.last", KeyCode::End), ("session.page.up", KeyCode::PageUp), ("session.page.down", KeyCode::PageDown)]
    {
        assert_eq!(kb.match_command(id, &[kp(code)]), Match::Full, "{id}");
        assert_eq!(kb.match_command(id, &[plain(code)]), Match::Full, "{id}");
    }
    // `home` does not match the keypad key (OpenTUI keeps them apart).
    assert_eq!(kb.match_command("dialog.select.home", &[kp(KeyCode::Home)]), Match::None);
    assert_eq!(crate::keymap::parse_bindings("kppageup").unwrap(), vec![vec![Stroke::key("kppageup")]]);
    assert_eq!(kb.shortcut("dialog.integration.copy"), "tab");
    // Text inputs treat them as their base keys.
    let mut a = app();
    let mut p = crate::ui::dialog::PromptDialog::new("Rename", "Name", "abc");
    p.cursor = 3;
    a.open_dialog(Dialog::new("rename", Body::Prompt(p)));
    a.on_key(kp(KeyCode::Home));
    assert!(matches!(&a.dialogs.last().unwrap().body, Body::Prompt(p) if p.cursor == 0));
}

// ---- #62244, #62927: the model picker -----------------------------------------------------------------------------

#[test]
fn favorites_lead_the_unconnected_model_list_from_the_snapshot_at_open() {
    let mut a = app();
    let model = |id: &str, name: &str| ModelInfo {
        id: id.into(),
        provider_id: "mock".into(),
        name: name.into(),
        status: "active".into(),
        enabled: true,
        ..Default::default()
    };
    let loc = a.store.location_mut("/work/proj");
    loc.models = Some(vec![model("a", "Alpha"), model("b", "Beta"), model("c", "Gamma")]);
    loc.providers = Some(vec![ProviderInfo { id: "mock".into(), name: "Mock".into(), ..Default::default() }]);
    a.models.set_favorite("mock", "c", true);
    a.run_command("model.list", None);
    assert_eq!(titles(&mut a), ["Gamma", "Alpha", "Beta"]);
    let lines = render(&mut a, 100, 30);
    assert!(lines.iter().any(|l| l.contains("Gamma") && l.contains("(Favorite)")), "{}", lines.join("\n"));
    // Ctrl+F on Beta: marked at once, but the order keeps the snapshot taken at open.
    a.on_key(Stroke::key("down"));
    a.on_key(Stroke::key("down"));
    a.on_key(Stroke::ctrl("f"));
    assert!(a.models.is_favorite("mock", "b"));
    assert_eq!(titles(&mut a), ["Gamma", "Alpha", "Beta"]);
    let lines = render(&mut a, 100, 30);
    assert!(lines.iter().any(|l| l.contains("Beta") && l.contains("(Favorite)")), "{}", lines.join("\n"));
    a.dialogs.clear();
    a.run_command("model.list", None);
    // Reopened: both favorites first, in the server's order.
    assert_eq!(titles(&mut a), ["Beta", "Gamma", "Alpha"]);
}

// ---- #62253: exports go to the working directory ---------------------------------------------------------------

#[test]
fn exports_are_named_session_uuid_in_the_working_directory() {
    let p = super::commands::export_path(std::path::Path::new("/work/proj"), "md");
    let name = p.file_name().unwrap().to_str().unwrap().to_string();
    assert_eq!(p.parent().unwrap(), std::path::Path::new("/work/proj"));
    let re = regex::Regex::new(r"^session-[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}\.md$").unwrap();
    assert!(re.is_match(&name), "{name}");
    assert_ne!(super::commands::random_uuid(), super::commands::random_uuid());
}

// ---- #62262, #61270: the open menu --------------------------------------------------------------------------------

const CHILD: &str = "ses_0123456789abcdefghijklmnop";
const ARCHIVED: &str = "ses_ABCDEFGHIJKLMNOPQRSTUVWXYZ";

fn open_routes() -> Handler {
    Arc::new(|method: &str, path: &str| match (method, path) {
        ("GET", "/api/session") => (200, json!({ "data": [info("ses_recent", "Recent work", 5)], "cursor": {} })),
        ("GET", p) if p == format!("/api/session/{CHILD}") => {
            let mut s = info(CHILD, "Child task", 4);
            s["parentID"] = json!("ses_parent");
            (200, json!({ "data": s }))
        }
        ("GET", p) if p == format!("/api/session/{ARCHIVED}") => {
            let mut s = info(ARCHIVED, "Old archived", 3);
            s["time"]["archived"] = json!(9);
            (200, json!({ "data": s }))
        }
        _ => (404, json!({ "_tag": "NotFound", "message": "no route" })),
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_open_menu_lists_its_own_recents_and_finds_a_session_by_exact_id() {
    let (mut a, mut rx, log) = flow(open_routes()).await;
    let mut archived: SessionInfo = serde_json::from_value(info("ses_stored_archived", "Stored archived", 9)).unwrap();
    archived.time.archived = Some(10);
    a.store.remember(archived);
    a.run_command("open.menu", None);
    drive_until(&mut a, &mut rx, 3000, |a| !a.open_sessions.recent.is_empty()).await;
    assert!(log.calls().iter().any(|c| c == "GET /api/session?limit=50&order=desc&parentID=null&archived=false"), "{:?}", log.calls());
    let shown = titles(&mut a);
    assert!(shown.contains(&"Recent work".to_string()), "{shown:?}");
    assert!(!shown.contains(&"Stored archived".to_string()), "archived sessions stay out: {shown:?}");
    // The exact ID of a child session lists it.
    for c in CHILD.chars() {
        a.on_key(Stroke::key(&c.to_string()));
    }
    drive_until(&mut a, &mut rx, 3000, |a| a.open_sessions.matched.is_some()).await;
    assert!(log.calls().iter().any(|c| c == &format!("GET /api/session/{CHILD}")), "{:?}", log.calls());
    assert_eq!(titles(&mut a), ["Child task"]);
    // So does an archived one; a partial ID looks nothing up.
    a.on_key(Stroke::ctrl("u"));
    while a.dialogs.last_mut().and_then(|d| d.sel()).is_some_and(|s| !s.filter.is_empty()) {
        a.on_key(Stroke::key("backspace"));
    }
    for c in ARCHIVED.chars() {
        a.on_key(Stroke::key(&c.to_string()));
    }
    drive_until(&mut a, &mut rx, 3000, |a| a.open_sessions.matched.as_ref().is_some_and(|m| m.id == ARCHIVED)).await;
    assert_eq!(titles(&mut a), ["Old archived"]);
    let gets = log.calls().iter().filter(|c| c.starts_with("GET /api/session/ses_")).count();
    a.on_key(Stroke::key("backspace"));
    drive_until(&mut a, &mut rx, 300, |_| false).await;
    assert_eq!(log.calls().iter().filter(|c| c.starts_with("GET /api/session/ses_")).count(), gets);
    assert!(a.open_sessions.matched.is_none());
}

#[test]
fn archived_sessions_stay_out_of_the_session_list() {
    let mut a = app();
    a.store.remember(serde_json::from_value(info("ses_live", "Live one", 5)).unwrap());
    let mut archived: SessionInfo = serde_json::from_value(info("ses_gone", "Archived one", 6)).unwrap();
    archived.time.archived = Some(7);
    a.store.remember(archived);
    a.store.location_mut("/work/proj").info = Some(LocationInfo {
        directory: "/work/proj".into(),
        project: ProjectRef { id: "prj_1".into(), directory: "/work/proj".into(), canonical: "/work/proj".into() },
    });
    a.open_session_list();
    a.session_search.loading = false;
    a.refresh_session_list();
    let lines = render(&mut a, 100, 30);
    let joined = lines.join("\n");
    assert!(joined.contains("Live one") && !joined.contains("Archived one"), "{joined}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn continue_skips_archived_sessions() {
    let (mut a, mut rx, log) = flow(open_routes()).await;
    a.opts.continue_last = true;
    a.post_mount();
    drive_until(&mut a, &mut rx, 3000, |a| a.session_id().is_some()).await;
    assert!(log.calls().iter().any(|c| c == "GET /api/session?limit=1&order=desc&parentID=null&archived=false&directory=%2Fwork%2Fproj"), "{:?}", log.calls());
    assert_eq!(a.session_id(), Some("ses_recent"));
}

// ---- #62906: dialog buttons ---------------------------------------------------------------------------------------

fn buttons(a: &App) -> (Vec<String>, usize) {
    match a.dialogs.last().map(|d| &d.body) {
        Some(Body::Text(td)) => td.buttons.clone().map(|b| (b.labels, b.active)).unwrap_or_default(),
        _ => Default::default(),
    }
}

fn hit(a: &App, want: &DialogHit) -> (u16, u16) {
    let (r, _) = a.dialog_hits.iter().find(|(_, h)| h == want).unwrap_or_else(|| panic!("{want:?} in {:?}", a.dialog_hits));
    (r.x, r.y)
}

fn mouse(kind: MouseEventKind, (column, row): (u16, u16)) -> MouseEvent {
    MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE }
}

#[test]
fn the_update_dialog_has_buttons_moved_with_arrows_and_tab() {
    let mut a = app();
    a.update.state = Some(super::update::UpdateState::Available("0.2.0".into()));
    a.open_dialog(a.update_dialog());
    assert_eq!(buttons(&a), (vec!["Skip".to_string(), "Update".to_string()], 1));
    for (key, shift, active) in [("left", false, 0), ("tab", false, 1), ("tab", true, 0), ("right", false, 1), ("right", false, 0), ("left", false, 1)] {
        let mut s = Stroke::key(key);
        s.shift = shift;
        a.on_key(s);
        assert_eq!(buttons(&a).1, active, "{key} shift={shift}");
    }
    // Layout: header, blank, body, blank, buttons, blank; the active button is highlighted, no `enter` hint.
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
    term.draw(|f| a.draw(f)).unwrap();
    let buf = term.backend().buffer().clone();
    let r = a.dialog_rect.unwrap();
    let row = |y: u16| (r.x..r.right()).map(|x| buf[(x, y)].symbol()).collect::<String>();
    let title = (0..30).find(|y| row(*y).contains("Update available")).unwrap();
    assert!(row(title + 1).trim().is_empty());
    assert!(row(title + 2).contains("An update is available."));
    let body_end = (title + 2..30).find(|y| row(*y).trim().is_empty()).unwrap();
    assert!(row(body_end + 1).contains(" Skip  Update "), "{}", row(body_end + 1));
    assert!(row(body_end + 2).trim().is_empty());
    let t = a.theme().dialog().clone();
    let x = r.x + row(body_end + 1).find(" Update ").unwrap() as u16 + 1;
    assert_eq!(buf[(x, body_end + 1)].bg, t.background.action.primary.focused.tui());
    // Hovering makes a button active; clicking runs it.
    a.on_mouse(mouse(MouseEventKind::Moved, hit(&a, &DialogHit::Button(0))));
    assert_eq!(buttons(&a).1, 0);
    a.on_mouse(mouse(MouseEventKind::Up(crossterm::event::MouseButton::Left), hit(&a, &DialogHit::Button(0))));
    assert!(a.dialogs.is_empty(), "Skip closes");
}

#[test]
fn the_active_update_button_resets_only_when_the_buttons_change() {
    let mut a = app();
    a.update.state = Some(super::update::UpdateState::Available("0.2.0".into()));
    a.open_dialog(a.update_dialog());
    a.on_key(Stroke::key("left"));
    a.refresh_update_dialog();
    assert_eq!(buttons(&a).1, 0, "same buttons keep the active one");
    a.update.state = Some(super::update::UpdateState::Installed("0.2.0".into()));
    a.refresh_update_dialog();
    assert_eq!(buttons(&a), (vec!["Skip".to_string(), "Restart".to_string()], 1));
    a.update.state = Some(super::update::UpdateState::Installing("0.2.0".into()));
    a.refresh_update_dialog();
    assert_eq!(buttons(&a), (vec![], 0));
}

#[test]
fn the_confirm_dialog_toggles_with_tab_and_answers_clicks() {
    let mut a = app();
    let confirm = |a: &App| match a.dialogs.last().map(|d| &d.body) {
        Some(Body::Confirm { confirm, .. }) => Some(*confirm),
        _ => None,
    };
    a.open_dialog(Dialog::new("confirm", Body::Confirm { title: "Delete worktree?".into(), message: "Sure?".into(), confirm: true }));
    a.on_key(Stroke::key("tab"));
    assert_eq!(confirm(&a), Some(false));
    let mut s = Stroke::key("tab");
    s.shift = true;
    a.on_key(s);
    assert_eq!(confirm(&a), Some(true));
    render(&mut a, 100, 30);
    a.on_mouse(mouse(MouseEventKind::Moved, hit(&a, &DialogHit::Button(0))));
    assert_eq!(confirm(&a), Some(false));
    a.on_mouse(mouse(MouseEventKind::Up(crossterm::event::MouseButton::Left), hit(&a, &DialogHit::Button(0))));
    assert!(a.dialogs.is_empty());
}

// ---- #60249: a toast hides the terminal cursor it covers ---------------------------------------------------------

#[test]
fn a_toast_over_the_prompt_cursor_hides_it() {
    let mut a = app();
    session(&mut a);
    let mut covered = 0;
    for (w, h) in [(40u16, 8u16), (40, 9), (40, 10), (50, 9), (100, 30)] {
        a.toast = None;
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        term.draw(|f| a.draw(f)).unwrap();
        if a.cursor_look.is_none() {
            continue;
        }
        let p = ratatui::backend::Backend::get_cursor_position(term.backend_mut()).unwrap();
        let cursor = (p.x, p.y);
        a.toast = Some(Toast::new("info", "x ".repeat(80)));
        render(&mut a, w, h);
        let rect = a.hits.iter().find(|(_, c)| matches!(c, crate::ui::text::Click::Custom(c) if c == super::view::TOAST_CLICK)).map(|(r, _)| *r).unwrap();
        let inside = cursor.0 >= rect.x && cursor.0 < rect.right() && cursor.1 >= rect.y && cursor.1 < rect.bottom();
        covered += usize::from(inside);
        assert_eq!(a.cursor_look.is_none(), inside, "{w}x{h} cursor {cursor:?} toast {rect:?}");
    }
    assert!(covered > 0, "one size puts the toast over the cursor");
}

// ---- #61679: delivered pending prompts are cleared --------------------------------------------------------------

#[test]
fn a_fetched_transcript_clears_pending_prompts_it_contains() {
    let mut a = app();
    let sid = session(&mut a);
    let item = |id: &str| InboxItem {
        id: id.into(),
        session_id: sid.clone(),
        kind: "user".into(),
        delivery: "steer".into(),
        payload: json!({ "text": "hi" }),
        ..Default::default()
    };
    a.store.pending.insert(sid.clone(), vec![item("msg_p1"), item("msg_p2")]);
    a.store.outbox.insert("msg_p1".into());
    let fetched: Vec<Message> = serde_json::from_value(json!([{ "id": "msg_p1", "type": "user", "text": "hi", "time": { "created": 5 } }])).unwrap();
    a.store.apply_messages(&sid, fetched, None);
    assert!(!a.store.outbox.contains("msg_p1"));
    assert_eq!(a.store.pending[&sid].iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), ["msg_p2"]);
}

// ---- #62104: right padding on user messages and tool blocks ------------------------------------------------------

#[test]
fn user_messages_keep_two_cells_of_right_padding() {
    let mut a = app();
    let sid = session(&mut a);
    let text = "word ".repeat(60);
    a.store
        .messages
        .get_mut(&sid)
        .unwrap()
        .push(serde_json::from_value(json!({ "id": "msg_9", "type": "user", "text": text.trim(), "time": { "created": 90000 } })).unwrap());
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(70, 60)).unwrap();
    term.draw(|f| a.draw(f)).unwrap();
    let buf = term.backend().buffer().clone();
    let bg = a.theme().background.raised.base.tui();
    let mut rows = 0;
    for y in 0..60u16 {
        let line: String = (0..70).map(|x| buf[(x, y)].symbol()).collect();
        if !line.contains("┃") || !line.contains("word word") {
            continue;
        }
        rows += 1;
        let last = (0..70u16).rev().find(|x| buf[(*x, y)].symbol() == "d").unwrap();
        let end = (last..70u16).take_while(|x| buf[(*x, y)].bg == bg).last().unwrap();
        assert!(end - last >= 2, "row {y}: {line:?}");
    }
    assert!(rows >= 4);
}

// ---- Scroll on send ----------------------------------------------------------------------------------------------

#[test]
fn scroll_on_send_keep_leaves_the_transcript_where_it_is() {
    let mut a = app();
    let sid = session(&mut a);
    assert!(super::commands::settings().iter().any(|s| s.1 == "Scroll on send" && s.2 == "session.submit_scroll"));
    a.view(&sid).scroll_from_bottom = 7;
    a.config.set("session.submit_scroll", Some(json!("keep")));
    a.paste("hello");
    a.submit("steer");
    assert_eq!(a.view(&sid).scroll_from_bottom, 7);
    a.config.set("session.submit_scroll", None);
    a.paste("again");
    a.submit("steer");
    assert_eq!(a.view(&sid).scroll_from_bottom, 0);
}

// ---- #62389: the docs command ------------------------------------------------------------------------------------

#[test]
fn the_docs_command_opens_the_docs() {
    let mut a = app();
    super::commands::TEST_OPENED.with(|o| o.borrow_mut().clear());
    a.run_command("docs.open", None);
    assert_eq!(super::commands::TEST_OPENED.with(|o| o.borrow().clone()), ["https://opencode.ai/docs"]);
}
