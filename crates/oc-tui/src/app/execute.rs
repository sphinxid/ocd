//! The `execute` tool's detail dialog (`routes/session/dialog-execute.tsx`): the code and its
//! output, following the live tool part while child calls stream and the output arrives.

use super::App;
use crate::api::types::{Content, ToolPart};
use crate::ui::dialog::{DIALOG_INSET, Dialog, Footer, FooterItem, Size, TextDialog};
use crate::ui::text::*;
use crate::util::locale;
use ratatui::style::Style;
use serde_json::{Value, json};

const PURPOSE: &str = "execute";

fn execute_code(input: &Value) -> Option<String> {
    input.get("code").and_then(|c| c.as_str()).filter(|c| !c.is_empty()).map(str::to_string)
}

/// The output text: the error message, or the completed text content without ANSI codes.
fn output_text(t: &ToolPart) -> Option<String> {
    match t.state.status.as_str() {
        "error" => t.state.error.as_ref().map(|e| e.message.clone()).filter(|m| !m.is_empty()),
        "completed" => {
            let text: Vec<String> = t.state.content.iter().flatten().filter(|c| c.kind == "text").filter_map(|c| c.text.clone()).collect();
            let text = locale::strip_ansi(text.join("\n").trim());
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    }
}

/// A JSON result, optionally followed by "\n\nWarnings:" or "\n\nLogs:".
fn highlighted_output(text: &str) -> Option<(String, Option<String>)> {
    if !text.starts_with('{') && !text.starts_with('[') {
        return None;
    }
    let end = ["\n\nWarnings:\n", "\n\nLogs:\n"].iter().filter_map(|m| text.find(m)).min();
    let json = match end {
        Some(i) => &text[..i],
        None => text,
    };
    serde_json::from_str::<Value>(json).ok()?;
    let rest = text[json.len()..].trim();
    Some((json.to_string(), (!rest.is_empty()).then(|| rest.to_string())))
}

fn line_count(text: Option<&str>) -> usize {
    text.map(|t| t.split('\n').count()).unwrap_or(0)
}

fn failed(t: &ToolPart) -> bool {
    match t.state.status.as_str() {
        "error" => true,
        "streaming" => false,
        _ => t.state.metadata.as_ref().and_then(|m| m.get("error")) == Some(&Value::Bool(true)),
    }
}

impl App {
    fn execute_part(&self, message: &str, tool: &str) -> Option<ToolPart> {
        let sid = self.session_id()?;
        let fam = self.store.family_of(&self.store.root(sid));
        let ids = std::iter::once(sid.to_string()).chain(fam);
        for id in ids {
            if let Some(m) = self.store.messages(&id).iter().find(|m| m.id == message) {
                return m.content.iter().find_map(|c| match c {
                    Content::Tool(t) if t.id == tool => Some(t.clone()),
                    _ => None,
                });
            }
        }
        None
    }

    /// Open the detail dialog for an `execute` tool row.
    pub fn open_execute(&mut self, message: &str, tool: &str) {
        let mut d = Dialog::text(PURPOSE, TextDialog::new("execute", vec![]));
        d.size = Size::XLarge;
        d.centered = true;
        d.data = json!({ "message": message, "tool": tool, "copied": Value::Null });
        self.dialogs.clear();
        self.open_dialog(d);
        self.refresh_execute_dialog();
    }

    /// Rebuild the open detail dialog from the live tool part.
    pub(super) fn refresh_execute_dialog(&mut self) {
        let Some(d) = self.dialogs.last().filter(|d| d.purpose == PURPOSE) else { return };
        let message = d.data["message"].as_str().unwrap_or("").to_string();
        let tool = d.data["tool"].as_str().unwrap_or("").to_string();
        let copied = d.data["copied"].as_str().map(str::to_string);
        let scroll = match &d.body {
            crate::ui::dialog::Body::Text(td) => td.scroll,
            _ => 0,
        };
        let Some(part) = self.execute_part(&message, &tool) else { return };
        let td = self.execute_body(&part, copied.as_deref(), scroll);
        if let Some(d) = self.dialogs.last_mut() {
            d.body = crate::ui::dialog::Body::Text(Box::new(td));
        }
    }

    fn execute_body(&self, part: &ToolPart, copied: Option<&str>, scroll: usize) -> TextDialog {
        let base = self.theme().clone();
        let t = base.dialog().clone();
        let (cols, rows) = self.size;
        let width = (Size::XLarge.width().min(cols.saturating_sub(2)) as usize).saturating_sub(DIALOG_INSET * 2).max(20);
        let max_height = ((rows as usize * 7) / 10).saturating_sub(6).max(3);
        let code = execute_code(&part.state.input);
        let text = output_text(part);
        let json_out = text.as_deref().and_then(highlighted_output);
        let failed = failed(part);
        let digits = line_count(code.as_deref()).max(line_count(json_out.as_ref().map(|(j, _)| j.as_str()))).max(1).to_string().len();
        let code_w = width.saturating_sub(digits + 1).max(1);
        let muted = base.text.muted;
        // `GutteredCode`: one left-aligned gutter for both blocks, numbers only on a line's first row.
        let guttered = |content: &str, lang: &str| -> Vec<SLine> {
            let mut out = Vec::new();
            for (i, segs) in crate::ui::highlight::highlight(content, lang, &self.syntax).into_iter().enumerate() {
                let wrapped = wrap(&segs, code_w, Wrap::Char);
                let wrapped = if wrapped.is_empty() { vec![vec![]] } else { wrapped };
                for (j, row) in wrapped.into_iter().enumerate() {
                    let num = if j == 0 { format!("{:<digits$}", i + 1) } else { " ".repeat(digits) };
                    let mut l = vec![seg(num, fg(muted)), Seg::new(" ", Style::default())];
                    l.extend(row);
                    out.push(SLine::new(l));
                }
            }
            out
        };
        let title = |s: &str| SLine::new(vec![seg(s, bold(fg(muted)))]);
        let mut lines = vec![SLine::blank(), title("Code")];
        match &code {
            Some(c) => lines.extend(guttered(c, "typescript")),
            None => lines.push(SLine::new(vec![seg("Waiting for code…", fg(muted))])),
        }
        lines.push(SLine::blank());
        lines.push(title("Output"));
        let out_color = if failed { base.text.feedback.error.base } else { base.text.base };
        match (&json_out, &text) {
            (Some((json, rest)), _) => {
                lines.extend(guttered(json, "json"));
                if let Some(rest) = rest {
                    for row in wrap(&[seg(rest.clone(), fg(out_color))], width.saturating_sub(digits + 1).max(1), Wrap::Word) {
                        lines.push(SLine::new(row).prefixed(&[Seg::new(" ".repeat(digits + 1), Style::default())]));
                    }
                }
            }
            (None, Some(text)) => {
                for row in wrap(&[seg(text.clone(), fg(out_color))], width, Wrap::Word) {
                    lines.push(SLine::new(row));
                }
            }
            (None, None) => {
                let msg = if part.state.status == "completed" { "No output" } else { "Waiting for output…" };
                lines.push(SLine::new(vec![seg(msg, fg(muted))]));
            }
        }
        lines.push(SLine::blank());
        let status = match part.state.status.as_str() {
            "streaming" => "Receiving code…".to_string(),
            "running" => "Running".to_string(),
            _ => {
                let duration = part.time.completed.map(|c| format!(" · {}", locale::duration(c - part.time.ran.unwrap_or(part.time.created)))).unwrap_or_default();
                if failed { format!("Failed{duration}") } else { format!("Completed{duration}") }
            }
        };
        let copy = |kind: &str, label: &str, key: &str| {
            if copied == Some(kind) { FooterItem::new("✓ copied", "").color(t.text.feedback.success.base) } else { FooterItem::new(label, key) }
        };
        let mut td = TextDialog::new("execute", vec![]);
        td.title_extra = Some((status, false));
        td.title_extra_color = Some(if failed { t.text.feedback.error.base } else { t.text.muted });
        td.header_gap = 2;
        td.height = Some(lines.len().min(max_height));
        td.lines = lines;
        td.scroll = scroll;
        td.band = Some(base.background.raised.high);
        td.band_scrolls = true;
        td.footer = Some(Footer {
            secondary: vec![FooterItem::new("scroll", "↑/↓"), copy("code", "copy code", "c"), copy("output", "copy output", "o"), FooterItem::new("back", "esc")],
            ..Default::default()
        });
        td
    }

    /// `c` / `o`: copy the code or the output.
    pub(super) fn execute_copy(&mut self, kind: &str) {
        let Some(d) = self.dialogs.last().filter(|d| d.purpose == PURPOSE) else { return };
        let (message, tool) = (d.data["message"].as_str().unwrap_or("").to_string(), d.data["tool"].as_str().unwrap_or("").to_string());
        let Some(part) = self.execute_part(&message, &tool) else { return };
        let value = if kind == "code" { execute_code(&part.state.input) } else { output_text(&part) };
        let Some(value) = value else { return };
        crate::clipboard::write(&value);
        if let Some(d) = self.dialogs.last_mut() {
            d.data["copied"] = json!(kind);
        }
        self.refresh_execute_dialog();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_output_splits_off_warnings() {
        assert_eq!(highlighted_output("{\"a\":1}\n\nLogs:\nhello"), Some(("{\"a\":1}".into(), Some("Logs:\nhello".into()))));
        assert_eq!(highlighted_output("[1,2]"), Some(("[1,2]".into(), None)));
        assert_eq!(highlighted_output("{not json"), None);
        assert_eq!(highlighted_output("plain"), None);
    }
}
