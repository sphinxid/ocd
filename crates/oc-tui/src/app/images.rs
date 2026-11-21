//! Image previews: `SessionImages` in the transcript (`session.image_preview`), the prompt's attachment previews
//! (`prompt.image_preview`) and `DialogImagePreview` (clicking a preview, or `prompt.images.view`).

use std::collections::HashSet;
use std::sync::Arc;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use serde_json::Value;

use super::App;
use crate::graphics::decode::{Loaded, Source};
use crate::graphics::Fit;
use crate::ui::dialog::{Body, Dialog, Size};
use crate::ui::images::{self, ImagePreview, ImageRow, PREVIEW_PURPOSE, Strip};
use crate::ui::text::{Click, put};
use crate::util::locale::{self, width as sw};

/// OpenTUI's default text color, for `<text>` without `fg`.
const DEFAULT_FG: Color = Color::Rgb(255, 255, 255);

impl App {
    /// Rows of a transcript image strip, 0 while `session.image_preview` is off.
    pub fn session_image_height(&self) -> u16 {
        if self.config.bool("session.image_preview") == Some(true) { images::preview_height(self.size.1) } else { 0 }
    }

    /// `imageAttachments`: the prompt's inline images, deduplicated like `deduplicatePromptImages`, with their
    /// mentions.
    pub fn prompt_images(&self) -> Vec<(String, Option<String>)> {
        prompt_images(&self.editor.info().files)
    }

    /// The prompt's preview strip rows (`prompt.image_preview`), `inner` columns wide.
    pub(super) fn prompt_strip(&self, inner: u16) -> Option<Arc<Strip>> {
        if self.config.bool("prompt.image_preview") != Some(true) {
            return None;
        }
        let imgs = self.prompt_images();
        if imgs.is_empty() {
            return None;
        }
        let height = images::preview_height(self.size.1);
        let mut s = images::strip(imgs.iter().map(|(u, _)| u.clone()).collect(), height, inner, true);
        s.mentions = imgs.into_iter().map(|(_, m)| m).collect();
        Some(Arc::new(s))
    }

    /// Draw the strips whose rows are at `rows` (`(row index from area.y, row)`), clipped to `clip`.
    pub(super) fn draw_strip_rows(&mut self, buf: &mut Buffer, area: Rect, rows: &[(usize, ImageRow)], clip: Rect) {
        let mut seen: HashSet<usize> = HashSet::new();
        for (i, r) in rows {
            if !seen.insert(Arc::as_ptr(&r.strip) as usize) {
                continue;
            }
            let top = area.y as i32 + *i as i32 - r.row as i32;
            self.draw_image_strip(buf, area.x, top, &r.strip, clip);
        }
    }

    fn draw_image_strip(&mut self, buf: &mut Buffer, x0: u16, top: i32, strip: &Strip, clip: Rect) {
        let h = strip.height as u32;
        // The prompt's texts are muted; the transcript's have no color of their own.
        let text_fg = if strip.more_opens { self.theme().text.muted.tui() } else { DEFAULT_FG };
        for (j, &(bx, bw)) in strip.boxes.iter().enumerate() {
            let Some(uri) = strip.uris.get(j) else { continue };
            let bx = x0 + bx;
            match self.images.get(uri) {
                Loaded::Ready(p) => self.gfx.draw_at(buf, &p, bx as i32, top, bw as u32, h, Fit::Cover, clip),
                Loaded::Failed => center_text(buf, "No preview", bx, top, bw, strip.height, text_fg, clip),
                Loaded::Pending => {}
            }
            self.image_box_hit(Rect::new(bx, top.max(0) as u16, bw, 0), top, strip, j, clip);
        }
        if let Some((mx, mw, label)) = &strip.more {
            let mx = x0 + mx;
            center_text(buf, label, mx, top, *mw, strip.height, text_fg, clip);
            if strip.more_opens {
                self.image_box_hit(Rect::new(mx, 0, *mw, 0), top, strip, strip.boxes.len(), clip);
            }
        }
    }

    /// A click on the box opens the preview at `index`; it wins over the row's own click.
    fn image_box_hit(&mut self, r: Rect, top: i32, strip: &Strip, index: usize, clip: Rect) {
        let y0 = top.max(clip.y as i32);
        let y1 = (top + strip.height as i32).min(clip.bottom() as i32);
        if y1 <= y0 {
            return;
        }
        let rect = Rect::new(r.x, y0 as u16, r.width, (y1 - y0) as u16).intersection(clip);
        if rect.is_empty() {
            return;
        }
        self.hits.insert(0, (rect, Click::Custom(format!("{}{}", images::CLICK, self.image_hits.len()))));
        self.image_hits.push((strip.uris.clone(), strip.mentions.clone(), index));
    }

    /// `dialog.replace(() => <DialogImagePreview images initial />)`.
    pub fn open_image_preview(&mut self, uris: Vec<String>, mentions: Vec<Option<String>>, initial: usize) {
        if uris.is_empty() {
            return;
        }
        let mut d = Dialog::new(PREVIEW_PURPOSE, Body::Image(Box::new(ImagePreview::new(uris, mentions, initial))));
        d.size = Size::XLarge;
        d.centered = true;
        self.open_dialog(d);
    }

    /// `image.preview:<n>` was clicked.
    pub(super) fn image_click(&mut self, n: &str) {
        let Some((uris, mentions, index)) = n.parse::<usize>().ok().and_then(|n| self.image_hits.get(n).cloned()) else { return };
        self.open_image_preview(uris, mentions, index);
    }

    /// `prompt.images.view`: the preview of the prompt's images, from the first.
    pub(super) fn view_prompt_images(&mut self) -> bool {
        let imgs = self.prompt_images();
        if imgs.is_empty() {
            return false;
        }
        let (uris, mentions) = imgs.into_iter().unzip();
        self.open_image_preview(uris, mentions, 0);
        true
    }

    /// Before the dialog is drawn: whether its image failed (the footer says "No preview").
    pub(super) fn prepare_image_dialog(&mut self) {
        let Some(uri) = self.dialogs.last().and_then(|d| match &d.body {
            Body::Image(p) => p.current().map(|s| s.to_string()),
            _ => None,
        }) else {
            return;
        };
        let failed = self.images.failed(&uri);
        if let Some(Body::Image(p)) = self.dialogs.last_mut().map(|d| &mut d.body) {
            p.failed = failed;
        }
    }

    /// After the dialog is drawn in `rect`: its image, `width: 100%` and `height: max(3, rows - 8)`, fitted.
    pub(super) fn draw_image_dialog(&mut self, buf: &mut Buffer, rect: Rect, area: Rect) {
        let Some(uri) = self.dialogs.last().and_then(|d| match &d.body {
            Body::Image(p) => p.current().map(|s| s.to_string()),
            _ => None,
        }) else {
            return;
        };
        let inset = crate::ui::dialog::DIALOG_INSET as u16;
        let r = Rect::new(rect.x + inset, rect.y + 3, rect.width.saturating_sub(inset * 2), images::preview_image_rows(area.height)).intersection(rect);
        if let Some(p) = self.images.ready(&uri) {
            self.gfx.draw(buf, &p, r, Fit::Fit, r);
        }
    }

    /// Read the bytes of the images the last frame asked for (`media:` refs, `http(s):` URLs).
    pub(super) fn fetch_wanted_images(&mut self) {
        for uri in self.images.take_wanted() {
            match crate::graphics::decode::source_of(&uri) {
                Source::Media(_) => {
                    let (media, client, u) = (self.media.clone(), self.client.clone(), uri.clone());
                    self.spawn(async move { media.read(&client, &u).await }, move |app, r| {
                        app.images.resolve(&uri, r.ok().as_deref().map(|b| b.as_slice()));
                        app.dirty = true;
                    });
                }
                Source::Remote(url) => {
                    self.spawn(
                        async move {
                            let r = reqwest::get(&url).await?.error_for_status()?;
                            Ok(r.bytes().await?.to_vec())
                        },
                        move |app, r: anyhow::Result<Vec<u8>>| {
                            app.images.resolve(&uri, r.ok().as_deref());
                            app.dirty = true;
                        },
                    );
                }
                _ => self.images.resolve(&uri, None),
            }
        }
    }
}

/// `<text>` centered in a `width` x `height` box at `(x, top)` (`alignItems`/`justifyContent: center`), truncated.
#[allow(clippy::too_many_arguments)]
fn center_text(buf: &mut Buffer, text: &str, x: u16, top: i32, width: u16, height: u16, fg: Color, clip: Rect) {
    let text = locale::take_width(text, width as usize);
    let tw = sw(&text) as u16;
    let tx = x + (width - tw).div_ceil(2);
    let ty = top + height as i32 / 2;
    if ty < clip.y as i32 || ty >= clip.bottom() as i32 || ty < 0 {
        return;
    }
    let max = clip.right().saturating_sub(tx) as usize;
    if tx >= clip.x {
        put(buf, tx, ty as u16, &text, Style::default().fg(fg), max);
    }
}

/// `deduplicatePromptImages(files).filter(uri starts with data:image/)` with each file's mention text.
pub fn prompt_images(files: &[Value]) -> Vec<(String, Option<String>)> {
    let mut seen: std::collections::HashMap<String, HashSet<String>> = std::collections::HashMap::new();
    let dedupe = files.len() >= 2;
    files
        .iter()
        .filter(|f| {
            let uri = f.get("uri").and_then(|u| u.as_str()).unwrap_or("");
            let mention = f.get("mention").and_then(|m| m.get("text")).and_then(|t| t.as_str()).unwrap_or("");
            if !dedupe || !uri.starts_with("data:image/") || mention.is_empty() {
                return true;
            }
            let meta = serde_json::to_string(&(f.get("name"), f.get("description"), mention)).unwrap_or_default();
            seen.entry(meta).or_default().insert(uri.to_string())
        })
        .filter_map(|f| {
            let uri = f.get("uri").and_then(|u| u.as_str())?;
            uri.starts_with("data:image/").then(|| (uri.to_string(), f.get("mention").and_then(|m| m.get("text")).and_then(|t| t.as_str()).map(|s| s.to_string())))
        })
        .collect()
}

/// Draw a frame with images: render, finish the images (their escapes go out before the cell diff), then apply
/// the cell diff and the cursor. `write` receives the escapes.
pub fn draw_frame<B: ratatui::backend::Backend>(term: &mut ratatui::Terminal<B>, app: &mut App, write: &mut dyn FnMut(&str)) -> Result<(), B::Error> {
    term.autoresize()?;
    let mut frame = term.get_frame();
    app.draw(&mut frame);
    let bytes = app.gfx.finish(frame.buffer_mut());
    if !bytes.is_empty() {
        write(&bytes);
    }
    let cursor = app.frame_cursor.map(ratatui::layout::Position::from);
    term.apply_buffer_with_cursor(cursor)?;
    Ok(())
}
