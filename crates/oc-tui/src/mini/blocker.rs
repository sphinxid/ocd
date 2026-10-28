//! Blocker docks in the footer: the permission body (`src/mini/footer.permission.tsx`) and the form
//! body (`src/mini/footer.form.tsx`) over the shared state machines.

use super::cell::{Buffer, Color, StyledChunk, attr};
use super::form_shared::*;
use super::permission_shared::*;
use super::structured;
use super::text::{WrapMode, layout};
use super::textarea::TextArea;
use super::theme::{BlockTheme, FooterTheme};
use super::types::{MiniForm, MiniPermission};
use crate::keymap::Stroke;
use crate::ui::form as uf;
use crate::util::locale::{self, width as sw};
use serde_json::Value;

fn text_rows(text: &str, fg: Color, width: usize, mode: WrapMode) -> Vec<Vec<StyledChunk>> {
    layout(&[StyledChunk::plain(text, fg)], width.max(1), mode, false).into_iter().map(|l| l.chunks).collect()
}

fn chunk_rows(chunks: &[StyledChunk], width: usize, mode: WrapMode) -> Vec<Vec<StyledChunk>> {
    layout(chunks, width.max(1), mode, false).into_iter().map(|l| l.chunks).collect()
}

fn draw_rows(buf: &mut Buffer, x: usize, y: usize, rows: &[Vec<StyledChunk>], width: usize, max_y: usize) -> usize {
    for (n, r) in rows.iter().enumerate() {
        if y + n >= max_y {
            break;
        }
        buf.draw_chunks(x as i32, (y + n) as i32, r, width);
    }
    rows.len()
}

/// A content block in a scroll box: rows of chunks laid at a column offset.
struct Block {
    x: usize,
    rows: Vec<Vec<StyledChunk>>,
    buf: Option<Buffer>,
}

impl Block {
    fn height(&self) -> usize {
        self.buf.as_ref().map(|b| b.height as usize).unwrap_or(self.rows.len())
    }
}

/// Lay out blocks with `gap` rows between them into a scroll viewport.
/// A scroll box at (x, y) of `width` x `height` (`width` includes the scrollbar column); the viewport
/// has `vpl`/`vpr` padding. `scrollbar`: (track, thumb, always visible).
#[allow(clippy::too_many_arguments)]
fn scroll_view(buf: &mut Buffer, x: usize, y: usize, width: usize, height: usize, blocks: &[Block], gap: usize, scroll: usize, scrollbar: Option<(Color, Color, bool)>, vpl: usize, vpr: usize) {
    let total: usize = blocks.iter().map(|b| b.height()).sum::<usize>() + gap * blocks.len().saturating_sub(1);
    let overflow = total > height;
    let bar = scrollbar.filter(|s| s.2 || overflow);
    let x = x + vpl;
    let width = width.saturating_sub(vpl);
    let content_w = width.saturating_sub(vpr + usize::from(bar.is_some()));
    let mut canvas = Buffer::new(content_w.max(1) as u16, total.max(1) as u16);
    let mut cy = 0;
    for (i, b) in blocks.iter().enumerate() {
        if i > 0 {
            cy += gap;
        }
        match &b.buf {
            Some(src) => super::markdown::blit(&mut canvas, src, b.x as i32, cy as i32),
            None => {
                for (r, row) in b.rows.iter().enumerate() {
                    canvas.draw_chunks(b.x as i32, (cy + r) as i32, row, content_w.saturating_sub(b.x));
                }
            }
        }
        cy += b.height();
    }
    let scroll = scroll.min(total.saturating_sub(height));
    for r in 0..height.min(total.saturating_sub(scroll)) {
        for cx in 0..content_w {
            let cell = canvas.get(cx as u16, (scroll + r) as u16).clone();
            if cell.glyph == super::cell::Glyph::Empty {
                continue;
            }
            let i = (y + r) * buf.width as usize + x + cx;
            if i < buf.cells.len() && x + cx < buf.width as usize {
                let bg = buf.cells[i].bg;
                buf.cells[i] = cell;
                if buf.cells[i].bg.a == 0 {
                    buf.cells[i].bg = bg;
                }
            }
        }
    }
    if let Some((track, thumb, _)) = bar {
        draw_scrollbar(buf, x + width - 1, y, height, total, height, scroll, track, thumb);
    }
}

/// OpenTUI's vertical `ScrollBar` slider (`sliderThumb` in half-cell units, `▀`/`▄` ends).
#[allow(clippy::too_many_arguments)]
pub fn draw_scrollbar(buf: &mut Buffer, sx: usize, y: usize, height: usize, content: usize, viewport: usize, scroll: usize, track: Color, thumb: Color) {
    let track_size = (height * 2) as f64;
    let range = content.saturating_sub(viewport) as f64;
    let (size, start) = if range == 0.0 {
        (track_size, 0.0)
    } else {
        let viewport_size = (viewport as f64).max(1.0);
        let content_size = range + viewport_size;
        let size = if content_size <= viewport_size { track_size } else { (track_size * (viewport_size / content_size)).floor().min(track_size).max(1.0) };
        let ratio = (scroll as f64).min(range) / range;
        let offset = ratio * (track_size - size);
        let lower = offset.floor();
        (size, if offset - lower < 0.5 { lower } else { lower + 1.0 })
    };
    let end = start + size;
    for r in 0..height {
        buf.fill_rect(sx as i32, (y + r) as i32, 1, 1, track);
        let cell_start = (r * 2) as f64;
        let in_cell = start.max(cell_start);
        let coverage = end.min(cell_start + 2.0) - in_cell;
        let ch = if coverage >= 2.0 {
            "█"
        } else if coverage > 0.0 {
            if in_cell == cell_start { "▀" } else { "▄" }
        } else {
            continue;
        };
        buf.draw_chunks(sx as i32, (y + r) as i32, &[StyledChunk { text: ch.into(), fg: thumb, bg: track, attrs: 0 }], 1);
    }
}

/// A choice row: ordinal, label rows, description rows, inline description, active.
type ChoiceRow = (String, Vec<Vec<StyledChunk>>, Vec<Vec<StyledChunk>>, Option<Vec<StyledChunk>>, bool);

pub struct PermissionBody {
    state: PermissionBodyState,
    scroll: usize,
    reject: TextArea,
}

impl PermissionBody {
    pub fn new(p: &MiniPermission) -> PermissionBody {
        PermissionBody { state: create_permission_body_state(&p.request.id, &p.request.session_id), scroll: 0, reject: TextArea::new() }
    }

    pub fn request_id(&self) -> &str {
        &self.state.request_id
    }

    fn options(&self, p: &MiniPermission) -> Vec<PermOption> {
        permission_options(self.state.stage).into_iter().filter(|o| *o != PermOption::Always || p.request.save.as_ref().is_some_and(|s| !s.is_empty())).collect()
    }

    fn compact(width: usize, height: usize) -> bool {
        width < 56 || height < 12
    }

    /// Keyboard handling; a reply to send when the user confirmed.
    pub fn key(&mut self, s: &Stroke, p: &MiniPermission, _directory: Option<&str>, _mono: bool, _width: usize, _height: usize) -> Option<PermissionReply> {
        let name = s.name.as_str();
        if self.state.stage == Stage::Reject {
            if name == "escape" {
                self.state = permission_cancel(&self.state);
                return None;
            }
            if name == "return" && !s.shift && !s.meta && !s.ctrl {
                if let Some(r) = permission_reject(&self.state, &p.request.id) {
                    self.state.submitting = true;
                    return Some(r);
                }
                return None;
            }
            if self.state.submitting {
                return None;
            }
            let before = self.reject.plain_text().to_string();
            if let Some(c) = s.is_char() {
                self.reject.insert_text(&c.to_string());
            } else {
                match name {
                    "backspace" => self.reject.delete_char_backward(),
                    "delete" => self.reject.delete_char(),
                    "left" => self.reject.move_left(),
                    "right" => self.reject.move_right(),
                    "home" => self.reject.line_home(),
                    "end" => self.reject.line_end(),
                    _ if s.ctrl && name == "a" => self.reject.line_home(),
                    _ if s.ctrl && name == "e" => self.reject.line_end(),
                    _ if s.ctrl && name == "w" => self.reject.delete_word_backward(),
                    _ if s.ctrl && name == "u" => self.reject.delete_to_line_start(),
                    _ if s.ctrl && name == "k" => self.reject.delete_to_line_end(),
                    _ if name == "return" || (s.ctrl && name == "j") => self.reject.newline(),
                    _ => {}
                }
            }
            if self.reject.plain_text() != before {
                self.state.message = self.reject.plain_text().to_string();
            }
            return None;
        }
        if name == "pageup" || name == "pagedown" {
            if name == "pageup" {
                self.scroll = self.scroll.saturating_sub(4);
            } else {
                self.scroll += 4;
            }
            return None;
        }
        if self.state.submitting {
            return None;
        }
        let opts = self.options(p);
        match name {
            "tab" => self.state = permission_shift(&self.state, if s.shift { -1 } else { 1 }, &opts),
            "left" | "h" => self.state = permission_shift(&self.state, -1, &opts),
            "right" | "l" => self.state = permission_shift(&self.state, 1, &opts),
            "return" => {
                let step = permission_run(&self.state, &p.request.id, self.state.selected);
                self.state = step.state;
                if step.reply.is_some() {
                    self.state.submitting = true;
                }
                if self.state.stage == Stage::Reject {
                    self.reject.set_text(&self.state.message);
                }
                return step.reply;
            }
            "escape" => {
                self.state = permission_escape(&self.state);
                if self.state.stage == Stage::Reject {
                    self.reject.set_text(&self.state.message);
                }
            }
            _ => {}
        }
        None
    }

    pub fn paste(&mut self, text: &str) {
        if self.state.stage == Stage::Reject {
            self.reject.insert_text(text);
            self.state.message = self.reject.plain_text().to_string();
        }
    }

    pub fn reply_failed(&mut self) {
        self.state.submitting = false;
    }

    /// Render the dock into rows `[y, y + height)`.
    #[allow(clippy::too_many_arguments)]
    pub fn render(&mut self, buf: &mut Buffer, y: usize, width: usize, height: usize, t: &FooterTheme, block: &BlockTheme, p: &MiniPermission, directory: Option<&str>, mono: bool) -> Option<(usize, usize)> {
        let compact = Self::compact(width, height);
        let stage = self.state.stage;
        let busy = self.state.submitting;
        buf.fill_rect(0, y as i32, width as i32, height as i32, t.surface);
        let bottom = y + height;
        // Header.
        let (pl, pr, pt, gap) = if compact { (0, 0, 0, 0) } else { (2, 3, 1, 1) };
        let mut row = y + pt;
        let title = match stage {
            Stage::Always => "Always allow",
            Stage::Reject => if width < 24 { "Reject" } else { "Reject permission" },
            Stage::Permission => if width < 24 { "Permission" } else { "Permission required" },
        };
        let hw = width.saturating_sub(pl + pr);
        let head = layout(&[StyledChunk::plain(if mono { "! " } else { "△ " }, t.permission), StyledChunk::plain(title, t.text)], hw.max(1), WrapMode::None, true);
        if let Some(l) = head.first() {
            buf.draw_chunks(pl as i32, row as i32, &l.chunks, hw);
        }
        row += 1;
        if !compact && stage == Stage::Reject {
            row += gap;
            buf.draw_chunks(pl as i32, row as i32, &[StyledChunk::plain("Tell OpenCode what to do differently", t.muted)], hw);
            row += 1;
        }
        row += if compact { 0 } else { 1 };
        let mut cursor = None;
        if stage == Stage::Reject {
            // Reject field docked at the bottom.
            let wide = width >= 80;
            let (bpl, bpr, bpt) = if compact { (0, 0, 0) } else { (2, 3, 1) };
            let inner = width.saturating_sub(bpl + bpr);
            let hint_w = if busy { 0 } else { sw(if compact { "enter reject" } else { "enter confirm" }) + if compact { 1 } else { 2 } + sw(if compact { "esc back" } else { "esc cancel" }) };
            let field_w = if wide { inner.saturating_sub(hint_w + if compact { 0 } else { 1 }) } else { inner };
            let field_rows = self.reject.rows(field_w, t.formfield_focused_text);
            let placeholder_rows = text_rows("Tell OpenCode what to do differently", t.muted, field_w, WrapMode::Word);
            let fh = if self.reject.plain_text().is_empty() { placeholder_rows.len() } else { field_rows.len() }.clamp(1, 3);
            let box_h = if wide { fh } else { fh + if compact { 0 } else { 1 } + 1 } + bpt * 2;
            let top = bottom.saturating_sub(box_h);
            buf.fill_rect(0, top as i32, width as i32, box_h as i32, t.line);
            let fy = top + bpt;
            buf.fill_rect(bpl as i32, fy as i32, field_w as i32, fh as i32, t.formfield_focused_bg);
            if self.reject.plain_text().is_empty() {
                for (i, r) in placeholder_rows.iter().take(fh).enumerate() {
                    let r: Vec<StyledChunk> = r.iter().map(|c| StyledChunk { bg: t.formfield_focused_bg, ..c.clone() }).collect();
                    buf.draw_chunks(bpl as i32, (fy + i) as i32, &r, field_w);
                }
            } else {
                for (i, r) in field_rows.iter().take(fh).enumerate() {
                    buf.draw_chunks(bpl as i32, (fy + i) as i32, &r.chunks, field_w);
                }
            }
            let (cr, cc) = self.reject.visual_cursor(field_w);
            if !busy {
                cursor = Some((bpl + cc, fy + cr.min(fh - 1)));
            }
            let hy = if wide { fy } else { fy + fh + if compact { 0 } else { 1 } };
            let hx = if wide { width.saturating_sub(bpr + hint_w.max(if busy { 1 } else { 0 })) } else { bpl };
            if busy {
                let msg = if compact { "Waiting…" } else { "Waiting for permission event…" };
                let x = if wide { width.saturating_sub(bpr + sw(msg)) } else { bpl };
                buf.draw_chunks(x as i32, hy as i32, &[StyledChunk::plain(msg, t.running)], inner);
            } else {
                let a = vec![StyledChunk::plain("enter ", t.text), StyledChunk::plain(if compact { "reject" } else { "confirm" }, t.muted)];
                let b = vec![StyledChunk::plain("esc ", t.text), StyledChunk::plain(if compact { "back" } else { "cancel" }, t.muted)];
                let aw: usize = a.iter().map(|c| sw(&c.text)).sum();
                buf.draw_chunks(hx as i32, hy as i32, &a, inner);
                buf.draw_chunks((hx + aw + if compact { 1 } else { 2 }) as i32, hy as i32, &b, inner);
            }
            return cursor;
        }
        // Controls at the bottom.
        let opts = self.options(p);
        let controls_w = opts.iter().map(|o| sw(permission_label(*o)) + 3).sum::<usize>().saturating_sub(1);
        let hint = if compact && width < 56 { "pgup/pgdn scroll".to_string() } else { format!("{} select  enter confirm  esc {}", if mono { "left/right" } else { "⇆" }, if stage == Stage::Always { "cancel" } else { "reject" }) };
        let inline = controls_w + sw(&hint) < width.saturating_sub(if compact { 0 } else { 5 });
        let (cpl, cpr, cpt) = if compact { (0, 0, 0) } else { (2, 3, 1) };
        let inner = width.saturating_sub(cpl + cpr);
        // Buttons wrap within their box.
        let mut button_rows: Vec<Vec<(PermOption, usize)>> = vec![vec![]];
        let bw = if inline { controls_w } else { inner };
        let mut x = 0;
        for o in &opts {
            let w = sw(permission_label(*o)) + 2;
            if x > 0 && x + w > bw {
                button_rows.push(vec![]);
                x = 0;
            }
            button_rows.last_mut().unwrap().push((*o, x));
            x += w + 1;
        }
        let ctrl_h = if inline { button_rows.len() } else { button_rows.len() + if compact { 0 } else { 1 } + 1 } + cpt * 2;
        let ctop = bottom.saturating_sub(ctrl_h);
        buf.fill_rect(0, ctop as i32, width as i32, ctrl_h as i32, t.pane);
        let by = ctop + cpt;
        for (r, rowv) in button_rows.iter().enumerate() {
            for (o, bx) in rowv {
                let selected = *o == self.state.selected;
                let label = permission_label(*o);
                let bg = if selected { t.action_focused_bg } else { Color::TRANSPARENT };
                buf.fill_rect((cpl + bx) as i32, (by + r) as i32, (sw(label) + 2) as i32, 1, if selected { t.action_focused_bg } else { t.pane });
                buf.draw_chunks((cpl + bx + 1) as i32, (by + r) as i32, &[StyledChunk { text: label.into(), fg: if selected { t.action_focused_text } else { t.action_secondary_text }, bg, attrs: if selected && mono { attr::INVERSE } else { 0 } }], sw(label));
            }
        }
        let hy = if inline { by } else { by + button_rows.len() + if compact { 0 } else { 1 } };
        if busy {
            let msg = if compact { "Waiting…" } else { "Waiting for permission event…" };
            let hx = if inline { width.saturating_sub(cpr + sw(msg)) } else { cpl };
            buf.draw_chunks(hx as i32, hy as i32, &[StyledChunk::plain(msg, t.running)], inner);
        } else {
            let chunks = if compact && width < 56 {
                vec![StyledChunk::plain("pgup/pgdn", t.text), StyledChunk::plain(" scroll", t.muted)]
            } else {
                vec![
                    StyledChunk::plain(if mono { "left/right" } else { "⇆" }, t.text),
                    StyledChunk::plain(" select  ", t.muted),
                    StyledChunk::plain("enter", t.text),
                    StyledChunk::plain(" confirm  ", t.muted),
                    StyledChunk::plain("esc", t.text),
                    StyledChunk::plain(format!(" {}", if stage == Stage::Always { "cancel" } else { "reject" }), t.muted),
                ]
            };
            let w: usize = chunks.iter().map(|c| sw(&c.text)).sum();
            let hx = if inline { width.saturating_sub(cpr + w) } else { cpl };
            buf.draw_chunks(hx as i32, hy as i32, &chunks, inner);
        }
        // Body scroll box.
        let (mpl, mpr, mpb) = if compact { (0, 0, 0) } else { (1, 3, 1) };
        let vpl = if compact { 0 } else { 1 };
        let vpr = if mono { 0 } else { 1 };
        let body_top = row;
        let body_h = ctop.saturating_sub(body_top + mpb);
        let body_w = width.saturating_sub(mpl + mpr);
        let content_w = body_w.saturating_sub(vpl + vpr + usize::from(!mono));
        let gapb = if compact { 0 } else { 1 };
        let mut blocks = Vec::new();
        if stage == Stage::Permission {
            let info = permission_info(p, directory, mono);
            let il = if compact { 0 } else { 1 };
            blocks.push(Block { x: il, rows: chunk_rows(&[StyledChunk::plain(format!("{} ", info.icon), t.muted), StyledChunk::plain(info.title.clone(), t.text)], content_w.saturating_sub(il), WrapMode::Word), buf: None });
            if let Some(diff) = &info.diff {
                if width >= 40 {
                    blocks.push(Block { x: 0, rows: vec![], buf: Some(structured::patch_diff_dock(diff, content_w.max(1), block, t.text)) });
                } else {
                    blocks.push(Block { x: 0, rows: text_rows(diff, t.text, content_w, WrapMode::Word), buf: None });
                }
            } else if let Some(patch) = &info.patch {
                blocks.push(Block { x: 0, rows: text_rows(patch, t.muted, content_w, WrapMode::Word), buf: None });
            } else {
                for line in &info.lines {
                    blocks.push(Block { x: 0, rows: text_rows(line, t.text, content_w, WrapMode::Word), buf: None });
                }
            }
            if info.diff.is_none() && info.patch.is_none() && info.lines.is_empty() {
                blocks.push(Block { x: 0, rows: text_rows("No diff provided", t.muted, content_w, WrapMode::Word), buf: None });
            }
            // The inner box groups the info row and content with `gap`; "No diff" sits after it.
        } else {
            for line in permission_always_lines(&p.request.action, p.request.save.as_deref().unwrap_or(&[])) {
                blocks.push(Block { x: 0, rows: text_rows(&line, t.text, content_w, WrapMode::Word), buf: None });
            }
        }
        let scrollbar = (!mono).then_some((t.surface, t.line, true));
        scroll_view(buf, mpl, body_top, body_w, body_h, &blocks, gapb, self.scroll, scrollbar, vpl, vpr);
        let _ = locale::width;
        None
    }
}

pub enum FormOutcome {
    None,
    Reply(FormReply),
    Cancel,
}

pub struct FormBody {
    state: FormBodyState,
    area: TextArea,
    scroll: usize,
}

impl FormBody {
    pub fn new(f: &MiniForm) -> FormBody {
        FormBody { state: create_form_body_state(&f.form), area: TextArea::new(), scroll: 0 }
    }

    pub fn form_id(&self) -> &str {
        &self.state.form_id
    }

    fn editing(&self, f: &MiniForm) -> bool {
        form_unsupported(&f.form).is_none() && !form_confirm(&f.form, &self.state) && (form_current(&f.form, &self.state).is_some_and(uf::textual) || self.state.editing)
    }

    fn sync_area(&mut self, f: &MiniForm) {
        let value = form_input(&self.state, form_current(&f.form, &self.state));
        if self.area.plain_text() != value {
            self.area.set_text(&value);
        }
    }

    fn submit(&mut self, f: &MiniForm, next: FormBodyState) -> FormOutcome {
        if let Some(invalid) = form_validate(&f.form, &next) {
            self.state = form_set_error(&next, &invalid);
            return FormOutcome::None;
        }
        match form_reply(f, &next) {
            Some(r) => {
                self.state = form_set_submitting(&next, true, "");
                FormOutcome::Reply(r)
            }
            None => {
                self.state = next;
                FormOutcome::None
            }
        }
    }

    pub fn failed(&mut self, error: String) {
        self.state = form_set_error(&self.state, &error);
    }

    fn move_field(&mut self, f: &MiniForm, direction: i64) {
        let n = f.form.fields.len() as i64 + 1;
        let next = ((self.state.field as i64 + direction + n) % n) as usize;
        if direction < 0 || form_confirm(&f.form, &self.state) {
            self.state = form_set_field(&self.state, &f.form, next);
            return;
        }
        if let Some(field) = form_current(&f.form, &self.state) {
            let key = uf::key(field);
            if uf::kind(field) == "external" {
                if self.state.answers.get(&key) != Some(&Value::Bool(true)) {
                    self.state = form_set_error(&self.state, &format!("Acknowledge {}", uf::label(field)));
                    return;
                }
            } else if let Some(invalid) = uf::validate(field, self.state.answers.get(&key)) {
                self.state = form_set_error(&self.state, &invalid);
                return;
            }
        }
        self.state = form_set_field(&self.state, &f.form, next);
    }

    pub fn key(&mut self, s: &Stroke, f: &MiniForm, _width: usize) -> FormOutcome {
        if self.state.submitting {
            return FormOutcome::None;
        }
        let name = s.name.as_str();
        if name == "escape" {
            self.state = form_set_submitting(&self.state, true, "");
            return FormOutcome::Cancel;
        }
        if name == "pageup" || name == "pagedown" {
            self.scroll = if name == "pageup" { self.scroll.saturating_sub(4) } else { self.scroll + 4 };
            return FormOutcome::None;
        }
        if form_unsupported(&f.form).is_some() {
            return FormOutcome::None;
        }
        if self.editing(f) {
            self.sync_area(f);
            if name == "return" && !s.shift && !s.meta {
                let next = form_commit_input(&self.state, &f.form, self.area.plain_text());
                self.state = next.clone();
                if !next.error.is_empty() {
                    return FormOutcome::None;
                }
                if form_single(&f.form) {
                    return self.submit(f, next);
                }
                self.state = form_set_field(&next, &f.form, next.field + 1);
                return FormOutcome::None;
            }
            if let Some(c) = s.is_char() {
                self.area.insert_text(&c.to_string());
            } else {
                match name {
                    "backspace" => self.area.delete_char_backward(),
                    "delete" => self.area.delete_char(),
                    "left" => self.area.move_left(),
                    "right" => self.area.move_right(),
                    _ if s.ctrl && name == "a" => self.area.line_home(),
                    _ if s.ctrl && name == "e" => self.area.line_end(),
                    _ if s.ctrl && name == "w" => self.area.delete_word_backward(),
                    _ if s.ctrl && name == "u" => self.area.delete_to_line_start(),
                    _ if s.ctrl && name == "j" => self.area.newline(),
                    _ => {}
                }
            }
            self.state = form_set_draft(&self.state, form_current(&f.form, &self.state), self.area.plain_text());
            return FormOutcome::None;
        }
        if matches!(name, "tab" | "left" | "right" | "h" | "l") {
            let dir = if s.shift || name == "left" || name == "h" { -1 } else { 1 };
            self.move_field(f, dir);
            return FormOutcome::None;
        }
        if form_confirm(&f.form, &self.state) && name == "return" {
            let st = self.state.clone();
            return self.submit(f, st);
        }
        let current = form_current(&f.form, &self.state).cloned();
        if let Some(field) = &current
            && uf::kind(field) == "external"
            && name == "return"
        {
            let key = uf::key(field);
            if self.state.answers.get(&key) == Some(&Value::Bool(true)) {
                if form_single(&f.form) {
                    let st = self.state.clone();
                    return self.submit(f, st);
                }
                self.move_field(f, 1);
                return FormOutcome::None;
            }
            if self.state.external_ready.contains(&key) {
                let next = form_acknowledge(&self.state, &f.form);
                self.state = next.clone();
                if form_single(&f.form) {
                    return self.submit(f, next);
                }
                return FormOutcome::None;
            }
            let url = field.get("url").and_then(|u| u.as_str()).unwrap_or_default().to_string();
            crate::app::commands::open_url(&url);
            self.state = form_set_external_ready(&self.state, &key);
            return FormOutcome::None;
        }
        let rows = current.as_ref().map(uf::rows).unwrap_or_default();
        let total = rows.len() + usize::from(current.as_ref().is_some_and(uf::custom));
        if let Some(d) = s.is_char().and_then(|c| c.to_digit(10))
            && d >= 1
            && (d as usize) <= total.min(9)
        {
            return self.choose(f, d as usize - 1);
        }
        match name {
            "up" | "k" => self.state = form_move(&self.state, &f.form, -1),
            "down" | "j" => self.state = form_move(&self.state, &f.form, 1),
            "return" => {
                let sel = self.state.selected;
                return self.choose(f, sel);
            }
            _ => {}
        }
        FormOutcome::None
    }

    fn choose(&mut self, f: &MiniForm, selected: usize) -> FormOutcome {
        let base = form_set_selected(&self.state, selected);
        let next = form_pick(&base, &f.form);
        self.state = next.clone();
        let multiple = form_current(&f.form, &self.state).is_some_and(|fld| uf::kind(fld) == "multiselect");
        if next.editing {
            self.sync_area(f);
            return FormOutcome::None;
        }
        if multiple {
            return FormOutcome::None;
        }
        if form_single(&f.form) {
            return self.submit(f, next);
        }
        FormOutcome::None
    }

    pub fn paste(&mut self, text: &str) {
        self.area.insert_text(text);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn render(&mut self, buf: &mut Buffer, y: usize, width: usize, height: usize, t: &FooterTheme, f: &MiniForm, mono: bool) -> Option<(usize, usize)> {
        let compact = width < 56 || height < 12;
        let (pl, pr, pt, pb) = if compact { (0, 0, 0, 0) } else { (2, 3, 1, 1) };
        buf.fill_rect(0, y as i32, width as i32, height as i32, t.surface);
        let inner = width.saturating_sub(pl + pr);
        let bottom = y + height - pb;
        let mut row = y + pt;
        let unsupported = form_unsupported(&f.form);
        let confirm = form_confirm(&f.form, &self.state);
        let current = form_current(&f.form, &self.state).cloned();
        let editing = self.editing(f);
        // Title row.
        let mut x = pl;
        buf.draw_chunks(x as i32, row as i32, &[StyledChunk::plain(if mono { "*" } else { "◆" }, if unsupported.is_some() { t.warning } else { t.question })], 1);
        x += 2;
        let counter = if unsupported.is_none() && !form_single(&f.form) {
            Some(if confirm { "Review".to_string() } else { format!("{}/{}", (self.state.field + 1).min(f.form.fields.len()), f.form.fields.len()) })
        } else {
            None
        };
        let title_w = inner.saturating_sub(2 + counter.as_ref().map(|c| sw(c) + 1).unwrap_or(0));
        let title = layout(&[StyledChunk::plain(f.form.title.clone(), t.text)], title_w.max(1), WrapMode::None, true);
        let tw = title.first().map(|l| l.width).unwrap_or(0);
        if let Some(l) = title.first() {
            buf.draw_chunks(x as i32, row as i32, &l.chunks, title_w);
        }
        if let Some(c) = counter {
            buf.draw_chunks((x + tw + 1) as i32, row as i32, &[StyledChunk::plain(c, t.muted)], inner);
        }
        row += 1 + if compact { 0 } else { 1 };
        // Footer hints.
        let action = if confirm {
            "submit".to_string()
        } else if editing {
            "save".into()
        } else if let Some(field) = current.as_ref().filter(|c| uf::kind(c) == "external") {
            let key = uf::key(field);
            if self.state.answers.get(&key) == Some(&Value::Bool(true)) {
                if form_single(&f.form) { "submit".into() } else { "next".into() }
            } else if self.state.external_ready.contains(&key) {
                if width < 24 { "done".into() } else { "acknowledge".into() }
            } else {
                "open URL".into()
            }
        } else {
            "choose".into()
        };
        let rows_count = current.as_ref().map(uf::rows).map(|r| r.len()).unwrap_or(0);
        let mut footer_rows: Vec<Vec<StyledChunk>> = Vec::new();
        if compact {
            let mut parts: Vec<(String, Color)> = Vec::new();
            if unsupported.is_none() {
                parts.push(if self.state.submitting { ("submitting…".into(), t.running) } else { (format!("enter {action}"), t.muted) });
            }
            if !self.state.submitting {
                parts.push(("esc dismiss".into(), t.muted));
            }
            if !self.state.submitting && height >= 10 {
                parts.push((format!("{}pgup/pgdn scroll", if width >= 56 && rows_count > 0 && !self.state.editing { "up/down select  tab next  " } else { "" }), t.muted));
            }
            // A wrapping flex row with a one-column gap.
            let mut line: Vec<StyledChunk> = Vec::new();
            let mut lw = 0;
            for (p, fg) in parts {
                if lw > 0 && lw + 1 + sw(&p) > width {
                    footer_rows.push(std::mem::take(&mut line));
                    lw = 0;
                }
                if lw > 0 {
                    line.push(StyledChunk::plain(" ", Color::WHITE));
                    lw += 1;
                }
                if sw(&p) > width {
                    for (i, r) in text_rows(&p, fg, width, WrapMode::Word).into_iter().enumerate() {
                        if i > 0 {
                            footer_rows.push(std::mem::take(&mut line));
                        }
                        line.extend(r);
                    }
                    lw = width;
                    continue;
                }
                lw += sw(&p);
                line.push(StyledChunk::plain(p, fg));
            }
            if !line.is_empty() {
                footer_rows.push(line);
            }
        } else {
            let text = if self.state.submitting {
                "submitting…".to_string()
            } else if unsupported.is_some() {
                "esc dismiss".into()
            } else if confirm {
                "enter submit   esc dismiss".into()
            } else if editing {
                "enter save   esc dismiss".into()
            } else if current.as_ref().is_some_and(|c| uf::kind(c) == "external") {
                format!("enter {action}   esc dismiss")
            } else if mono {
                "up/down select   enter choose   tab next   esc dismiss".into()
            } else {
                "↑↓ select   enter choose   tab next   esc dismiss".into()
            };
            let fg = if self.state.submitting { t.running } else { t.muted };
            let err_w = if self.state.error.is_empty() { 0 } else { sw(&self.state.error).min(inner / 2) + 1 };
            let mut r = text_rows(&text, fg, inner.saturating_sub(err_w), WrapMode::Word);
            if !self.state.error.is_empty() {
                let e = layout(&[StyledChunk::plain(self.state.error.clone(), t.error)], err_w.saturating_sub(1).max(1), WrapMode::None, true);
                if let (Some(first), Some(el)) = (r.first_mut(), e.first()) {
                    let pad = inner.saturating_sub(first.iter().map(|c| sw(&c.text)).sum::<usize>() + el.width);
                    first.push(StyledChunk::plain(" ".repeat(pad), fg));
                    first.extend(el.chunks.clone());
                }
            }
            footer_rows = r;
        }
        let fh = footer_rows.len();
        let footer_top = bottom.saturating_sub(fh);
        draw_rows(buf, pl, footer_top, &footer_rows, inner, bottom);
        let mut area_bottom = footer_top;
        if compact && !self.state.error.is_empty() {
            area_bottom = area_bottom.saturating_sub(1);
            let e = layout(&[StyledChunk::plain(self.state.error.clone(), t.error)], inner.max(1), WrapMode::None, true);
            if let Some(l) = e.first() {
                buf.draw_chunks(pl as i32, area_bottom as i32, &l.chunks, inner);
            }
        }
        // Scroll content.
        let gap = if compact { 0 } else { 1 };
        let cw = inner.saturating_sub(if mono { 0 } else { 1 });
        let mut blocks: Vec<Block> = Vec::new();
        if let Some(m) = f.form.metadata.as_ref().and_then(|m| m.get("message")).and_then(|m| m.as_str()) {
            blocks.push(Block { x: 0, rows: text_rows(m, t.muted, cw, WrapMode::Word), buf: None });
        }
        if let Some(u) = &unsupported {
            blocks.push(Block { x: 0, rows: text_rows(u, t.warning, cw, WrapMode::Word), buf: None });
            blocks.push(Block { x: 0, rows: text_rows("This request remains pending until you dismiss it.", t.muted, cw, WrapMode::Word), buf: None });
        }
        if unsupported.is_none() {
            if let Some(field) = current.as_ref().filter(|c| uf::kind(c) == "external") {
                let key = uf::key(field);
                let desc = field.get("description").and_then(|d| d.as_str()).map(str::to_string).unwrap_or_else(|| uf::label(field));
                blocks.push(Block { x: 0, rows: text_rows(&desc, t.text, cw, WrapMode::Word), buf: None });
                blocks.push(Block { x: 0, rows: text_rows(field.get("url").and_then(|u| u.as_str()).unwrap_or_default(), t.link, cw, WrapMode::Word), buf: None });
                let ack = self.state.answers.get(&key) == Some(&Value::Bool(true));
                let msg = if ack { "Acknowledged" } else if self.state.external_ready.contains(&key) { "Press enter to acknowledge completion" } else { "Press enter to open the URL" };
                blocks.push(Block { x: 0, rows: text_rows(msg, if ack { t.selection } else { t.muted }, cw, WrapMode::Word), buf: None });
            } else if let Some(field) = current.as_ref().filter(|_| !confirm) {
                let multiple = uf::kind(field) == "multiselect";
                let desc = field.get("description").and_then(|d| d.as_str()).map(str::to_string).unwrap_or_else(|| uf::label(field));
                let label = format!("{desc}{}{}", if field.get("required").and_then(|r| r.as_bool()) == Some(true) { " (required)" } else { "" }, if multiple { " (select all that apply)" } else { "" });
                blocks.push(Block { x: 0, rows: text_rows(&label, t.text, cw, WrapMode::Word), buf: None });
                if !uf::textual(field) && !self.state.editing {
                    let key = uf::key(field);
                    let value = self.state.answers.get(&key).cloned();
                    // (ordinal, main rows, description rows, inline description, active)
                    let mut items: Vec<ChoiceRow> = Vec::new();
                    for (i, (v, label, description)) in uf::rows(field).iter().enumerate() {
                        let active = self.state.selected == i;
                        let picked = match &value {
                            Some(Value::Array(a)) => a.iter().any(|x| x.as_str() == Some(&crate::mini::util::js_string(v))),
                            Some(x) => x == v,
                            None => false,
                        };
                        let ordinal = if mono { format!("{}{}.", if active { ">" } else { " " }, i + 1) } else { format!("{}.", i + 1) };
                        let fg = if active { t.formfield_focused_text } else { t.formfield_text };
                        let mut main = vec![];
                        if multiple {
                            main.push(StyledChunk::plain(format!("[{}] ", if picked { "x" } else { " " }), if picked { t.selection } else { fg }));
                        }
                        main.push(StyledChunk::plain(label.clone(), fg));
                        if !multiple && picked {
                            main.push(StyledChunk::plain(" *", t.selection));
                        }
                        let ow = sw(&ordinal) + 1;
                        let avail = cw.saturating_sub(ow);
                        let inline = description.as_ref().is_some_and(|d| sw(&ordinal) + 2 + sw(label) + if multiple { 4 } else if picked { 2 } else { 0 } + sw(d) <= width.saturating_sub(if compact { 0 } else { 5 }).saturating_sub(if mono { 0 } else { 1 }));
                        if inline {
                            items.push((ordinal, chunk_rows(&main, avail, WrapMode::Word), vec![], Some(vec![StyledChunk::plain(description.clone().unwrap(), t.muted)]), active));
                        } else {
                            let d = description.as_ref().map(|d| text_rows(d, t.muted, avail, WrapMode::Word)).unwrap_or_default();
                            items.push((ordinal, chunk_rows(&main, avail, WrapMode::Word), d, None, active));
                        }
                    }
                    if uf::custom(field) {
                        let n = uf::rows(field).len();
                        let active = self.state.selected == n;
                        let fg = if active { t.formfield_focused_text } else { t.formfield_text };
                        let ordinal = if mono { format!("{}{}.", if active { ">" } else { " " }, n + 1) } else { format!("{}.", n + 1) };
                        let avail = cw.saturating_sub(sw(&ordinal) + 1);
                        items.push((ordinal, text_rows("Type your own answer", fg, avail, WrapMode::Word), vec![], None, active));
                    }
                    let total: usize = items.iter().map(|it| it.1.len() + it.2.len()).sum();
                    let mut opt = Buffer::new(cw.max(1) as u16, total.max(1) as u16);
                    let mut r = 0;
                    for (ordinal, main, desc, inline, active) in items {
                        let fg = if active { t.formfield_focused_text } else { t.formfield_text };
                        let h = main.len() + desc.len();
                        if active {
                            opt.fill_rect(0, r as i32, cw as i32, h as i32, t.formfield_focused_bg);
                        }
                        let ow = sw(&ordinal) + 1;
                        opt.draw_chunks(0, r as i32, &[StyledChunk::plain(ordinal, fg)], ow);
                        for (k, line) in main.iter().enumerate() {
                            opt.draw_chunks(ow as i32, (r + k) as i32, line, cw.saturating_sub(ow));
                        }
                        if let Some(d) = inline {
                            let mw: usize = main.first().map(|l| l.iter().map(|c| sw(&c.text)).sum()).unwrap_or(0);
                            opt.draw_chunks((ow + mw + 1) as i32, r as i32, &d, cw.saturating_sub(ow + mw + 1));
                        }
                        for (k, line) in desc.iter().enumerate() {
                            opt.draw_chunks(ow as i32, (r + main.len() + k) as i32, line, cw.saturating_sub(ow));
                        }
                        r += h;
                    }
                    blocks.push(Block { x: 0, rows: vec![], buf: Some(opt) });
                }
            }
            if confirm {
                let mut rows = Vec::new();
                for field in &f.form.fields {
                    let key = uf::key(field);
                    let v = if uf::kind(field) == "external" {
                        if self.state.answers.get(&key) == Some(&Value::Bool(true)) { "acknowledged".to_string() } else { "required".into() }
                    } else {
                        let d = form_display(field, self.state.answers.get(&key));
                        if d.is_empty() { "(not answered)".into() } else { d }
                    };
                    rows.extend(text_rows(&format!("{}: {v}", uf::label(field)), t.muted, cw, WrapMode::Word));
                }
                blocks.push(Block { x: 0, rows, buf: None });
            }
        }
        if sw(&self.state.error) > width || self.state.error.contains('\n') {
            blocks.push(Block { x: 0, rows: text_rows(&self.state.error, t.error, cw, WrapMode::Word), buf: None });
        }
        let content_h: usize = blocks.iter().map(|b| b.height()).sum::<usize>() + gap * blocks.len().saturating_sub(1);
        let editing_area = unsupported.is_none() && current.as_ref().is_some_and(|c| uf::kind(c) != "external") && editing;
        let mut cursor = None;
        let mut area_h = 0;
        if editing_area {
            self.sync_area_if_idle(f);
            let maxh = 1.max(3.min(height.saturating_sub(6)));
            area_h = self.area.rows(inner, t.formfield_focused_text).len().clamp(1, maxh);
        }
        let (view_h, text_y) = if !compact && editing_area {
            let vh = content_h.min(height.saturating_sub(9).max(1));
            (vh, row + vh + 1)
        } else if editing_area {
            let ty = area_bottom.saturating_sub(area_h);
            (ty.saturating_sub(row), ty)
        } else {
            (area_bottom.saturating_sub(row), 0)
        };
        let scrollbar = (!mono).then_some((t.surface, t.line, false));
        scroll_view(buf, pl, row, inner, view_h, &blocks, gap, self.scroll, scrollbar, 0, if mono { 0 } else { 1 });
        if editing_area {
            let rows = self.area.rows(inner, t.formfield_focused_text);
            buf.fill_rect(pl as i32, text_y as i32, inner as i32, area_h as i32, t.formfield_focused_bg);
            if self.area.plain_text().is_empty() {
                let ph = form_placeholder(current.as_ref());
                buf.draw_chunks(pl as i32, text_y as i32, &[StyledChunk { text: ph, fg: t.muted, bg: t.formfield_focused_bg, attrs: 0 }], inner);
            } else {
                for (i, r) in rows.iter().take(area_h).enumerate() {
                    buf.draw_chunks(pl as i32, (text_y + i) as i32, &r.chunks, inner);
                }
            }
            let (cr, cc) = self.area.visual_cursor(inner);
            cursor = Some((pl + cc, text_y + cr.min(area_h - 1)));
        }
        cursor
    }

    fn sync_area_if_idle(&mut self, f: &MiniForm) {
        if self.area.plain_text().is_empty() {
            self.sync_area(f);
        }
    }
}
