//! The sidebar's todo list (`feature-plugins/sidebar/todo.tsx`, built-in TUI plugin `opencode.sidebar.todo`):
//! the session's current `todowrite` list after MCP, collapsible above two items, with the open state in the
//! plugin's keyed storage (`plugin.opencode.sidebar.todo.view`, `{"open": bool}`, open by default).

use super::App;
use crate::theme::Theme;
use crate::ui::text::*;
use crate::util::todo::{session_todos, todo_item};

pub const ID: &str = "opencode.sidebar.todo";
/// `storage.store("view")` of the plugin.
const STORAGE: &str = "plugin.opencode.sidebar.todo.view";
/// Click target of the section header.
pub const TOGGLE: &str = "sidebar.todo.toggle";

impl App {
    fn sidebar_todo_open(&self) -> bool {
        self.keyed.get(STORAGE).and_then(|v| v.get("open").and_then(|o| o.as_bool())).unwrap_or(true)
    }

    /// Appends the section to the sidebar content (`width`: the content column).
    pub(super) fn sidebar_todo(&self, lines: &mut Vec<SLine>, t: &Theme, sid: &str, width: usize) {
        let boundary = self.store.session(sid).and_then(|s| s.revert.as_ref().map(|r| r.message_id.clone()));
        let list = session_todos(self.store.messages(sid), boundary.as_deref());
        if list.is_empty() {
            return;
        }
        // The sidebar content stacks its sections one row apart.
        if lines.last().is_some_and(|l| !l.segs.is_empty()) {
            lines.push(SLine::blank());
        }
        let collapsible = list.len() > 2;
        let open = !collapsible || self.sidebar_todo_open();
        let mut head = Vec::new();
        if collapsible {
            head.push(Seg::new(if open { "▼ " } else { "▶ " }, fg(t.text.base)));
        }
        head.push(Seg::new("Todo", bold(fg(t.text.base))));
        if !open {
            let remaining = list.iter().filter(|item| item.status == "pending" || item.status == "in_progress").count();
            head.push(Seg::new(format!(" ({remaining} remaining)"), fg(t.text.muted)));
        }
        lines.push(SLine::new(head).on_click(collapsible.then(|| Click::Custom(TOGGLE.into()))));
        if open {
            for item in &list {
                lines.extend(todo_item(t, item, width));
            }
        }
    }

    /// Header click: flips and persists the open state (only offered above two items).
    pub(super) fn toggle_sidebar_todo(&mut self) {
        let open = self.sidebar_todo_open();
        self.keyed.update(STORAGE, move |v| {
            if !v.is_object() {
                *v = serde_json::json!({});
            }
            v["open"] = serde_json::json!(!open);
        });
        self.dirty = true;
    }
}
