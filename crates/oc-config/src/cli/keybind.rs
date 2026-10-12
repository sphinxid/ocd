//! TUI keybind configuration (`packages/tui/src/config/keybind.ts`).
//!
//! The `Definitions` table (name, default binding, description) is ported verbatim and in
//! source order. Decoding reproduces the Effect Schema `KeybindOverrides` struct: known keys
//! are validated against `BindingValue`, unknown keys are stripped, and the decoded object is
//! keyed in definition order (Effect emits struct properties in schema order).

use indexmap::IndexMap;
use serde_json::{Map, Value};

/// `LeaderDefault`.
pub const LEADER_DEFAULT: &str = "ctrl+x";

/// A definition's default binding value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultBinding {
    /// A comma-separated binding string (or `"none"`).
    Str(&'static str),
    /// A structured binding, stored as JSON text.
    Json(&'static str),
}

impl DefaultBinding {
    pub fn to_value(&self) -> Value {
        match self {
            DefaultBinding::Str(s) => Value::String((*s).to_string()),
            DefaultBinding::Json(text) => serde_json::from_str(text).expect("valid default binding JSON"),
        }
    }
}

/// One keybind definition (`keybind(value, description)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Definition {
    pub name: &'static str,
    pub default: DefaultBinding,
    pub description: &'static str,
}

/// `TuiKeybind.Definitions`, in source order.
pub static DEFINITIONS: &[Definition] = &[
    Definition { name: "leader", default: DefaultBinding::Str(LEADER_DEFAULT), description: "Leader key for keybind combinations" },
    Definition { name: "app.exit", default: DefaultBinding::Str("ctrl+c,ctrl+d,<leader>q"), description: "Exit the application" },
    Definition { name: "app.clear", default: DefaultBinding::Str("ctrl+l"), description: "Clear the screen in mini" },
    Definition { name: "app.debug", default: DefaultBinding::Str("none"), description: "Toggle debug panel" },
    Definition { name: "app.console", default: DefaultBinding::Str("none"), description: "Toggle console" },
    Definition { name: "app.scrap", default: DefaultBinding::Str("none"), description: "Open scrap screen" },
    Definition { name: "app.toggle.animations", default: DefaultBinding::Str("none"), description: "Toggle animations" },
    Definition { name: "app.toggle.file_context", default: DefaultBinding::Str("none"), description: "Toggle file context" },
    Definition { name: "app.toggle.diffwrap", default: DefaultBinding::Str("none"), description: "Toggle diff wrapping" },
    Definition { name: "app.toggle.paste_summary", default: DefaultBinding::Str("none"), description: "Toggle paste summary" },
    Definition { name: "command.palette.show", default: DefaultBinding::Str("ctrl+p"), description: "List available commands" },
    Definition { name: "help.show", default: DefaultBinding::Str("none"), description: "Open help dialog" },
    Definition { name: "docs.open", default: DefaultBinding::Str("none"), description: "Open documentation" },
    Definition { name: "opencode.settings", default: DefaultBinding::Str("none"), description: "Open settings" },
    Definition { name: "server.pair", default: DefaultBinding::Str("none"), description: "Pair device" },
    Definition { name: "service.restart", default: DefaultBinding::Str("none"), description: "Restart service" },
    Definition { name: "location.reload", default: DefaultBinding::Str("none"), description: "Reload configuration" },
    Definition { name: "diff.open", default: DefaultBinding::Str("none"), description: "Open diff viewer" },
    Definition { name: "diff.close", default: DefaultBinding::Str("escape,q"), description: "Close diff viewer" },
    Definition { name: "diff.down", default: DefaultBinding::Str("j,down"), description: "Move diff viewer down" },
    Definition { name: "diff.up", default: DefaultBinding::Str("k,up"), description: "Move diff viewer up" },
    Definition { name: "diff.page.down", default: DefaultBinding::Str("pagedown,ctrl+f"), description: "Page diff viewer down" },
    Definition { name: "diff.page.up", default: DefaultBinding::Str("pageup,ctrl+b"), description: "Page diff viewer up" },
    Definition { name: "diff.half_page.down", default: DefaultBinding::Str("ctrl+d"), description: "Scroll diff viewer down half a page" },
    Definition { name: "diff.half_page.up", default: DefaultBinding::Str("ctrl+u"), description: "Scroll diff viewer up half a page" },
    Definition { name: "diff.first", default: DefaultBinding::Str("gg,home"), description: "Go to the start of the diff" },
    Definition { name: "diff.last", default: DefaultBinding::Str("shift+g,end"), description: "Go to the end of the diff" },
    Definition { name: "diff.toggle", default: DefaultBinding::Str("none"), description: "Deprecated: file tree is mouse-controlled" },
    Definition { name: "diff.expand", default: DefaultBinding::Str("none"), description: "Deprecated: file tree is mouse-controlled" },
    Definition { name: "diff.expand_all", default: DefaultBinding::Str("none"), description: "Deprecated: file tree is mouse-controlled" },
    Definition { name: "diff.collapse", default: DefaultBinding::Str("none"), description: "Deprecated: file tree is mouse-controlled" },
    Definition { name: "diff.switch_focus", default: DefaultBinding::Str("none"), description: "Deprecated: keyboard navigation always controls the diff" },
    Definition { name: "diff.next_hunk", default: DefaultBinding::Str("]"), description: "Jump to next diff hunk" },
    Definition { name: "diff.previous_hunk", default: DefaultBinding::Str("["), description: "Jump to previous diff hunk" },
    Definition { name: "diff.next_file", default: DefaultBinding::Str("n,alt+down"), description: "Jump to next diff file" },
    Definition { name: "diff.previous_file", default: DefaultBinding::Str("p,alt+up"), description: "Jump to previous diff file" },
    Definition { name: "diff.toggle_file_tree", default: DefaultBinding::Str("b"), description: "Toggle diff viewer file tree" },
    Definition { name: "diff.single_patch", default: DefaultBinding::Str("s"), description: "Toggle single patch view" },
    Definition { name: "diff.switch_source", default: DefaultBinding::Str("d"), description: "Switch diff viewer source" },
    Definition { name: "diff.toggle_view", default: DefaultBinding::Str("v"), description: "Toggle diff viewer split or unified view" },
    Definition { name: "diff.mark_reviewed", default: DefaultBinding::Str("m"), description: "Toggle selected diff file reviewed" },
    Definition { name: "diff.help", default: DefaultBinding::Str("?,shift+?,shift+/"), description: "Show more diff viewer shortcuts" },
    Definition { name: "prompt.editor", default: DefaultBinding::Str("<leader>e"), description: "Open external editor" },
    Definition { name: "theme.switch", default: DefaultBinding::Str("none"), description: "List available themes" },
    Definition { name: "theme.switch_mode", default: DefaultBinding::Str("none"), description: "Switch between light and dark theme mode" },
    Definition { name: "theme.mode.lock", default: DefaultBinding::Str("none"), description: "Lock or unlock theme mode" },
    Definition { name: "session.sidebar.toggle", default: DefaultBinding::Str("<leader>b"), description: "Toggle sidebar" },
    Definition { name: "pane.focus.left", default: DefaultBinding::Str("<leader>left"), description: "Focus session pane" },
    Definition { name: "pane.focus.right", default: DefaultBinding::Str("<leader>right"), description: "Focus right pane" },
    Definition { name: "terminal.select", default: DefaultBinding::Str("<leader>down"), description: "Select terminal" },
    Definition { name: "terminal.toggle", default: DefaultBinding::Str("<leader>t"), description: "Toggle terminal pane" },
    Definition { name: "terminal.close", default: DefaultBinding::Str("<leader>up"), description: "Close terminal pane" },
    Definition { name: "session.toggle.scrollbar", default: DefaultBinding::Str("none"), description: "Toggle session scrollbar" },
    Definition { name: "opencode.status", default: DefaultBinding::Str("<leader>s"), description: "View status" },
    Definition { name: "opencode.debug", default: DefaultBinding::Str("none"), description: "View debug info" },
    Definition { name: "session.export", default: DefaultBinding::Str("<leader>x"), description: "Export session to editor" },
    Definition { name: "session.copy", default: DefaultBinding::Str("none"), description: "Copy session transcript" },
    Definition { name: "session.copy.id", default: DefaultBinding::Str("none"), description: "Copy session ID" },
    Definition { name: "session.move", default: DefaultBinding::Str("none"), description: "Manage workspaces" },
    Definition { name: "session.new", default: DefaultBinding::Str("<leader>n"), description: "Create a new session" },
    Definition { name: "session.list", default: DefaultBinding::Str("<leader>l"), description: "List all sessions" },
    Definition { name: "open.menu", default: DefaultBinding::Str("ctrl+o"), description: "Open recent sessions and projects" },
    Definition { name: "session.tab.next", default: DefaultBinding::Str("ctrl+tab,alt+down"), description: "Switch to next open session tab" },
    Definition { name: "session.tab.previous", default: DefaultBinding::Str("ctrl+shift+tab,alt+up"), description: "Switch to previous open session tab" },
    Definition { name: "session.tab.history.back", default: DefaultBinding::Str("none"), description: "Go back in session tab history" },
    Definition { name: "session.tab.history.forward", default: DefaultBinding::Str("ctrl+i"), description: "Go forward in session tab history" },
    Definition { name: "session.tab.next_unread", default: DefaultBinding::Str("alt+shift+down"), description: "Switch to next unread session tab" },
    Definition { name: "session.tab.previous_unread", default: DefaultBinding::Str("alt+shift+up"), description: "Switch to previous unread session tab" },
    Definition { name: "session.tab.close", default: DefaultBinding::Str("<leader>w"), description: "Close current session tab" },
    Definition { name: "session.tab.reopen", default: DefaultBinding::Str("ctrl+shift+t"), description: "Reopen last closed session tab" },
    Definition { name: "session.timeline", default: DefaultBinding::Str("<leader>g"), description: "Show session timeline" },
    Definition { name: "session.fork", default: DefaultBinding::Str("none"), description: "Fork session from message" },
    Definition { name: "session.rename", default: DefaultBinding::Str("ctrl+r"), description: "Rename session" },
    Definition { name: "session.delete", default: DefaultBinding::Str("ctrl+d"), description: "Delete session" },
    Definition { name: "session.share", default: DefaultBinding::Str("none"), description: "Share current session" },
    Definition { name: "session.unshare", default: DefaultBinding::Str("none"), description: "Unshare current session" },
    Definition { name: "session.interrupt", default: DefaultBinding::Str("escape"), description: "Interrupt current session" },
    Definition { name: "session.background", default: DefaultBinding::Str("ctrl+b"), description: "Background blocking session tools" },
    Definition { name: "session.compact", default: DefaultBinding::Str("<leader>c"), description: "Compact the session" },
    Definition { name: "session.aside", default: DefaultBinding::Str("none"), description: "Ask a side question" },
    Definition { name: "session.cd", default: DefaultBinding::Str("none"), description: "Change working directory" },
    Definition { name: "session.queued_prompts", default: DefaultBinding::Str("<leader>q"), description: "Manage queued prompts" },
    Definition { name: "queued_prompt.undo", default: DefaultBinding::Str("ctrl+u"), description: "Undo queued prompt" },
    Definition { name: "queued_prompt.delete", default: DefaultBinding::Str("ctrl+d"), description: "Delete queued prompt" },
    Definition { name: "session.toggle.exploration_grouping", default: DefaultBinding::Str("none"), description: "Toggle related tool call grouping" },
    Definition { name: "session.verbosity.cycle", default: DefaultBinding::Str("none"), description: "Cycle transcript verbosity" },
    Definition { name: "session.child.first", default: DefaultBinding::Str("down"), description: "Toggle subagent picker" },
    Definition { name: "session.child.next", default: DefaultBinding::Str("right"), description: "Go to next child session" },
    Definition { name: "session.child.previous", default: DefaultBinding::Str("left"), description: "Go to previous child session" },
    Definition { name: "session.parent", default: DefaultBinding::Str("up"), description: "Go to parent session" },
    Definition { name: "session.pin.toggle", default: DefaultBinding::Str("ctrl+f"), description: "Pin or unpin session in the session list" },
    Definition { name: "session.quick_switch.1", default: DefaultBinding::Str("<leader>1"), description: "Switch to session in quick slot 1" },
    Definition { name: "session.quick_switch.2", default: DefaultBinding::Str("<leader>2"), description: "Switch to session in quick slot 2" },
    Definition { name: "session.quick_switch.3", default: DefaultBinding::Str("<leader>3"), description: "Switch to session in quick slot 3" },
    Definition { name: "session.quick_switch.4", default: DefaultBinding::Str("<leader>4"), description: "Switch to session in quick slot 4" },
    Definition { name: "session.quick_switch.5", default: DefaultBinding::Str("<leader>5"), description: "Switch to session in quick slot 5" },
    Definition { name: "session.quick_switch.6", default: DefaultBinding::Str("<leader>6"), description: "Switch to session in quick slot 6" },
    Definition { name: "session.quick_switch.7", default: DefaultBinding::Str("<leader>7"), description: "Switch to session in quick slot 7" },
    Definition { name: "session.quick_switch.8", default: DefaultBinding::Str("<leader>8"), description: "Switch to session in quick slot 8" },
    Definition { name: "session.quick_switch.9", default: DefaultBinding::Str("<leader>9"), description: "Switch to session in quick slot 9" },
    Definition { name: "session.tab.select.1", default: DefaultBinding::Str("<leader>1,ctrl+1"), description: "Switch to session tab 1" },
    Definition { name: "session.tab.select.2", default: DefaultBinding::Str("<leader>2,ctrl+2"), description: "Switch to session tab 2" },
    Definition { name: "session.tab.select.3", default: DefaultBinding::Str("<leader>3,ctrl+3"), description: "Switch to session tab 3" },
    Definition { name: "session.tab.select.4", default: DefaultBinding::Str("<leader>4,ctrl+4"), description: "Switch to session tab 4" },
    Definition { name: "session.tab.select.5", default: DefaultBinding::Str("<leader>5,ctrl+5"), description: "Switch to session tab 5" },
    Definition { name: "session.tab.select.6", default: DefaultBinding::Str("<leader>6,ctrl+6"), description: "Switch to session tab 6" },
    Definition { name: "session.tab.select.7", default: DefaultBinding::Str("<leader>7,ctrl+7"), description: "Switch to session tab 7" },
    Definition { name: "session.tab.select.8", default: DefaultBinding::Str("<leader>8,ctrl+8"), description: "Switch to session tab 8" },
    Definition { name: "session.tab.select.9", default: DefaultBinding::Str("<leader>9,ctrl+9"), description: "Switch to session tab 9" },
    Definition { name: "session.tab.select.10", default: DefaultBinding::Str("<leader>0,ctrl+0"), description: "Switch to session tab 10" },
    Definition { name: "stash.delete", default: DefaultBinding::Str("ctrl+d"), description: "Delete stash entry" },
    Definition { name: "model.dialog.provider", default: DefaultBinding::Str("ctrl+a"), description: "Open provider list from model dialog" },
    Definition { name: "model.dialog.favorite", default: DefaultBinding::Str("ctrl+f"), description: "Toggle model favorite status" },
    Definition { name: "model.list", default: DefaultBinding::Str("<leader>m"), description: "List available models" },
    Definition { name: "model.cycle_recent", default: DefaultBinding::Str("f2"), description: "Next recently used model" },
    Definition { name: "model.cycle_recent_reverse", default: DefaultBinding::Str("shift+f2"), description: "Previous recently used model" },
    Definition { name: "model.cycle_favorite", default: DefaultBinding::Str("none"), description: "Next favorite model" },
    Definition { name: "model.cycle_favorite_reverse", default: DefaultBinding::Str("none"), description: "Previous favorite model" },
    Definition { name: "mcp.list", default: DefaultBinding::Str("none"), description: "List MCP servers" },
    Definition { name: "provider.connect", default: DefaultBinding::Str("none"), description: "Connect integration" },
    Definition { name: "agent.list", default: DefaultBinding::Str("<leader>a"), description: "List agents" },
    Definition { name: "agent.cycle", default: DefaultBinding::Str("shift+tab"), description: "Next agent" },
    Definition { name: "agent.cycle.reverse", default: DefaultBinding::Str("none"), description: "Previous agent" },
    Definition { name: "variant.cycle", default: DefaultBinding::Str("ctrl+t"), description: "Cycle model variants" },
    Definition { name: "variant.list", default: DefaultBinding::Str("none"), description: "List model variants" },
    Definition { name: "session.page.up", default: DefaultBinding::Str("pageup,ctrl+alt+b"), description: "Scroll messages up by one page" },
    Definition { name: "session.page.down", default: DefaultBinding::Str("pagedown,ctrl+alt+f"), description: "Scroll messages down by one page" },
    Definition { name: "session.line.up", default: DefaultBinding::Str("ctrl+alt+y"), description: "Scroll messages up by one line" },
    Definition { name: "session.line.down", default: DefaultBinding::Str("ctrl+alt+e"), description: "Scroll messages down by one line" },
    Definition { name: "session.half.page.up", default: DefaultBinding::Str("ctrl+alt+u"), description: "Scroll messages up by half page" },
    Definition { name: "session.half.page.down", default: DefaultBinding::Str("ctrl+alt+d"), description: "Scroll messages down by half page" },
    Definition { name: "session.first", default: DefaultBinding::Str("ctrl+g,home,alt+home"), description: "Navigate to first message" },
    Definition { name: "session.last", default: DefaultBinding::Str("ctrl+alt+g,end"), description: "Navigate to last message" },
    Definition { name: "session.message.next", default: DefaultBinding::Str("none"), description: "Navigate to next message" },
    Definition { name: "session.message.previous", default: DefaultBinding::Str("none"), description: "Navigate to previous message" },
    Definition { name: "session.message.user.next", default: DefaultBinding::Str("none"), description: "Navigate to next user message" },
    Definition { name: "session.message.user.previous", default: DefaultBinding::Str("none"), description: "Navigate to previous user message" },
    Definition { name: "session.messages_last_user", default: DefaultBinding::Str("alt+end"), description: "Navigate to last user message" },
    Definition { name: "messages.copy", default: DefaultBinding::Str("<leader>y"), description: "Copy message" },
    Definition { name: "session.undo", default: DefaultBinding::Str("<leader>u"), description: "Undo message" },
    Definition { name: "session.redo", default: DefaultBinding::Str("<leader>r"), description: "Redo message" },
    Definition { name: "session.toggle.thinking", default: DefaultBinding::Str("none"), description: "Toggle thinking blocks visibility" },
    Definition { name: "prompt.submit", default: DefaultBinding::Str("none"), description: "Submit prompt" },
    Definition { name: "prompt.queue", default: DefaultBinding::Str("<leader>return"), description: "Queue prompt" },
    Definition { name: "prompt.editor_context.clear", default: DefaultBinding::Str("none"), description: "Clear editor context" },
    Definition { name: "prompt.images.view", default: DefaultBinding::Str("<leader>i"), description: "View image attachments" },
    Definition { name: "prompt.skills", default: DefaultBinding::Str("none"), description: "Open skill selector" },
    Definition { name: "prompt.stash", default: DefaultBinding::Str("none"), description: "Stash prompt" },
    Definition { name: "prompt.stash.pop", default: DefaultBinding::Str("none"), description: "Pop stashed prompt" },
    Definition { name: "prompt.stash.list", default: DefaultBinding::Str("none"), description: "List stashed prompts" },
    Definition { name: "prompt.clear", default: DefaultBinding::Str("ctrl+c"), description: "Clear input field" },
    Definition { name: "prompt.paste", default: DefaultBinding::Json(r#"{"key":"ctrl+v","preventDefault":false}"#), description: "Paste from clipboard" },
    Definition { name: "input.submit", default: DefaultBinding::Str("return"), description: "Submit input" },
    Definition { name: "input.newline", default: DefaultBinding::Str("shift+return,ctrl+return,alt+return,ctrl+j"), description: "Insert newline in input" },
    Definition { name: "input.move.left", default: DefaultBinding::Str("left,ctrl+b"), description: "Move cursor left in input" },
    Definition { name: "input.move.right", default: DefaultBinding::Str("right,ctrl+f"), description: "Move cursor right in input" },
    Definition { name: "input.move.up", default: DefaultBinding::Str("up"), description: "Move cursor up in input" },
    Definition { name: "input.move.down", default: DefaultBinding::Str("down"), description: "Move cursor down in input" },
    Definition { name: "input.select.left", default: DefaultBinding::Str("shift+left"), description: "Select left in input" },
    Definition { name: "input.select.right", default: DefaultBinding::Str("shift+right"), description: "Select right in input" },
    Definition { name: "input.select.up", default: DefaultBinding::Str("shift+up"), description: "Select up in input" },
    Definition { name: "input.select.down", default: DefaultBinding::Str("shift+down"), description: "Select down in input" },
    Definition { name: "input.line.home", default: DefaultBinding::Str("ctrl+a"), description: "Move to start of line in input" },
    Definition { name: "input.line.end", default: DefaultBinding::Str("ctrl+e"), description: "Move to end of line in input" },
    Definition { name: "input.select.line.home", default: DefaultBinding::Str("ctrl+shift+a"), description: "Select to start of line in input" },
    Definition { name: "input.select.line.end", default: DefaultBinding::Str("ctrl+shift+e"), description: "Select to end of line in input" },
    Definition { name: "input.visual.line.home", default: DefaultBinding::Str("alt+a"), description: "Move to start of visual line in input" },
    Definition { name: "input.visual.line.end", default: DefaultBinding::Str("alt+e"), description: "Move to end of visual line in input" },
    Definition { name: "input.select.visual.line.home", default: DefaultBinding::Str("alt+shift+a"), description: "Select to start of visual line in input" },
    Definition { name: "input.select.visual.line.end", default: DefaultBinding::Str("alt+shift+e"), description: "Select to end of visual line in input" },
    Definition { name: "input.buffer.home", default: DefaultBinding::Str("none"), description: "Move to start of buffer in input" },
    Definition { name: "input.buffer.end", default: DefaultBinding::Str("none"), description: "Move to end of buffer in input" },
    Definition { name: "input.select.buffer.home", default: DefaultBinding::Str("shift+home"), description: "Select to start of buffer in input" },
    Definition { name: "input.select.buffer.end", default: DefaultBinding::Str("shift+end"), description: "Select to end of buffer in input" },
    Definition { name: "input.delete.line", default: DefaultBinding::Str("ctrl+shift+d"), description: "Delete line in input" },
    Definition { name: "input.delete.to.line.end", default: DefaultBinding::Str("ctrl+k"), description: "Delete to end of line in input" },
    Definition { name: "input.delete.to.line.start", default: DefaultBinding::Str("ctrl+u"), description: "Delete to start of line in input" },
    Definition { name: "input.backspace", default: DefaultBinding::Str("backspace,shift+backspace"), description: "Backspace in input" },
    Definition { name: "input.delete", default: DefaultBinding::Str("ctrl+d,delete,shift+delete"), description: "Delete character in input" },
    Definition { name: "input.undo", default: DefaultBinding::Str("ctrl+-,super+z"), description: "Undo in input" },
    Definition { name: "input.redo", default: DefaultBinding::Str("ctrl+.,super+shift+z"), description: "Redo in input" },
    Definition { name: "input.word.forward", default: DefaultBinding::Str("alt+f,alt+right,ctrl+right"), description: "Move word forward in input" },
    Definition { name: "input.word.backward", default: DefaultBinding::Str("alt+b,alt+left,ctrl+left"), description: "Move word backward in input" },
    Definition { name: "input.select.word.forward", default: DefaultBinding::Str("alt+shift+f,alt+shift+right"), description: "Select word forward in input" },
    Definition { name: "input.select.word.backward", default: DefaultBinding::Str("alt+shift+b,alt+shift+left"), description: "Select word backward in input" },
    Definition { name: "input.delete.word.forward", default: DefaultBinding::Str("alt+d,alt+delete,ctrl+delete"), description: "Delete word forward in input" },
    Definition { name: "input.delete.word.backward", default: DefaultBinding::Str("ctrl+w,ctrl+backspace,alt+backspace"), description: "Delete word backward in input" },
    Definition { name: "input.select.all", default: DefaultBinding::Str("super+a"), description: "Select all in input" },
    Definition { name: "prompt.history.previous", default: DefaultBinding::Str("up"), description: "Previous history item" },
    Definition { name: "prompt.history.next", default: DefaultBinding::Str("down"), description: "Next history item" },
    Definition { name: "composer.subagent.up", default: DefaultBinding::Str("up"), description: "Previous subagent" },
    Definition { name: "composer.subagent.down", default: DefaultBinding::Str("down"), description: "Next subagent" },
    Definition { name: "composer.subagent.select", default: DefaultBinding::Str("return"), description: "Navigate to subagent" },
    Definition { name: "composer.subagent.interrupt", default: DefaultBinding::Str("ctrl+d"), description: "Interrupt subagent" },
    Definition { name: "composer.shell.up", default: DefaultBinding::Str("up"), description: "Previous shell" },
    Definition { name: "composer.shell.down", default: DefaultBinding::Str("down"), description: "Next shell" },
    Definition { name: "composer.shell.select", default: DefaultBinding::Str("return"), description: "View shell output" },
    Definition { name: "composer.shell.kill", default: DefaultBinding::Str("ctrl+d"), description: "Kill shell command" },
    Definition { name: "composer.terminal.up", default: DefaultBinding::Str("up,k"), description: "Previous terminal" },
    Definition { name: "composer.terminal.down", default: DefaultBinding::Str("down,j"), description: "Next terminal" },
    Definition { name: "composer.terminal.select", default: DefaultBinding::Str("return"), description: "Select terminal" },
    Definition { name: "dialog.select.prev", default: DefaultBinding::Str("up,ctrl+p"), description: "Move to previous dialog item" },
    Definition { name: "dialog.select.next", default: DefaultBinding::Str("down,ctrl+n"), description: "Move to next dialog item" },
    Definition { name: "dialog.select.page_up", default: DefaultBinding::Str("pageup"), description: "Move up one page in dialog" },
    Definition { name: "dialog.select.page_down", default: DefaultBinding::Str("pagedown"), description: "Move down one page in dialog" },
    Definition { name: "dialog.select.home", default: DefaultBinding::Str("home"), description: "Move to first dialog item" },
    Definition { name: "dialog.select.end", default: DefaultBinding::Str("end"), description: "Move to last dialog item" },
    Definition { name: "dialog.select.submit", default: DefaultBinding::Str("return"), description: "Submit selected dialog item" },
    Definition { name: "dialog.prompt.submit", default: DefaultBinding::Str("return"), description: "Submit dialog prompt" },
    Definition { name: "dialog.integration.rename", default: DefaultBinding::Str("ctrl+r"), description: "Rename integration account" },
    Definition { name: "dialog.integration.delete", default: DefaultBinding::Str("ctrl+d"), description: "Delete integration account" },
    Definition { name: "dialog.worktree.generate", default: DefaultBinding::Str("tab"), description: "Generate worktree name" },
    Definition { name: "dialog.move_session.new", default: DefaultBinding::Str("ctrl+a"), description: "New worktree" },
    Definition { name: "dialog.move_session.move", default: DefaultBinding::Str("ctrl+m"), description: "Move session to worktree" },
    Definition { name: "dialog.move_session.delete", default: DefaultBinding::Str("ctrl+d"), description: "Delete worktree" },
    Definition { name: "dialog.move_session.refresh", default: DefaultBinding::Str("ctrl+r"), description: "Refresh worktrees" },
    Definition { name: "prompt.autocomplete.prev", default: DefaultBinding::Str("up,ctrl+p"), description: "Move to previous autocomplete item" },
    Definition { name: "prompt.autocomplete.next", default: DefaultBinding::Str("down,ctrl+n"), description: "Move to next autocomplete item" },
    Definition { name: "prompt.autocomplete.hide", default: DefaultBinding::Str("escape"), description: "Hide autocomplete" },
    Definition { name: "prompt.autocomplete.select", default: DefaultBinding::Str("return"), description: "Select autocomplete item" },
    Definition { name: "prompt.autocomplete.complete", default: DefaultBinding::Str("tab"), description: "Complete autocomplete item" },
    Definition { name: "permission.prompt.fullscreen", default: DefaultBinding::Str("ctrl+f"), description: "Toggle permission prompt fullscreen" },
    Definition { name: "plugins.toggle", default: DefaultBinding::Str("return"), description: "Toggle plugin" },
    Definition { name: "dialog.mcp.toggle", default: DefaultBinding::Str("space"), description: "Toggle MCP server" },
    Definition { name: "dialog.plugins.error", default: DefaultBinding::Str("space"), description: "View plugin error" },
    Definition { name: "dialog.plugins.update", default: DefaultBinding::Str("ctrl+u"), description: "Update plugin from plugin dialog" },
    Definition { name: "dialog.plugins.check", default: DefaultBinding::Str("ctrl+r"), description: "Check for plugin updates from plugin dialog" },
    Definition { name: "terminal.suspend", default: DefaultBinding::Str("ctrl+z"), description: "Suspend terminal" },
    Definition { name: "terminal.title.toggle", default: DefaultBinding::Str("none"), description: "Toggle terminal title" },
    Definition { name: "plugins.list", default: DefaultBinding::Str("none"), description: "Open plugin manager dialog" },
    Definition { name: "which-key.toggle", default: DefaultBinding::Str("ctrl+alt+k"), description: "Toggle which-key panel" },
    Definition { name: "which-key.layout.toggle", default: DefaultBinding::Str("ctrl+alt+shift+k"), description: "Switch which-key layout" },
    Definition { name: "which-key.pending.toggle", default: DefaultBinding::Str("ctrl+alt+shift+p"), description: "Toggle which-key pending preview" },
    Definition { name: "which-key.group.previous", default: DefaultBinding::Str("ctrl+alt+left,ctrl+alt+["), description: "Previous which-key group" },
    Definition { name: "which-key.group.next", default: DefaultBinding::Str("ctrl+alt+right,ctrl+alt+]"), description: "Next which-key group" },
    Definition { name: "which-key.scroll.up", default: DefaultBinding::Str("ctrl+alt+up,ctrl+alt+p"), description: "Scroll which-key up" },
    Definition { name: "which-key.scroll.down", default: DefaultBinding::Str("ctrl+alt+down,ctrl+alt+n"), description: "Scroll which-key down" },
    Definition { name: "which-key.page.up", default: DefaultBinding::Str("ctrl+alt+pageup"), description: "Page which-key up" },
    Definition { name: "which-key.page.down", default: DefaultBinding::Str("ctrl+alt+pagedown"), description: "Page which-key down" },
    Definition { name: "which-key.home", default: DefaultBinding::Str("ctrl+alt+home"), description: "Jump to first which-key binding" },
    Definition { name: "which-key.end", default: DefaultBinding::Str("ctrl+alt+end"), description: "Jump to last which-key binding" },
];

/// `name in Definitions`.
pub fn is_definition(name: &str) -> bool {
    definition(name).is_some()
}

/// Look up a definition by its dotted command name.
pub fn definition(name: &str) -> Option<&'static Definition> {
    DEFINITIONS.iter().find(|d| d.name == name)
}

/// `TuiKeybind.defaultValue(name)`.
pub fn default_value(name: &str) -> Option<Value> {
    definition(name).map(|d| d.default.to_value())
}

/// `TuiKeybind.Descriptions[name]`.
pub fn description(name: &str) -> Option<&'static str> {
    definition(name).map(|d| d.description)
}

/// `TuiKeybind.unknownKeys(input)`: keys that are not definition names, in input order.
pub fn unknown_keys<'a>(input: impl IntoIterator<Item = &'a String>) -> Vec<String> {
    unknown_keys_in(DEFINITIONS, input)
}

pub(crate) fn unknown_keys_in<'a>(defs: &[Definition], input: impl IntoIterator<Item = &'a String>) -> Vec<String> {
    input.into_iter().filter(|k| !defs.iter().any(|d| d.name == k.as_str())).cloned().collect()
}

/// Decode `KeybindOverrides` (Effect `Schema.Struct` of optional `BindingValue`s).
///
/// Returns `None` when the input is not a plain object or any known key holds an invalid
/// binding. Unknown keys are stripped; the result is keyed in definition order.
pub fn decode_overrides(input: &Value) -> Option<IndexMap<String, Value>> {
    decode_overrides_in(DEFINITIONS, input)
}

pub(crate) fn decode_overrides_in(defs: &[Definition], input: &Value) -> Option<IndexMap<String, Value>> {
    let object = input.as_object()?;
    let mut out = IndexMap::new();
    for def in defs {
        if let Some(value) = object.get(def.name) {
            out.insert(def.name.to_string(), decode_binding_value(value)?);
        }
    }
    Some(out)
}

/// `TuiKeybind.parse(keybinds)`: rejects unknown names with the verbatim upstream message, then
/// resolves every definition to its override (`?? default`) decoded as a `BindingValue`.
///
/// Errors: `Unrecognized keybind: a` / `Unrecognized keybinds: a, b`. An override that does not
/// decode as a `BindingValue` yields `Invalid keybind: <name>` (upstream throws Effect's decode
/// error there; overrides that went through [`decode_overrides`] never hit this).
pub fn parse(keybinds: &IndexMap<String, Value>) -> Result<IndexMap<String, Value>, String> {
    parse_in(DEFINITIONS, keybinds)
}

pub(crate) fn parse_in(defs: &[Definition], keybinds: &IndexMap<String, Value>) -> Result<IndexMap<String, Value>, String> {
    let invalid = unknown_keys_in(defs, keybinds.keys());
    if !invalid.is_empty() {
        return Err(format!(
            "Unrecognized keybind{}: {}",
            if invalid.len() == 1 { "" } else { "s" },
            invalid.join(", ")
        ));
    }
    let mut out = IndexMap::new();
    for def in defs {
        let value = match keybinds.get(def.name) {
            Some(v) if !v.is_null() => v.clone(),
            _ => def.default.to_value(),
        };
        let decoded = decode_binding_value(&value).ok_or_else(|| format!("Invalid keybind: {}", def.name))?;
        out.insert(def.name.to_string(), decoded);
    }
    Ok(out)
}

/// Decode `BindingValue = false | "none" | BindingItem | BindingItem[]`.
pub fn decode_binding_value(value: &Value) -> Option<Value> {
    match value {
        Value::Bool(false) => Some(Value::Bool(false)),
        Value::Array(items) => items.iter().map(decode_binding_item).collect::<Option<Vec<_>>>().map(Value::Array),
        other => decode_binding_item(other),
    }
}

/// Decode `BindingItem = string | KeyStroke | BindingObject` (first matching member wins).
pub fn decode_binding_item(value: &Value) -> Option<Value> {
    match value {
        Value::String(s) => Some(Value::String(s.clone())),
        Value::Object(_) => decode_key_stroke(value).or_else(|| decode_binding_object(value)),
        _ => None,
    }
}

const KEY_STROKE_FLAGS: [&str; 5] = ["ctrl", "shift", "meta", "super", "hyper"];

/// `KeyStroke = { name: string, ctrl?, shift?, meta?, super?, hyper?: boolean }`.
pub fn decode_key_stroke(value: &Value) -> Option<Value> {
    let object = value.as_object()?;
    let mut out = Map::new();
    out.insert("name".into(), Value::String(object.get("name")?.as_str()?.to_string()));
    for flag in KEY_STROKE_FLAGS {
        if let Some(v) = object.get(flag) {
            out.insert(flag.into(), Value::Bool(v.as_bool()?));
        }
    }
    Some(Value::Object(out))
}

const BINDING_OBJECT_KEYS: [&str; 4] = ["key", "event", "preventDefault", "fallthrough"];

/// `BindingObject = { key: string | KeyStroke, event?: "press"|"release", preventDefault?, fallthrough?: boolean, [k]: unknown }`.
pub fn decode_binding_object(value: &Value) -> Option<Value> {
    let object = value.as_object()?;
    let mut out = Map::new();
    let key = object.get("key")?;
    let key = match key {
        Value::String(s) => Value::String(s.clone()),
        other => decode_key_stroke(other)?,
    };
    out.insert("key".into(), key);
    if let Some(event) = object.get("event") {
        match event.as_str() {
            Some("press") | Some("release") => {
                out.insert("event".into(), event.clone());
            }
            _ => return None,
        }
    }
    for flag in ["preventDefault", "fallthrough"] {
        if let Some(v) = object.get(flag) {
            out.insert(flag.into(), Value::Bool(v.as_bool()?));
        }
    }
    for (k, v) in object {
        if !BINDING_OBJECT_KEYS.contains(&k.as_str()) {
            out.insert(k.clone(), v.clone());
        }
    }
    Some(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn table_shape() {
        assert_eq!(DEFINITIONS.len(), 241);
        assert_eq!(DEFINITIONS[0].name, "leader");
        assert_eq!(default_value("leader"), Some(json!("ctrl+x")));
        assert_eq!(default_value("input.undo"), Some(json!("ctrl+-,super+z")));
        assert_eq!(default_value("prompt.paste"), Some(json!({"key": "ctrl+v", "preventDefault": false})));
        assert_eq!(description("which-key.end"), Some("Jump to last which-key binding"));
        assert!(is_definition("terminal.suspend"));
        assert!(!is_definition("app.heap_snapshot"));
        let mut names: Vec<_> = DEFINITIONS.iter().map(|d| d.name).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), DEFINITIONS.len());
    }

    #[test]
    fn decode_accepts_binding_forms_and_strips_unknown() {
        let decoded = decode_overrides(&json!({
            "session.new": "ctrl+n",
            "bogus.key": "x",
            "app.exit": false,
            "leader": ["ctrl+a", {"name": "b", "ctrl": true, "extra": 1}, {"key": {"name": "c"}, "event": "release", "custom": [1]}],
        }))
        .unwrap();
        // definition order, not input order
        assert_eq!(decoded.keys().collect::<Vec<_>>(), vec!["leader", "app.exit", "session.new"]);
        assert_eq!(
            decoded["leader"],
            json!(["ctrl+a", {"name": "b", "ctrl": true}, {"key": {"name": "c"}, "event": "release", "custom": [1]}])
        );
        assert_eq!(decoded["app.exit"], json!(false));
    }

    #[test]
    fn decode_rejects_invalid_known_values() {
        assert!(decode_overrides(&json!({"app.exit": true})).is_none());
        assert!(decode_overrides(&json!({"app.exit": 1})).is_none());
        assert!(decode_overrides(&json!({"app.exit": null})).is_none());
        assert!(decode_overrides(&json!({"app.exit": {"key": "a", "event": "hold"}})).is_none());
        assert!(decode_overrides(&json!({"app.exit": {"name": "a", "ctrl": "yes"}})).is_none());
        assert!(decode_overrides(&json!({"app.exit": [false]})).is_none());
        assert!(decode_overrides(&json!(["x"])).is_none());
        // invalid unknown keys are irrelevant
        assert!(decode_overrides(&json!({"nope": 5})).unwrap().is_empty());
    }

    #[test]
    fn keystroke_wins_over_binding_object() {
        assert_eq!(decode_binding_item(&json!({"name": "a", "key": "b"})), Some(json!({"name": "a"})));
    }

    #[test]
    fn parse_errors_and_defaults() {
        let mut one = IndexMap::new();
        one.insert("nope".to_string(), json!("x"));
        assert_eq!(parse(&one).unwrap_err(), "Unrecognized keybind: nope");
        one.insert("also.nope".to_string(), json!("y"));
        assert_eq!(parse(&one).unwrap_err(), "Unrecognized keybinds: nope, also.nope");

        let mut ok = IndexMap::new();
        ok.insert("app.exit".to_string(), json!("ctrl+q"));
        let parsed = parse(&ok).unwrap();
        assert_eq!(parsed.len(), DEFINITIONS.len());
        assert_eq!(parsed["app.exit"], json!("ctrl+q"));
        assert_eq!(parsed["leader"], json!("ctrl+x"));
    }
}
