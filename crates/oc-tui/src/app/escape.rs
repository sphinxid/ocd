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
}

pub struct JoinEscapes<S> {
    inner: S,
    state: State,
    /// Events received while in a sequence, released unchanged if it does not complete.
    held: Vec<Event>,
    out: VecDeque<std::io::Result<Event>>,
    timer: Option<Pin<Box<tokio::time::Sleep>>>,
    done: bool,
}

impl<S> JoinEscapes<S> {
    pub fn new(inner: S) -> Self {
        JoinEscapes { inner, state: State::Ground, held: vec![], out: VecDeque::new(), timer: None, done: false }
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
                } else {
                    self.out.push_back(Ok(ev));
                }
            }
            State::Escape => match char_of(&ev) {
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
        }
    }
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

    #[tokio::test]
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

    #[tokio::test]
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
}
