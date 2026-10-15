//! Embedded terminal pane over the persistent PTY WebSocket protocol
//! (`src/component/terminal-pane.tsx`, `src/context/session-terminals.tsx`, spec 06 §9.2).

use super::{App, Toast};
use crate::keymap::Stroke;
use crate::theme::{Rgba, Theme, color::rgb_to_oklch};
use crate::ui::text::*;
use futures::{SinkExt, StreamExt};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// `interactionFrame`: `[type:u8][cols:u16 BE][rows:u16 BE][payload…]`.
pub fn interaction_frame(cols: u16, rows: u16, data: Option<&[u8]>) -> Vec<u8> {
    let mut f = Vec::with_capacity(5 + data.map(|d| d.len()).unwrap_or(0));
    f.push(if data.is_some() { 1 } else { 0 });
    f.extend_from_slice(&cols.to_be_bytes());
    f.extend_from_slice(&rows.to_be_bytes());
    if let Some(d) = data {
        f.extend_from_slice(d);
    }
    f
}

/// State of one attached terminal.
pub struct TermState {
    pub id: String,
    pub parser: vt100::Parser,
    pub failure: Option<String>,
    pub tx: Option<UnboundedSender<Vec<u8>>>,
    pub size: (u16, u16),
    pub attached: bool,
    pub controller: bool,
    pub restored: bool,
    pub exited: bool,
    pub pending: Vec<Vec<u8>>,
}

/// Scrollback lines kept by the embedded terminal's parser.
const SCROLLBACK: usize = 1000;

thread_local! {
    /// Set while a guarded vt100 call runs: its panic is expected and handled, so not printed.
    static QUIET_PANIC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Run `f` catching a panic, without the default hook printing it over the screen.
fn quiet_unwind(f: impl FnOnce()) -> bool {
    static HOOK: std::sync::Once = std::sync::Once::new();
    HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if !QUIET_PANIC.with(|q| q.get()) {
                previous(info);
            }
        }));
    });
    QUIET_PANIC.with(|q| q.set(true));
    let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).is_ok();
    QUIET_PANIC.with(|q| q.set(false));
    ok
}

/// vt100 0.15 panics on some ordinary terminal output (insert mode with wide characters, inverted
/// scroll regions, line insertion and deletion near the bottom, ...). Its parser runs on the UI
/// thread, where a panic would end the whole TUI, so a panicking call resets the screen instead.
fn guarded(parser: &mut vt100::Parser, size: (u16, u16), f: impl FnOnce(&mut vt100::Parser)) {
    if !quiet_unwind(|| f(parser)) {
        tracing::warn!("terminal emulator panicked; screen reset");
        *parser = vt100::Parser::new(size.0.max(1), size.1.max(1), SCROLLBACK);
    }
}

impl TermState {
    /// Feed PTY output to the screen.
    pub fn process(&mut self, data: &[u8]) {
        let size = self.parser.screen().size();
        guarded(&mut self.parser, size, |p| p.process(data));
    }

    /// Resize the screen (`rows`, `cols`; zero is treated as one).
    pub fn set_size(&mut self, rows: u16, cols: u16) {
        let size = (rows.max(1), cols.max(1));
        guarded(&mut self.parser, size, |p| p.set_size(size.0, size.1));
    }

    pub(crate) fn new(id: &str) -> TermState {
        TermState {
            id: id.to_string(),
            parser: vt100::Parser::new(24, 80, SCROLLBACK),
            failure: None,
            tx: None,
            size: (80, 24),
            attached: false,
            controller: false,
            restored: false,
            exited: false,
            pending: Vec::new(),
        }
    }

    fn send(&self, frame: Vec<u8>) {
        if self.attached
            && let Some(tx) = &self.tx
        {
            let _ = tx.send(frame);
        }
    }

    pub fn interact(&mut self) {
        if self.restored {
            self.send(interaction_frame(self.size.0, self.size.1, None));
        }
    }

    pub fn input(&mut self, data: &[u8]) {
        if !self.restored {
            self.pending.push(data.to_vec());
            return;
        }
        self.send(interaction_frame(self.size.0, self.size.1, Some(data)));
    }
}

/// Encode a key stroke as terminal input bytes (xterm).
pub fn encode_key(s: &Stroke) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    if s.meta {
        out.push(0x1b);
    }
    let seq: &[u8] = match s.name.as_str() {
        "return" => b"\r",
        "escape" => b"\x1b",
        "tab" if s.shift => b"\x1b[Z",
        "tab" => b"\t",
        "backspace" => b"\x7f",
        "delete" => b"\x1b[3~",
        "insert" => b"\x1b[2~",
        "up" => b"\x1b[A",
        "down" => b"\x1b[B",
        "right" => b"\x1b[C",
        "left" => b"\x1b[D",
        "home" => b"\x1b[H",
        "end" => b"\x1b[F",
        "pageup" => b"\x1b[5~",
        "pagedown" => b"\x1b[6~",
        "space" if s.ctrl => b"\x00",
        "space" => b" ",
        "f1" => b"\x1bOP",
        "f2" => b"\x1bOQ",
        "f3" => b"\x1bOR",
        "f4" => b"\x1bOS",
        name => {
            let mut chars = name.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            if s.ctrl {
                let lc = c.to_ascii_lowercase();
                if lc.is_ascii_lowercase() {
                    out.push(lc as u8 - b'a' + 1);
                    return Some(out);
                }
                match c {
                    '[' => out.push(0x1b),
                    '\\' => out.push(0x1c),
                    ']' => out.push(0x1d),
                    '^' => out.push(0x1e),
                    '_' | '-' => out.push(0x1f),
                    _ => return None,
                }
                return Some(out);
            }
            let c = if s.shift { c.to_ascii_uppercase() } else { c };
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            return Some(out);
        }
    };
    out.extend_from_slice(seq);
    Some(out)
}

fn hue_distance(a: f64, b: f64) -> f64 {
    let d = (a - b).abs();
    d.min(360.0 - d)
}

fn terminal_hue(t: &Theme, target: f64) -> Rgba {
    let mut best: Option<(f64, Rgba)> = None;
    for (_, scale) in &t.hues {
        let c = scale[1];
        if c.a == 0 {
            continue;
        }
        let o = rgb_to_oklch(c.r as f64 / 255.0, c.g as f64 / 255.0, c.b as f64 / 255.0);
        if o.c < 0.03 {
            continue;
        }
        let d = hue_distance(o.h, target);
        if best.is_none_or(|(bd, _)| d < bd) {
            best = Some((d, c));
        }
    }
    match best {
        Some((d, c)) if d <= 30.0 => c,
        _ => t.accent(200),
    }
}

/// `terminalPalette`: the 16 ANSI colors derived from the theme.
pub fn terminal_palette(t: &Theme, dark: bool) -> [Rgba; 16] {
    let black = t.hue("neutral", if dark { 800 } else { 200 });
    let white = t.hue("neutral", if dark { 200 } else { 800 });
    let bright_white = t.hue("neutral", if dark { 100 } else { 900 });
    let blue = terminal_hue(t, rgb_to_oklch(0.0, 0.0, 1.0).h);
    let magenta = terminal_hue(t, rgb_to_oklch(1.0, 0.0, 1.0).h);
    let normal = [t.text.feedback.error.base, t.text.feedback.success.base, t.text.feedback.warning.base, blue, magenta, t.text.feedback.info.base, white];
    let mut out = [Rgba::default(); 16];
    out[0] = black;
    out[1..8].copy_from_slice(&normal);
    out[8] = t.text.muted;
    for i in 0..6 {
        out[9 + i] = t.dec(normal[i]);
    }
    out[15] = bright_white;
    out
}

fn map_color(c: vt100::Color, palette: &[Rgba; 16], default: Rgba) -> Color {
    match c {
        vt100::Color::Default => default.tui(),
        vt100::Color::Idx(i) if i < 16 => palette[i as usize].tui(),
        vt100::Color::Idx(i) => crate::theme::color::ansi(i as i64).tui(),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

/// Render a vt100 screen into `area`.
pub fn render_screen(buf: &mut Buffer, area: Rect, screen: &vt100::Screen, t: &Theme, dark: bool, show_cursor: bool) -> Option<(u16, u16)> {
    let palette = terminal_palette(t, dark);
    let bg_default = t.background.raised.base;
    let (rows, cols) = screen.size();
    for r in 0..rows.min(area.height) {
        for c in 0..cols.min(area.width) {
            let Some(cell) = screen.cell(r, c) else { continue };
            if cell.is_wide_continuation() {
                continue;
            }
            let mut fgc = map_color(cell.fgcolor(), &palette, t.text.base);
            let mut bgc = map_color(cell.bgcolor(), &palette, bg_default);
            if cell.inverse() {
                std::mem::swap(&mut fgc, &mut bgc);
            }
            let mut st = Style::default().fg(fgc).bg(bgc);
            if cell.bold() {
                st = st.add_modifier(Modifier::BOLD);
            }
            if cell.italic() {
                st = st.add_modifier(Modifier::ITALIC);
            }
            if cell.underline() {
                st = st.add_modifier(Modifier::UNDERLINED);
            }
            let text = cell.contents();
            let text = if text.is_empty() { " ".to_string() } else { text };
            put(buf, area.x + c, area.y + r, &text, st, (area.width - c) as usize);
        }
    }
    if show_cursor && !screen.hide_cursor() {
        let (r, c) = screen.cursor_position();
        if r < area.height && c < area.width {
            return Some((area.x + c, area.y + r));
        }
    }
    None
}

impl App {
    /// Terminals of a session and the selected one.
    pub fn session_terminals(&self, sid: &str) -> (Vec<Value>, Option<String>) {
        let list = self.terminal_lists.get(sid).cloned().unwrap_or_default();
        let selected = self
            .keyed
            .get("session-terminal-selection")
            .and_then(|v| v.pointer(&format!("/sessions/{sid}")).and_then(|s| s.as_str()).map(|s| s.to_string()))
            .filter(|id| list.iter().any(|t| t.get("id").and_then(|x| x.as_str()) == Some(id.as_str())));
        (list, selected)
    }

    pub fn refresh_terminals(&mut self, sid: &str) {
        let c = self.client.clone();
        let s = sid.to_string();
        self.spawn(async move { c.terminals(&s).await.map(|l| (s, l)) }, |app, r| {
            if let Ok((sid, list)) = r {
                app.terminal_lists.insert(sid, list);
                app.dirty = true;
            }
        });
    }

    pub fn select_terminal(&mut self, sid: &str, pty: Option<&str>) {
        let sid = sid.to_string();
        let pty = pty.map(|p| p.to_string());
        let p2 = pty.clone();
        self.keyed.update("session-terminal-selection", move |v| {
            if !v.is_object() {
                *v = serde_json::json!({});
            }
            if !v.get("sessions").is_some_and(|s| s.is_object()) {
                v["sessions"] = serde_json::json!({});
            }
            v["sessions"][sid] = match p2 {
                Some(p) => Value::String(p),
                None => Value::Null,
            };
        });
        if let Some(p) = pty {
            self.attach_terminal(&p);
            self.right_focus = true;
        } else {
            self.right_focus = false;
        }
        self.dirty = true;
    }

    pub fn new_terminal(&mut self, sid: &str) {
        let c = self.client.clone();
        let s = sid.to_string();
        let cwd = self.store.session(sid).map(|x| x.location.directory.clone());
        self.spawn(
            async move {
                let t = c.terminal_create(&s, cwd.as_deref()).await?;
                let list = c.terminals(&s).await?;
                Ok((s, t, list))
            },
            |app, r| match r {
                Ok((sid, t, list)) => {
                    app.terminal_lists.insert(sid.clone(), list);
                    if let Some(id) = t.get("id").and_then(|x| x.as_str()) {
                        let id = id.to_string();
                        app.select_terminal(&sid, Some(&id));
                    }
                }
                Err(_) => app.toast(Toast::new("error", "Unable to load terminal")),
            },
        );
    }

    /// Start (once) the WebSocket attachment for a PTY.
    pub fn attach_terminal(&mut self, pty: &str) {
        if self.terminal_states.contains_key(pty) {
            return;
        }
        self.terminal_states.insert(pty.to_string(), TermState::new(pty));
        let Ok(handle) = tokio::runtime::Handle::try_current() else { return };
        let c = self.client.clone();
        let tx = self.tx.clone();
        let id = pty.to_string();
        let attachment = crate::util::id_body(false);
        let (in_tx, mut in_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        if let Some(st) = self.terminal_states.get_mut(pty) {
            st.tx = Some(in_tx);
        }
        let apply = move |id: String, f: Box<dyn FnOnce(&mut TermState) + Send>| {
            let _ = tx.send(super::AppMsg::Apply(Box::new(move |app: &mut App| {
                if let Some(st) = app.terminal_states.get_mut(&id) {
                    f(st);
                }
                app.dirty = true;
            })));
        };
        handle.spawn(async move {
            let fail = |m: &str| {
                let m = m.to_string();
                apply(id.clone(), Box::new(move |st| st.failure = Some(m)));
            };
            let snapshot = match c.terminal_snapshot(&id).await {
                Ok(s) => s,
                Err(_) => return fail("Terminal connection failed"),
            };
            let cols = snapshot.pointer("/info/size/cols").and_then(|v| v.as_u64()).unwrap_or(80) as u16;
            let rows = snapshot.pointer("/info/size/rows").and_then(|v| v.as_u64()).unwrap_or(24) as u16;
            let tail = snapshot.pointer("/info/output/tail").and_then(|v| v.as_i64()).unwrap_or(0);
            use base64::Engine;
            let checkpoint = snapshot.get("checkpoint").and_then(|v| v.as_str()).and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok()).unwrap_or_default();
            apply(
                id.clone(),
                Box::new(move |st| {
                    st.set_size(rows, cols);
                    st.process(&checkpoint);
                }),
            );
            let ticket = match c.terminal_ticket(&id).await {
                Ok(t) => t,
                Err(_) => return fail("Terminal connection failed"),
            };
            let url = c.terminal_url(&id, &ticket, tail, &attachment);
            let Ok((ws, _)) = tokio_tungstenite::connect_async(url).await else { return fail("Terminal connection failed") };
            let (mut sink, mut stream) = ws.split();
            loop {
                tokio::select! {
                    out = in_rx.recv() => match out {
                        Some(frame) => {
                            if sink.send(WsMessage::Binary(frame.into())).await.is_err() {
                                return fail("Terminal connection failed");
                            }
                        }
                        None => return,
                    },
                    msg = stream.next() => match msg {
                        Some(Ok(WsMessage::Binary(data))) => {
                            let data = data.to_vec();
                            apply(id.clone(), Box::new(move |st| st.process(&data)));
                        }
                        Some(Ok(WsMessage::Text(text))) => {
                            let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                            let att = attachment.clone();
                            apply(id.clone(), Box::new(move |st| handle_message(st, &v, &att)));
                        }
                        Some(Ok(WsMessage::Close(_))) | None => {
                            apply(id.clone(), Box::new(|st| {
                                if !st.exited {
                                    st.failure = Some("Terminal disconnected".into());
                                }
                                st.attached = false;
                            }));
                            return;
                        }
                        Some(Err(_)) => return fail("Terminal connection failed"),
                        _ => {}
                    }
                }
            }
        });
    }

    /// Draw the terminal pane into `area` (box bg raised.base, terminal at left=1).
    pub fn draw_terminal(&mut self, buf: &mut Buffer, area: Rect, t: &Theme, pty: &str) -> Option<(u16, u16)> {
        fill(buf, area, t.background.raised.base.tui());
        let dark = self.themes.effective_mode() == crate::theme::Mode::Dark;
        let focused = self.right_focus;
        let st = self.terminal_states.get_mut(pty)?;
        if let Some(f) = &st.failure {
            put(buf, area.x, area.y, f, fg(t.text.feedback.error.base).bg(t.background.raised.base.tui()), area.width as usize);
            return None;
        }
        let cols = area.width.saturating_sub(2).max(1);
        let rows = area.height.max(1);
        if st.size != (cols, rows) {
            st.size = (cols, rows);
            if st.controller && st.restored {
                st.interact();
            }
        }
        let inner = Rect::new(area.x + 1, area.y, area.width.saturating_sub(1), area.height);
        render_screen(buf, inner, st.parser.screen(), t, dark, focused)
    }

    /// Forward a key to the focused terminal; false when not applicable.
    pub fn terminal_key(&mut self, s: &Stroke) -> bool {
        if !self.right_focus || self.terminal_visible().is_none() {
            return false;
        }
        if self.keybinds.is_leader(s) || !self.pending_keys.is_empty() {
            return false;
        }
        let Some(pty) = self.terminal_visible() else { return false };
        let Some(bytes) = encode_key(s) else { return true };
        if let Some(st) = self.terminal_states.get_mut(&pty) {
            st.input(&bytes);
        }
        true
    }

    /// The visible terminal of the current session, if any.
    pub fn terminal_visible(&self) -> Option<String> {
        if !self.terminal_open {
            return None;
        }
        let sid = self.session_id()?;
        self.session_terminals(sid).1
    }

    pub fn pane_width(&self, available: u16) -> u16 {
        let half = (available / 2).max(1);
        let w = self.pane_pref.unwrap_or(half);
        // `clampSessionPaneWidth`: max(min(24, half), min(w, max(half, total - 44))).
        24.min(half).max(w.min(half.max(available.saturating_sub(44))))
    }
}

fn handle_message(st: &mut TermState, v: &Value, attachment: &str) {
    match v.get("type").and_then(|t| t.as_str()) {
        Some("attached") => {
            if v.get("inputProtocol").and_then(|p| p.as_i64()) != Some(1) {
                st.failure = Some("Persistent terminal server is out of date; restart OpenCode".into());
                return;
            }
            if let (Some(c), Some(r)) = (v.pointer("/info/size/cols").and_then(|x| x.as_u64()), v.pointer("/info/size/rows").and_then(|x| x.as_u64())) {
                st.set_size(r as u16, c as u16);
            }
            st.controller = v.get("role").and_then(|r| r.as_str()) == Some("controller");
            st.attached = true;
        }
        Some("replay_complete") => {
            st.restored = true;
            let pending = std::mem::take(&mut st.pending);
            if pending.is_empty() {
                st.interact();
            }
            for p in pending {
                st.input(&p);
            }
        }
        Some("resized") => {
            if let (Some(c), Some(r)) = (v.get("cols").and_then(|x| x.as_u64()), v.get("rows").and_then(|x| x.as_u64())) {
                st.set_size(r as u16, c as u16);
            }
            use base64::Engine;
            if let Some(cp) = v.get("checkpoint").and_then(|c| c.as_str()).and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok()) {
                st.process(b"\x1bc");
                st.process(&cp);
            }
        }
        Some("exited") => st.exited = true,
        Some("controller_changed") => {
            let previous = st.controller;
            st.controller = v.get("attachmentID").and_then(|a| a.as_str()) == Some(attachment);
            if st.controller && !previous && st.restored {
                st.interact();
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_and_keys() {
        assert_eq!(interaction_frame(80, 24, None), vec![0, 0, 80, 0, 24]);
        assert_eq!(interaction_frame(300, 2, Some(b"a")), vec![1, 1, 44, 0, 2, b'a']);
        assert_eq!(encode_key(&Stroke::ctrl("c")), Some(vec![3]));
        assert_eq!(encode_key(&Stroke::key("return")), Some(b"\r".to_vec()));
        assert_eq!(encode_key(&Stroke::key("up")), Some(b"\x1b[A".to_vec()));
        let mut s = Stroke::key("x");
        s.meta = true;
        assert_eq!(encode_key(&s), Some(b"\x1bx".to_vec()));
    }

    /// vt100 0.15 panics on these ordinary sequences after a resize (wrapped wide characters then
    /// erase-below; a cursor saved before the screen shrank, then restored and written at). The
    /// parser runs on the UI thread: the screen is reset instead of the TUI going down.
    #[test]
    fn emulator_panic_resets_the_screen() {
        let mut st = TermState::new("pty_1");
        st.process("\t\t\tx全角全角全角全角".as_bytes());
        st.set_size(40, 40);
        st.process(b"\x1b[J");
        st.process(b"after");
        assert!(st.parser.screen().contents().contains("after"));
        assert_eq!(st.parser.screen().size(), (40, 40));

        let mut st = TermState::new("pty_2");
        st.set_size(40, 40);
        st.process(b"\x1b[999;999H\x1b7");
        st.set_size(24, 40);
        st.process(b"\x1b8hello");
        st.process(b"\x1b[Hok");
        assert_eq!(st.parser.screen().size(), (24, 40));
        assert!(st.parser.screen().contents().starts_with("ok"));

        // Zero sizes from the server are clamped.
        st.set_size(0, 0);
        st.process(b"x");
        assert_eq!(st.parser.screen().size(), (1, 1));
    }

    #[test]
    fn renders_screen() {
        let doc: serde_json::Value = serde_json::from_str(crate::theme::OPENCODE_V2).unwrap();
        let t = crate::theme::resolve::resolve_document(&doc, crate::theme::Mode::Dark).unwrap();
        let mut p = vt100::Parser::new(3, 10, 0);
        p.process(b"hi \x1b[31mred\x1b[0m");
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 3));
        render_screen(&mut buf, Rect::new(0, 0, 10, 3), p.screen(), &t, true, false);
        assert_eq!(buf[(0, 0)].symbol(), "h");
        assert_eq!(buf[(3, 0)].fg, terminal_palette(&t, true)[1].tui());
        assert_eq!(terminal_palette(&t, true)[1].hex(), "#e06c75");
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn attaches_over_websocket() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (got_tx, mut got_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut peek = [0u8; 512];
                let n = sock.peek(&mut peek).await.unwrap();
                let head = String::from_utf8_lossy(&peek[..n]).to_string();
                if head.contains("/connect?") {
                    let got_tx = got_tx.clone();
                    tokio::spawn(async move {
                        let ws = tokio_tungstenite::accept_async(sock).await.unwrap();
                        let (mut sink, mut stream) = ws.split();
                        let attached = r#"{"type":"attached","attachmentID":"a","inputProtocol":1,"info":{"size":{"cols":20,"rows":5}},"role":"controller","generation":1,"replay":{}}"#;
                        sink.send(WsMessage::Text(attached.into())).await.unwrap();
                        sink.send(WsMessage::Binary(b"hello world".to_vec().into())).await.unwrap();
                        sink.send(WsMessage::Text(r#"{"type":"replay_complete","endOffset":11}"#.into())).await.unwrap();
                        while let Some(Ok(WsMessage::Binary(b))) = stream.next().await {
                            let _ = got_tx.send(b.to_vec());
                        }
                    });
                    continue;
                }
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let body = if head.contains("/snapshot") {
                    r#"{"data":{"info":{"id":"pty_1","size":{"cols":20,"rows":5},"output":{"head":0,"tail":0}},"text":"","checkpoint":"","cursor":{"x":0,"y":0}}}"#
                } else {
                    r#"{"data":{"ticket":"t","expires_in":60}}"#
                };
                let resp = format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}", body.len());
                sock.write_all(resp.as_bytes()).await.unwrap();
            }
        });
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let opts = super::super::TuiOptions { url: format!("http://{addr}"), directory: "/tmp".into(), ..Default::default() };
        let mut app = App::new(opts, crate::config::Config::from_value(serde_json::json!({})), tx);
        app.attach_terminal("pty_1");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if let Ok(Some(super::super::AppMsg::Apply(f))) = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await {
                f(&mut app);
            }
            if app.terminal_states["pty_1"].restored {
                break;
            }
        }
        let st = app.terminal_states.get_mut("pty_1").unwrap();
        assert!(st.attached && st.restored && st.controller);
        assert!(st.parser.screen().contents().contains("hello world"));
        st.size = (20, 5);
        st.input(b"ls\r");
        let first = tokio::time::timeout(std::time::Duration::from_secs(3), got_rx.recv()).await.unwrap().unwrap();
        assert_eq!(first, interaction_frame(80, 24, None));
        let second = tokio::time::timeout(std::time::Duration::from_secs(3), got_rx.recv()).await.unwrap().unwrap();
        assert_eq!(second, interaction_frame(20, 5, Some(b"ls\r")));
    }
}
