//! Escape sequences split across terminal reads.
//!
//! crossterm turns an ESC byte that ends a read into an Esc key press at once, and the rest of the
//! sequence, arriving a moment later, into typed characters. Mouse motion reports, focus events and
//! kitty key reports are escape sequences, and over SSH or under load a read can end right after the
//! ESC, so moving the mouse over a busy session could "press" Esc (two of those within five seconds
//! interrupt the session) and type `[<35;12;8M` into the prompt. Upstream's input parser (OpenTUI
//! `StdinParser`) holds a lone ESC for 20 ms waiting for the rest of its sequence; [`JoinEscapes`] does
//! the same on crossterm's events: an Esc followed within that window by `[` or `O` starts a sequence,
//! which is collected up to its final byte and then decoded (keys) or dropped (mouse, focus and other
//! reports, which only matter as they arrive whole). Anything else releases the held events unchanged.
//!
//! An OSC reply (`ESC ]` … BEL or ST) is collected the same way and handed to [`JoinEscapes::on_osc`] instead of
//! being typed: a palette reply that arrives after the startup probe gave up re-themes the system theme, as OpenTUI
//! 0.9.4 emits `palette` again for a reply that arrives after its query ended.

use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::Stream;

/// How long a lone ESC waits for the rest of its sequence (OpenTUI `StdinParser` `DEFAULT_TIMEOUT_MS`).
pub const ESCAPE_TIMEOUT: Duration = Duration::from_millis(20);

enum State {
    Ground,
    /// An Esc press is held; the next event decides.
    Escape,
    /// `ESC [` (CSI) or `ESC O` (SS3) seen; the characters so far (after the introducer).
    Sequence { ss3: bool, body: String },
    /// `ESC ]` (OSC) seen; its text so far, and whether an `ESC` (the start of ST) just came.
    Osc { body: String, esc: bool },
}

pub struct JoinEscapes<S> {
    inner: S,
    state: State,
    /// Events received while in a sequence, released unchanged if it does not complete.
    held: Vec<Event>,
    out: VecDeque<std::io::Result<Event>>,
    timer: Option<Pin<Box<tokio::time::Sleep>>>,
    done: bool,
    /// Receives each complete OSC sequence's text (without `ESC ]` and its terminator).
    osc: Option<Box<dyn FnMut(String) + Send>>,
}

impl<S> JoinEscapes<S> {
    pub fn new(inner: S) -> Self {
        JoinEscapes { inner, state: State::Ground, held: vec![], out: VecDeque::new(), timer: None, done: false, osc: None }
    }

    /// Hands complete OSC sequences (terminal replies such as `4;1;rgb:…`) to `f` instead of releasing them as keys.
    pub fn on_osc(mut self, f: impl FnMut(String) + Send + 'static) -> Self {
        self.osc = Some(Box::new(f));
        self
    }

    fn osc_done(&mut self, body: String) {
        self.held.clear();
        self.state = State::Ground;
        self.timer = None;
        if let Some(f) = self.osc.as_mut() {
            f(body);
        }
    }

    fn arm(&mut self) {
        self.timer = Some(Box::pin(tokio::time::sleep(ESCAPE_TIMEOUT)));
    }

    /// Give up on the held events: release them as they came.
    fn release(&mut self) {
        self.state = State::Ground;
        self.timer = None;
        self.out.extend(self.held.drain(..).map(Ok));
    }

    fn feed(&mut self, ev: Event) {
        match &mut self.state {
            State::Ground => {
                if is_plain(&ev, KeyCode::Esc) {
                    self.held.push(ev);
                    self.state = State::Escape;
                    self.arm();
                } else if self.osc.is_some() && is_alt(&ev, ']') {
                    // crossterm reads `ESC ]` that arrive together as Alt+].
                    self.held.push(ev);
                    self.state = State::Osc { body: String::new(), esc: false };
                    self.arm();
                } else {
                    self.out.push_back(Ok(ev));
                }
            }
            State::Escape => match char_of(&ev) {
                Some(']') if self.osc.is_some() => {
                    self.held.push(ev);
                    self.state = State::Osc { body: String::new(), esc: false };
                    self.arm();
                }
                Some(c @ ('[' | 'O')) => {
                    self.held.push(ev);
                    self.state = State::Sequence { ss3: c == 'O', body: String::new() };
                    self.arm();
                }
                _ => {
                    self.release();
                    self.feed(ev);
                }
            },
            State::Sequence { ss3, body } => {
                let Some(c) = char_of(&ev).filter(|c| (' '..='~').contains(c)) else {
                    self.release();
                    self.feed(ev);
                    return;
                };
                body.push(c);
                // CSI: parameter and intermediate bytes 0x20-0x3F, then a final byte 0x40-0x7E (`[` excepted
                // right after the introducer: `ESC [ [` is a Linux console function key). SS3: one final byte.
                let finished = *ss3 || (('@'..='~').contains(&c) && !(c == '[' && body.len() == 1));
                if !finished {
                    self.held.push(ev);
                    self.arm();
                    return;
                }
                let decoded = decode(*ss3, body);
                self.held.clear();
                self.state = State::Ground;
                self.timer = None;
                if let Some(ev) = decoded {
                    self.out.push_back(Ok(ev));
                }
            }
            State::Osc { body, esc } => {
                // BEL (read as Ctrl+G) or ST (`ESC \`, read as Alt+\ or Esc then `\`) ends it.
                if is_ctrl(&ev, 'g') || is_alt(&ev, '\\') || (*esc && char_of(&ev) == Some('\\')) {
                    let body = std::mem::take(body);
                    self.osc_done(body);
                    return;
                }
                if !*esc && is_plain(&ev, KeyCode::Esc) {
                    *esc = true;
                    self.held.push(ev);
                    self.arm();
                    return;
                }
                match char_of(&ev).filter(|c| (' '..='~').contains(c)) {
                    Some(c) if !*esc => {
                        body.push(c);
                        self.held.push(ev);
                        self.arm();
                    }
                    _ => {
                        self.release();
                        self.feed(ev);
                    }
                }
            }
        }
    }
}

fn is_alt(ev: &Event, c: char) -> bool {
    matches!(ev, Event::Key(KeyEvent { code: KeyCode::Char(k), modifiers, kind: KeyEventKind::Press, .. }) if *k == c && *modifiers == KeyModifiers::ALT)
}

fn is_ctrl(ev: &Event, c: char) -> bool {
    matches!(ev, Event::Key(KeyEvent { code: KeyCode::Char(k), modifiers, kind: KeyEventKind::Press, .. }) if *k == c && *modifiers == KeyModifiers::CONTROL)
}

fn char_of(ev: &Event) -> Option<char> {
    match ev {
        Event::Key(KeyEvent { code: KeyCode::Char(c), modifiers, kind: KeyEventKind::Press, .. })
            if (*modifiers - KeyModifiers::SHIFT).is_empty() =>
        {
            Some(*c)
        }
        _ => None,
    }
}

fn is_plain(ev: &Event, code: KeyCode) -> bool {
    matches!(ev, Event::Key(KeyEvent { code: c, modifiers, kind: KeyEventKind::Press, .. }) if *c == code && modifiers.is_empty())
}

/// The key a reassembled sequence stands for (unmodified cursor keys and the kitty Esc report); `None`
/// drops it (mouse reports, focus changes, anything else).
fn decode(ss3: bool, body: &str) -> Option<Event> {
    let key = |code| Some(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
    let last = body.chars().last()?;
    let params = &body[..body.len() - last.len_utf8()];
    if !ss3 && last == 'u' && (params == "27" || params == "27;1") {
        return key(KeyCode::Esc);
    }
    if !(params.is_empty() || params == "1" || params == "1;1") {
        return None;
    }
    match last {
        'A' => key(KeyCode::Up),
        'B' => key(KeyCode::Down),
        'C' => key(KeyCode::Right),
        'D' => key(KeyCode::Left),
        'H' => key(KeyCode::Home),
        'F' => key(KeyCode::End),
        _ => None,
    }
}

impl<S> Stream for JoinEscapes<S>
where
    S: Stream<Item = std::io::Result<Event>> + Unpin,
{
    type Item = std::io::Result<Event>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            if let Some(ev) = this.out.pop_front() {
                return Poll::Ready(Some(ev));
            }
            if this.done {
                return Poll::Ready(None);
            }
            match Pin::new(&mut this.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(ev))) => {
                    this.feed(ev);
                    continue;
                }
                Poll::Ready(Some(Err(e))) => {
                    this.release();
                    this.out.push_back(Err(e));
                    continue;
                }
                Poll::Ready(None) => {
                    this.release();
                    this.done = true;
                    continue;
                }
                Poll::Pending => {}
            }
            if let Some(timer) = &mut this.timer
                && timer.as_mut().poll(cx).is_ready()
            {
                this.release();
                continue;
            }
            return Poll::Pending;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    fn key(c: KeyCode) -> std::io::Result<Event> {
        Ok(Event::Key(KeyEvent::new(c, KeyModifiers::NONE)))
    }

    fn chars(s: &str) -> Vec<std::io::Result<Event>> {
        s.chars()
            .map(|c| {
                let m = if c.is_ascii_uppercase() { KeyModifiers::SHIFT } else { KeyModifiers::NONE };
                Ok(Event::Key(KeyEvent::new(KeyCode::Char(c), m)))
            })
            .collect()
    }

    /// Events as crossterm delivers them, with a pause before each group.
    async fn run(groups: Vec<(u64, Vec<std::io::Result<Event>>)>) -> Vec<String> {
        let (tx, rx) = futures::channel::mpsc::unbounded();
        tokio::spawn(async move {
            for (pause, events) in groups {
                tokio::time::sleep(Duration::from_millis(pause)).await;
                for e in events {
                    let _ = tx.unbounded_send(e);
                }
            }
        });
        JoinEscapes::new(rx)
            .map(|e| match e.unwrap() {
                Event::Key(k) => format!("{:?}", k.code),
                other => format!("{other:?}"),
            })
            .collect()
            .await
    }

    // A paused clock: the pauses are exact even where the system timer is coarse (about 16 ms on Windows).
    #[tokio::test(start_paused = true)]
    async fn split_mouse_report_is_not_an_escape_press() {
        let out = run(vec![(0, vec![key(KeyCode::Esc)]), (5, chars("[<35;10;10M")), (5, chars("hi"))]).await;
        assert_eq!(out, vec!["Char('h')", "Char('i')"]);
        // Focus reports and a split cursor key.
        let out = run(vec![(0, vec![key(KeyCode::Esc)]), (3, chars("[I")), (0, vec![key(KeyCode::Esc)]), (3, chars("OA"))]).await;
        assert_eq!(out, vec!["Up"]);
        // The kitty keyboard protocol reports Esc itself as `CSI 27 u`.
        let out = run(vec![(0, vec![key(KeyCode::Esc)]), (2, chars("[27u"))]).await;
        assert_eq!(out, vec!["Esc"]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_real_escape_press_comes_through() {
        // Alone: released once the window passes.
        let out = run(vec![(0, vec![key(KeyCode::Esc)]), (60, chars("x"))]).await;
        assert_eq!(out, vec!["Esc", "Char('x')"]);
        // Followed by something that cannot continue a sequence.
        let out = run(vec![(0, vec![key(KeyCode::Esc), key(KeyCode::Esc)])]).await;
        assert_eq!(out, vec!["Esc", "Esc"]);
        // An unfinished sequence is released as it came.
        let out = run(vec![(0, vec![key(KeyCode::Esc)]), (2, chars("[1")), (60, chars("z"))]).await;
        assert_eq!(out, vec!["Esc", "Char('[')", "Char('1')", "Char('z')"]);
    }
    fn alt(c: char) -> std::io::Result<Event> {
        Ok(Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT)))
    }

    fn ctrl(c: char) -> std::io::Result<Event> {
        Ok(Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)))
    }

    /// Late palette replies (`ESC ] 4;1;rgb:… BEL`, or ST) go to `on_osc`, not into the prompt as typed keys.
    #[tokio::test(start_paused = true)]
    async fn late_osc_replies_go_to_the_handler() {
        let (tx, rx) = futures::channel::mpsc::unbounded();
        let mut bel = vec![alt(']')];
        bel.extend(chars("4;1;rgb:cccc/0000/0000"));
        bel.push(ctrl('g'));
        let mut st = vec![key(KeyCode::Esc)];
        st.extend(chars("]11;rgb:1010/1010/1010"));
        st.push(alt('\\'));
        tokio::spawn(async move {
            for events in [bel, st, chars("ok")] {
                tokio::time::sleep(Duration::from_millis(2)).await;
                for e in events {
                    let _ = tx.unbounded_send(e);
                }
            }
        });
        let got = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = got.clone();
        let keys: Vec<String> = JoinEscapes::new(rx)
            .on_osc(move |body| sink.lock().unwrap().push(body))
            .map(|e| match e.unwrap() {
                Event::Key(k) => format!("{:?}", k.code),
                other => format!("{other:?}"),
            })
            .collect()
            .await;
        assert_eq!(keys, vec!["Char('o')", "Char('k')"]);
        assert_eq!(*got.lock().unwrap(), vec!["4;1;rgb:cccc/0000/0000".to_string(), "11;rgb:1010/1010/1010".to_string()]);
    }
}
