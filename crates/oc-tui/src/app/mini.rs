//! Mini mode rendering (`src/mini/`, spec §7.9): the transcript is committed to native
//! terminal scrollback and only a live footer (in-progress tail, prompt / permission / form,
//! dialogs, toasts) is redrawn at the bottom of the screen.

use super::App;
use super::view::replace_spinners;
use crate::ui::dialog;
use crate::ui::text::*;
use crate::ui::view_helpers::{GO_LEFT, GO_RIGHT, logo_line};
use crate::util::locale::{self, width as sw};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};

/// Scrollback bookkeeping for one mini run.
#[derive(Debug, Default)]
pub struct MiniState {
    pub session: Option<String>,
    /// Transcript lines already written to scrollback for `session`.
    pub committed: usize,
    pub width: u16,
    /// Replay existing history when a session is resumed (`--replay`, `mini.replay`).
    pub replay: bool,
    /// Replay only the newest N messages (`--replay-limit`, `mini.replay_limit`, default 200).
    pub replay_limit: usize,
    /// The next session seen is an existing one (resume, clear or re-wrap), so history rules apply.
    pub resumed: bool,
}

/// Maximum in-progress transcript rows shown live in the footer.
const LIVE_TAIL: usize = 8;

impl App {
    fn mini_width(&self, width: u16) -> usize {
        width.saturating_sub(2).max(1) as usize
    }

    /// New scrollback lines to commit (already rendered to plain styled rows) plus the
    /// uncommitted in-progress tail, for the current session.
    pub fn mini_transcript(&mut self, st: &mut MiniState, width: u16) -> (Vec<SLine>, Vec<SLine>) {
        let Some(sid) = self.session_id().map(|s| s.to_string()) else {
            st.session = None;
            st.committed = 0;
            return (vec![], vec![]);
        };
        if st.resumed && st.session.as_deref() != Some(sid.as_str()) && !self.store.message_cursor.contains_key(&sid) {
            // Wait for the resumed session's history before deciding what to replay.
            return (vec![], vec![]);
        }
        let w = self.mini_width(width);
        // Incremental: only changed rows are rendered; only new and tail lines are copied.
        self.transcript_sync(&sid, w);
        let len = self.tcache.len();
        let marks = self.tcache.marks();
        let running = self.store.is_running(&sid);
        let stable = if running {
            marks.iter().rev().find(|(_, _, user)| *user).map(|(i, _, _)| i.saturating_sub(1)).unwrap_or(0)
        } else {
            len
        };
        if st.session.as_deref() != Some(sid.as_str()) {
            st.session = Some(sid.clone());
            // Resumed session: replay the newest messages (or nothing with --no-replay).
            let resumed = std::mem::take(&mut st.resumed);
            st.committed = if !resumed {
                0
            } else if !st.replay {
                stable
            } else {
                let users: Vec<usize> = marks.iter().map(|(i, _, _)| i.saturating_sub(1)).collect();
                if users.len() > st.replay_limit { users[users.len() - st.replay_limit] } else { 0 }
            };
        }
        if stable < st.committed {
            st.committed = stable;
        }
        let new: Vec<SLine> = self.tcache.range(st.committed, stable);
        st.committed = stable;
        let tail = self.tcache.range(len.saturating_sub(LIVE_TAIL).max(stable), len);
        (new, tail)
    }

    /// Draw the live footer into a fresh buffer of `width` × (computed height ≤ `max_h`).
    /// Returns the buffer and the cursor position inside it.
    pub fn mini_footer(&mut self, width: u16, max_h: u16, tail: &[SLine]) -> (Buffer, Option<(u16, u16)>) {
        let t = self.theme().clone();
        let w = self.mini_width(width);
        let mut dock: Vec<SLine> = Vec::new();
        for l in tail {
            let mut l = l.clone();
            l.segs.insert(0, pad(1));
            dock.push(l);
        }
        if !tail.is_empty() {
            dock.push(SLine::blank());
        }
        // Busy status line (ocd addition) right above the prompt / permission / form.
        for mut line in self.busy_lines(&t, w.saturating_sub(1), self.highlight(&t)) {
            line.segs.insert(0, pad(1));
            dock.push(line);
        }
        let permission = self.active_permission();
        let form = self.active_form();
        let mut prompt = None;
        if let Some(p) = &permission {
            dock.extend(self.permission_lines(&t, w, p));
        } else if form.is_some() {
            dock.extend(self.form_lines(&t, w));
        } else {
            prompt = Some(self.prompt_lines(&t, w));
        }
        let prompt_h = prompt.as_ref().map(|p| p.0.len()).unwrap_or(0);
        let ac_h = self.autocomplete.as_ref().map(|a| a.items.len().clamp(1, 10)).unwrap_or(0);
        let mut h = (dock.len() + prompt_h + ac_h) as u16;
        if !self.dialogs.is_empty() {
            h = h.max(24);
        }
        let h = h.min(max_h).max(1);
        let area = Rect::new(0, 0, width, h);
        let mut buf = Buffer::empty(area);
        let inner = Rect::new(1, 0, w as u16, h);
        let dock_h = (h as usize).saturating_sub(prompt_h).min(dock.len());
        let skip = dock.len() - dock_h;
        let dock_y = h.saturating_sub((dock_h + prompt_h) as u16);
        let hits = draw_lines(&mut buf, Rect::new(inner.x, dock_y, inner.width, dock_h as u16), &dock[skip..], 0, "");
        self.hits = hits;
        let mut cursor = None;
        if let Some(pl) = prompt {
            let y = dock_y + dock_h as u16;
            let ph = (pl.0.len() as u16).min(h.saturating_sub(y));
            self.render_prompt(&mut buf, Rect::new(inner.x, y, inner.width, ph), &t, pl, &mut cursor);
        }
        self.draw_toast(&mut buf, area, &t);
        if let Some(mut d) = self.dialogs.pop() {
            let kb = self.keybinds.clone();
            let (rect, hits, c) = dialog::render(&mut buf, area, &mut d, &t, &kb);
            self.dialogs.push(d);
            self.dialog_hits = hits;
            self.dialog_rect = Some(rect);
            cursor = c;
        }
        replace_spinners(&mut buf, self.spinner_frame());
        (buf, cursor)
    }

    /// Entry header: `▪ oc mini v<version> · <path>` (`src/mini/splash.ts:280-298`).
    pub fn mini_header(&self, width: u16) -> SLine {
        let t = self.theme();
        let path = locale::abbreviate_home(&self.location_dir());
        let head = format!("▪ oc mini v{}", self.opts.version);
        let room = (width as usize).saturating_sub(sw(&head) + 3);
        SLine::new(vec![
            seg("▪", fg(t.text.action.primary.base)),
            Seg::new(" oc mini", bold(fg(t.text.base))),
            seg(format!(" v{}", self.opts.version), fg(t.text.muted)),
            seg(format!(" · {}", locale::truncate_left(&path, room.max(1))), fg(t.text.muted)),
        ])
    }

    /// Exit banner: the `go` logo beside `Session <title>` and `Continue opencode mini -s <id>`.
    pub fn mini_banner(&self) -> Vec<SLine> {
        let Some(s) = self.session() else { return vec![] };
        let t = self.theme();
        let title = s.title.clone().filter(|t| !t.is_empty() && !locale::is_default_title(t)).unwrap_or_else(|| "Untitled session".into());
        let title = locale::truncate(&title, 50);
        let rows: [Vec<Seg>; 3] = [
            vec![seg("Session   ", fg(t.text.muted)), Seg::new(title, bold(fg(t.text.base)))],
            vec![seg("Continue  ", fg(t.text.muted)), Seg::new(format!("ocd mini -s {}", s.id), bold(fg(t.text.base)))],
            vec![],
        ];
        rows.into_iter()
            .enumerate()
            .map(|(i, text)| {
                let mut segs = logo_line(GO_LEFT[i + 1], t, t.text.muted, false);
                segs.extend(logo_line(GO_RIGHT[i + 1], t, t.text.base, true));
                segs.push(pad(2));
                segs.extend(text);
                SLine::new(segs)
            })
            .collect()
    }
}

/// Render styled lines at `width` into ANSI rows (transparent background, trailing blanks trimmed).
pub fn lines_to_ansi(lines: &[SLine], width: u16) -> Vec<String> {
    if lines.is_empty() {
        return vec![];
    }
    let h = lines.len() as u16;
    let area = Rect::new(0, 0, width.max(1), h);
    let mut buf = Buffer::empty(area);
    draw_lines(&mut buf, Rect::new(1, 0, width.saturating_sub(1).max(1), h), lines, 0, SPINNER_FRAMES[0]);
    replace_spinners(&mut buf, SPINNER_FRAMES[0]);
    (0..h).map(|y| row_to_ansi(&buf, y)).collect()
}

fn sgr_color(c: Color, bg: bool) -> String {
    let base = if bg { 48 } else { 38 };
    match c {
        Color::Reset => format!("{}", base + 1),
        Color::Rgb(r, g, b) => format!("{base};2;{r};{g};{b}"),
        Color::Indexed(i) => format!("{base};5;{i}"),
        Color::Black => format!("{}", if bg { 40 } else { 30 }),
        Color::Red => format!("{}", if bg { 41 } else { 31 }),
        Color::Green => format!("{}", if bg { 42 } else { 32 }),
        Color::Yellow => format!("{}", if bg { 43 } else { 33 }),
        Color::Blue => format!("{}", if bg { 44 } else { 34 }),
        Color::Magenta => format!("{}", if bg { 45 } else { 35 }),
        Color::Cyan => format!("{}", if bg { 46 } else { 36 }),
        Color::Gray => format!("{}", if bg { 47 } else { 37 }),
        Color::DarkGray => format!("{}", if bg { 100 } else { 90 }),
        Color::LightRed => format!("{}", if bg { 101 } else { 91 }),
        Color::LightGreen => format!("{}", if bg { 102 } else { 92 }),
        Color::LightYellow => format!("{}", if bg { 103 } else { 93 }),
        Color::LightBlue => format!("{}", if bg { 104 } else { 94 }),
        Color::LightMagenta => format!("{}", if bg { 105 } else { 95 }),
        Color::LightCyan => format!("{}", if bg { 106 } else { 96 }),
        Color::White => format!("{}", if bg { 107 } else { 97 }),
    }
}

fn style_sgr(fgc: Color, bgc: Color, m: Modifier) -> String {
    let mut parts = vec!["0".to_string(), sgr_color(fgc, false), sgr_color(bgc, true)];
    for (flag, code) in [
        (Modifier::BOLD, "1"),
        (Modifier::DIM, "2"),
        (Modifier::ITALIC, "3"),
        (Modifier::UNDERLINED, "4"),
        (Modifier::REVERSED, "7"),
        (Modifier::CROSSED_OUT, "9"),
    ] {
        if m.contains(flag) {
            parts.push(code.into());
        }
    }
    format!("\x1b[{}m", parts.join(";"))
}

/// One buffer row as an ANSI string.
pub fn row_to_ansi(buf: &Buffer, y: u16) -> String {
    let area = buf.area;
    // Trim trailing default cells.
    let mut end = area.right();
    while end > area.x {
        let c = &buf[(end - 1, y)];
        if (c.symbol() == " " || c.symbol().is_empty()) && c.bg == Color::Reset && !c.modifier.contains(Modifier::REVERSED) {
            end -= 1;
        } else {
            break;
        }
    }
    let mut out = String::new();
    let mut last: Option<(Color, Color, Modifier)> = None;
    for x in area.x..end {
        let c = &buf[(x, y)];
        let key = (c.fg, c.bg, c.modifier);
        if last != Some(key) {
            out.push_str(&style_sgr(c.fg, c.bg, c.modifier));
            last = Some(key);
        }
        out.push_str(c.symbol());
    }
    if last.is_some() {
        out.push_str("\x1b[0m");
    }
    out
}

/// Plain text of styled lines (for tests).
pub fn plain(lines: &[SLine]) -> Vec<String> {
    lines.iter().map(|l| l.segs.iter().map(|s| s.text.as_str()).collect::<String>()).collect()
}


fn pad(n: usize) -> Seg {
    Seg::new(" ".repeat(n), ratatui::style::Style::default())
}
