//! Legacy v1 TUI configuration (`packages/tui/src/config/v1/index.tsx`, `v1/keybind.ts`).
//!
//! Used by the `cli.json` migration: `COMMAND_MAP` renames v1 snake_case keybind names to v2
//! dotted command names, and [`decode_tui_v1`] reproduces the whole-object Effect Schema decode
//! of `TuiConfigV1.Info` (excess properties ignored; any invalid declared field rejects the
//! whole document).

use indexmap::IndexMap;
use serde_json::{Map, Number, Value};

use super::keybind::{DefaultBinding, Definition, LEADER_DEFAULT};

/// v1 `TuiKeybind.Definitions` (snake_case names), in source order.
pub static V1_DEFINITIONS: &[Definition] = &[
    Definition { name: "leader", default: DefaultBinding::Str(LEADER_DEFAULT), description: "Leader key for keybind combinations" },
    Definition { name: "app_exit", default: DefaultBinding::Str("ctrl+c,ctrl+d,<leader>q"), description: "Exit the application" },
    Definition { name: "app_debug", default: DefaultBinding::Str("none"), description: "Toggle debug panel" },
    Definition { name: "app_console", default: DefaultBinding::Str("none"), description: "Toggle console" },
    Definition { name: "app_heap_snapshot", default: DefaultBinding::Str("none"), description: "Write heap snapshot" },
    Definition { name: "app_toggle_animations", default: DefaultBinding::Str("none"), description: "Toggle animations" },
    Definition { name: "app_toggle_file_context", default: DefaultBinding::Str("none"), description: "Toggle file context" },
    Definition { name: "app_toggle_diffwrap", default: DefaultBinding::Str("none"), description: "Toggle diff wrapping" },
    Definition { name: "app_toggle_paste_summary", default: DefaultBinding::Str("none"), description: "Toggle paste summary" },
    Definition { name: "command_list", default: DefaultBinding::Str("ctrl+p"), description: "List available commands" },
    Definition { name: "help_show", default: DefaultBinding::Str("none"), description: "Open help dialog" },
    Definition { name: "docs_open", default: DefaultBinding::Str("none"), description: "Open documentation" },
    Definition { name: "diff_open", default: DefaultBinding::Str("none"), description: "Open diff viewer" },
    Definition { name: "diff_close", default: DefaultBinding::Str("escape,q"), description: "Close diff viewer" },
    Definition { name: "diff_toggle", default: DefaultBinding::Str("enter,space"), description: "Toggle diff viewer item" },
    Definition { name: "diff_expand", default: DefaultBinding::Str("right"), description: "Expand diff viewer item" },
    Definition { name: "diff_expand_all", default: DefaultBinding::Str("E"), description: "Expand all diff viewer folders" },
    Definition { name: "diff_collapse", default: DefaultBinding::Str("left"), description: "Collapse diff viewer item" },
    Definition { name: "diff_switch_focus", default: DefaultBinding::Str("tab"), description: "Switch diff viewer focus" },
    Definition { name: "diff_next_hunk", default: DefaultBinding::Str("]"), description: "Jump to next diff hunk" },
    Definition { name: "diff_previous_hunk", default: DefaultBinding::Str("["), description: "Jump to previous diff hunk" },
    Definition { name: "diff_next_file", default: DefaultBinding::Str("n"), description: "Jump to next diff file" },
    Definition { name: "diff_previous_file", default: DefaultBinding::Str("p"), description: "Jump to previous diff file" },
    Definition { name: "diff_toggle_file_tree", default: DefaultBinding::Str("b"), description: "Toggle diff viewer file tree" },
    Definition { name: "diff_single_patch", default: DefaultBinding::Str("s"), description: "Toggle single patch view" },
    Definition { name: "diff_switch_source", default: DefaultBinding::Str("d"), description: "Switch diff viewer source" },
    Definition { name: "diff_toggle_view", default: DefaultBinding::Str("v"), description: "Toggle diff viewer split or unified view" },
    Definition { name: "diff_help", default: DefaultBinding::Str("?"), description: "Show more diff viewer shortcuts" },
    Definition { name: "editor_open", default: DefaultBinding::Str("<leader>e"), description: "Open external editor" },
    Definition { name: "theme_list", default: DefaultBinding::Str("<leader>t"), description: "List available themes" },
    Definition { name: "theme_switch_mode", default: DefaultBinding::Str("none"), description: "Switch between light and dark theme mode" },
    Definition { name: "theme_mode_lock", default: DefaultBinding::Str("none"), description: "Lock or unlock theme mode" },
    Definition { name: "sidebar_toggle", default: DefaultBinding::Str("<leader>b"), description: "Toggle sidebar" },
    Definition { name: "scrollbar_toggle", default: DefaultBinding::Str("none"), description: "Toggle session scrollbar" },
    Definition { name: "status_view", default: DefaultBinding::Str("<leader>s"), description: "View status" },
    Definition { name: "debug_view", default: DefaultBinding::Str("none"), description: "View debug info" },
    Definition { name: "session_export", default: DefaultBinding::Str("<leader>x"), description: "Export session to editor" },
    Definition { name: "session_copy", default: DefaultBinding::Str("none"), description: "Copy session transcript" },
    Definition { name: "session_move", default: DefaultBinding::Str("none"), description: "Move session" },
    Definition { name: "session_new", default: DefaultBinding::Str("<leader>n"), description: "Create a new session" },
    Definition { name: "session_list", default: DefaultBinding::Str("<leader>l"), description: "List all sessions" },
    Definition { name: "open_menu", default: DefaultBinding::Str("ctrl+o"), description: "Open recent sessions and projects" },
    Definition { name: "session_tab_next", default: DefaultBinding::Str("ctrl+tab,alt+down"), description: "Switch to next open tab" },
    Definition { name: "session_tab_previous", default: DefaultBinding::Str("ctrl+shift+tab,alt+up"), description: "Switch to previous open tab" },
    Definition { name: "session_tab_next_unread", default: DefaultBinding::Str("alt+shift+down"), description: "Switch to next unread tab" },
    Definition { name: "session_tab_previous_unread", default: DefaultBinding::Str("alt+shift+up"), description: "Switch to previous unread tab" },
    Definition { name: "session_tab_close", default: DefaultBinding::Str("<leader>w"), description: "Close current tab" },
    Definition { name: "session_tab_reopen", default: DefaultBinding::Str("ctrl+shift+t"), description: "Reopen last closed tab" },
    Definition { name: "session_timeline", default: DefaultBinding::Str("<leader>g"), description: "Show session timeline" },
    Definition { name: "session_fork", default: DefaultBinding::Str("none"), description: "Fork session from message" },
    Definition { name: "session_rename", default: DefaultBinding::Str("ctrl+r"), description: "Rename session" },
    Definition { name: "session_delete", default: DefaultBinding::Str("ctrl+d"), description: "Delete session" },
    Definition { name: "session_share", default: DefaultBinding::Str("none"), description: "Share current session" },
    Definition { name: "session_unshare", default: DefaultBinding::Str("none"), description: "Unshare current session" },
    Definition { name: "session_interrupt", default: DefaultBinding::Str("escape"), description: "Interrupt current session" },
    Definition { name: "session_background", default: DefaultBinding::Str("ctrl+b"), description: "Background blocking session tools" },
    Definition { name: "session_compact", default: DefaultBinding::Str("<leader>c"), description: "Compact the session" },
    Definition { name: "session_queued_prompts", default: DefaultBinding::Str("<leader>q"), description: "View queued prompts" },
    Definition { name: "queued_prompt_delete", default: DefaultBinding::Str("ctrl+d"), description: "Delete queued prompt" },
    Definition { name: "session_child_first", default: DefaultBinding::Str("down"), description: "Toggle subagent picker" },
    Definition { name: "session_parent", default: DefaultBinding::Str("up"), description: "Go to parent session" },
    Definition { name: "session_pin_toggle", default: DefaultBinding::Str("ctrl+f"), description: "Pin or unpin session in the session list" },
    Definition { name: "session_quick_switch_1", default: DefaultBinding::Str("<leader>1"), description: "Switch to session in quick slot 1" },
    Definition { name: "session_quick_switch_2", default: DefaultBinding::Str("<leader>2"), description: "Switch to session in quick slot 2" },
    Definition { name: "session_quick_switch_3", default: DefaultBinding::Str("<leader>3"), description: "Switch to session in quick slot 3" },
    Definition { name: "session_quick_switch_4", default: DefaultBinding::Str("<leader>4"), description: "Switch to session in quick slot 4" },
    Definition { name: "session_quick_switch_5", default: DefaultBinding::Str("<leader>5"), description: "Switch to session in quick slot 5" },
    Definition { name: "session_quick_switch_6", default: DefaultBinding::Str("<leader>6"), description: "Switch to session in quick slot 6" },
    Definition { name: "session_quick_switch_7", default: DefaultBinding::Str("<leader>7"), description: "Switch to session in quick slot 7" },
    Definition { name: "session_quick_switch_8", default: DefaultBinding::Str("<leader>8"), description: "Switch to session in quick slot 8" },
    Definition { name: "session_quick_switch_9", default: DefaultBinding::Str("<leader>9"), description: "Switch to session in quick slot 9" },
    Definition { name: "session_tab_select_1", default: DefaultBinding::Str("<leader>1,ctrl+1"), description: "Switch to tab 1" },
    Definition { name: "session_tab_select_2", default: DefaultBinding::Str("<leader>2,ctrl+2"), description: "Switch to tab 2" },
    Definition { name: "session_tab_select_3", default: DefaultBinding::Str("<leader>3,ctrl+3"), description: "Switch to tab 3" },
    Definition { name: "session_tab_select_4", default: DefaultBinding::Str("<leader>4,ctrl+4"), description: "Switch to tab 4" },
    Definition { name: "session_tab_select_5", default: DefaultBinding::Str("<leader>5,ctrl+5"), description: "Switch to tab 5" },
    Definition { name: "session_tab_select_6", default: DefaultBinding::Str("<leader>6,ctrl+6"), description: "Switch to tab 6" },
    Definition { name: "session_tab_select_7", default: DefaultBinding::Str("<leader>7,ctrl+7"), description: "Switch to tab 7" },
    Definition { name: "session_tab_select_8", default: DefaultBinding::Str("<leader>8,ctrl+8"), description: "Switch to tab 8" },
    Definition { name: "session_tab_select_9", default: DefaultBinding::Str("<leader>9,ctrl+9"), description: "Switch to tab 9" },
    Definition { name: "session_tab_select_10", default: DefaultBinding::Str("<leader>0,ctrl+0"), description: "Switch to tab 10" },
    Definition { name: "stash_delete", default: DefaultBinding::Str("ctrl+d"), description: "Delete stash entry" },
    Definition { name: "model_provider_list", default: DefaultBinding::Str("ctrl+a"), description: "Open provider list from model dialog" },
    Definition { name: "model_favorite_toggle", default: DefaultBinding::Str("ctrl+f"), description: "Toggle model favorite status" },
    Definition { name: "model_list", default: DefaultBinding::Str("<leader>m"), description: "List available models" },
    Definition { name: "model_cycle_recent", default: DefaultBinding::Str("f2"), description: "Next recently used model" },
    Definition { name: "model_cycle_recent_reverse", default: DefaultBinding::Str("shift+f2"), description: "Previous recently used model" },
    Definition { name: "model_cycle_favorite", default: DefaultBinding::Str("none"), description: "Next favorite model" },
    Definition { name: "model_cycle_favorite_reverse", default: DefaultBinding::Str("none"), description: "Previous favorite model" },
    Definition { name: "mcp_list", default: DefaultBinding::Str("none"), description: "List MCP servers" },
    Definition { name: "provider_connect", default: DefaultBinding::Str("none"), description: "Connect an integration" },
    Definition { name: "agent_list", default: DefaultBinding::Str("<leader>a"), description: "List agents" },
    Definition { name: "agent_cycle", default: DefaultBinding::Str("shift+tab"), description: "Next agent" },
    Definition { name: "agent_cycle_reverse", default: DefaultBinding::Str("none"), description: "Previous agent" },
    Definition { name: "variant_cycle", default: DefaultBinding::Str("ctrl+t"), description: "Cycle model variants" },
    Definition { name: "variant_list", default: DefaultBinding::Str("none"), description: "List model variants" },
    Definition { name: "messages_page_up", default: DefaultBinding::Str("pageup,ctrl+alt+b"), description: "Scroll messages up by one page" },
    Definition { name: "messages_page_down", default: DefaultBinding::Str("pagedown,ctrl+alt+f"), description: "Scroll messages down by one page" },
    Definition { name: "messages_line_up", default: DefaultBinding::Str("ctrl+alt+y"), description: "Scroll messages up by one line" },
    Definition { name: "messages_line_down", default: DefaultBinding::Str("ctrl+alt+e"), description: "Scroll messages down by one line" },
    Definition { name: "messages_half_page_up", default: DefaultBinding::Str("ctrl+alt+u"), description: "Scroll messages up by half page" },
    Definition { name: "messages_half_page_down", default: DefaultBinding::Str("ctrl+alt+d"), description: "Scroll messages down by half page" },
    Definition { name: "messages_first", default: DefaultBinding::Str("ctrl+g,home,alt+home"), description: "Navigate to first message" },
    Definition { name: "messages_last", default: DefaultBinding::Str("ctrl+alt+g,end"), description: "Navigate to last message" },
    Definition { name: "messages_next", default: DefaultBinding::Str("none"), description: "Navigate to next message" },
    Definition { name: "messages_previous", default: DefaultBinding::Str("none"), description: "Navigate to previous message" },
    Definition { name: "messages_next_user", default: DefaultBinding::Str("none"), description: "Navigate to next user message" },
    Definition { name: "messages_previous_user", default: DefaultBinding::Str("none"), description: "Navigate to previous user message" },
    Definition { name: "messages_last_user", default: DefaultBinding::Str("alt+end"), description: "Navigate to last user message" },
    Definition { name: "messages_copy", default: DefaultBinding::Str("<leader>y"), description: "Copy message" },
    Definition { name: "messages_undo", default: DefaultBinding::Str("<leader>u"), description: "Undo message" },
    Definition { name: "messages_redo", default: DefaultBinding::Str("<leader>r"), description: "Redo message" },
    Definition { name: "display_thinking", default: DefaultBinding::Str("none"), description: "Toggle thinking blocks visibility" },
    Definition { name: "prompt_submit", default: DefaultBinding::Str("none"), description: "Submit prompt" },
    Definition { name: "prompt_queue", default: DefaultBinding::Str("<leader>return"), description: "Queue prompt" },
    Definition { name: "prompt_editor_context_clear", default: DefaultBinding::Str("none"), description: "Clear editor context" },
    Definition { name: "prompt_images_view", default: DefaultBinding::Str("<leader>i"), description: "View image attachments" },
    Definition { name: "prompt_skills", default: DefaultBinding::Str("none"), description: "Open skill selector" },
    Definition { name: "prompt_stash", default: DefaultBinding::Str("none"), description: "Stash prompt" },
    Definition { name: "prompt_stash_pop", default: DefaultBinding::Str("none"), description: "Pop stashed prompt" },
    Definition { name: "prompt_stash_list", default: DefaultBinding::Str("none"), description: "List stashed prompts" },
    Definition { name: "input_clear", default: DefaultBinding::Str("ctrl+c"), description: "Clear input field" },
    Definition { name: "input_paste", default: DefaultBinding::Json(r#"{"key":"ctrl+v","preventDefault":false}"#), description: "Paste from clipboard" },
    Definition { name: "input_submit", default: DefaultBinding::Str("return"), description: "Submit input" },
    Definition { name: "input_newline", default: DefaultBinding::Str("shift+return,ctrl+return,alt+return,ctrl+j"), description: "Insert newline in input" },
    Definition { name: "input_move_left", default: DefaultBinding::Str("left,ctrl+b"), description: "Move cursor left in input" },
    Definition { name: "input_move_right", default: DefaultBinding::Str("right,ctrl+f"), description: "Move cursor right in input" },
    Definition { name: "input_move_up", default: DefaultBinding::Str("up"), description: "Move cursor up in input" },
    Definition { name: "input_move_down", default: DefaultBinding::Str("down"), description: "Move cursor down in input" },
    Definition { name: "input_select_left", default: DefaultBinding::Str("shift+left"), description: "Select left in input" },
    Definition { name: "input_select_right", default: DefaultBinding::Str("shift+right"), description: "Select right in input" },
    Definition { name: "input_select_up", default: DefaultBinding::Str("shift+up"), description: "Select up in input" },
    Definition { name: "input_select_down", default: DefaultBinding::Str("shift+down"), description: "Select down in input" },
    Definition { name: "input_line_home", default: DefaultBinding::Str("ctrl+a"), description: "Move to start of line in input" },
    Definition { name: "input_line_end", default: DefaultBinding::Str("ctrl+e"), description: "Move to end of line in input" },
    Definition { name: "input_select_line_home", default: DefaultBinding::Str("ctrl+shift+a"), description: "Select to start of line in input" },
    Definition { name: "input_select_line_end", default: DefaultBinding::Str("ctrl+shift+e"), description: "Select to end of line in input" },
    Definition { name: "input_visual_line_home", default: DefaultBinding::Str("alt+a"), description: "Move to start of visual line in input" },
    Definition { name: "input_visual_line_end", default: DefaultBinding::Str("alt+e"), description: "Move to end of visual line in input" },
    Definition { name: "input_select_visual_line_home", default: DefaultBinding::Str("alt+shift+a"), description: "Select to start of visual line in input" },
    Definition { name: "input_select_visual_line_end", default: DefaultBinding::Str("alt+shift+e"), description: "Select to end of visual line in input" },
    Definition { name: "input_buffer_home", default: DefaultBinding::Str("none"), description: "Move to start of buffer in input" },
    Definition { name: "input_buffer_end", default: DefaultBinding::Str("none"), description: "Move to end of buffer in input" },
    Definition { name: "input_select_buffer_home", default: DefaultBinding::Str("shift+home"), description: "Select to start of buffer in input" },
    Definition { name: "input_select_buffer_end", default: DefaultBinding::Str("shift+end"), description: "Select to end of buffer in input" },
    Definition { name: "input_delete_line", default: DefaultBinding::Str("ctrl+shift+d"), description: "Delete line in input" },
    Definition { name: "input_delete_to_line_end", default: DefaultBinding::Str("ctrl+k"), description: "Delete to end of line in input" },
    Definition { name: "input_delete_to_line_start", default: DefaultBinding::Str("ctrl+u"), description: "Delete to start of line in input" },
    Definition { name: "input_backspace", default: DefaultBinding::Str("backspace,shift+backspace"), description: "Backspace in input" },
    Definition { name: "input_delete", default: DefaultBinding::Str("ctrl+d,delete,shift+delete"), description: "Delete character in input" },
    Definition { name: "input_undo", default: DefaultBinding::Str("ctrl+-,super+z"), description: "Undo in input" },
    Definition { name: "input_redo", default: DefaultBinding::Str("ctrl+.,super+shift+z"), description: "Redo in input" },
    Definition { name: "input_word_forward", default: DefaultBinding::Str("alt+f,alt+right,ctrl+right"), description: "Move word forward in input" },
    Definition { name: "input_word_backward", default: DefaultBinding::Str("alt+b,alt+left,ctrl+left"), description: "Move word backward in input" },
    Definition { name: "input_select_word_forward", default: DefaultBinding::Str("alt+shift+f,alt+shift+right"), description: "Select word forward in input" },
    Definition { name: "input_select_word_backward", default: DefaultBinding::Str("alt+shift+b,alt+shift+left"), description: "Select word backward in input" },
    Definition { name: "input_delete_word_forward", default: DefaultBinding::Str("alt+d,alt+delete,ctrl+delete"), description: "Delete word forward in input" },
    Definition { name: "input_delete_word_backward", default: DefaultBinding::Str("ctrl+w,ctrl+backspace,alt+backspace"), description: "Delete word backward in input" },
    Definition { name: "input_select_all", default: DefaultBinding::Str("super+a"), description: "Select all in input" },
    Definition { name: "history_previous", default: DefaultBinding::Str("up"), description: "Previous history item" },
    Definition { name: "history_next", default: DefaultBinding::Str("down"), description: "Next history item" },
    Definition { name: "dialog.select.prev", default: DefaultBinding::Str("up,ctrl+p"), description: "Move to previous dialog item" },
    Definition { name: "dialog.select.next", default: DefaultBinding::Str("down,ctrl+n"), description: "Move to next dialog item" },
    Definition { name: "dialog.select.page_up", default: DefaultBinding::Str("pageup"), description: "Move up one page in dialog" },
    Definition { name: "dialog.select.page_down", default: DefaultBinding::Str("pagedown"), description: "Move down one page in dialog" },
    Definition { name: "dialog.select.home", default: DefaultBinding::Str("home"), description: "Move to first dialog item" },
    Definition { name: "dialog.select.end", default: DefaultBinding::Str("end"), description: "Move to last dialog item" },
    Definition { name: "dialog.select.submit", default: DefaultBinding::Str("return"), description: "Submit selected dialog item" },
    Definition { name: "dialog.prompt.submit", default: DefaultBinding::Str("return"), description: "Submit dialog prompt" },
    Definition { name: "dialog.project_copy.generate", default: DefaultBinding::Str("tab"), description: "Generate project copy name" },
    Definition { name: "dialog.move_session.new", default: DefaultBinding::Str("ctrl+m"), description: "New project copy" },
    Definition { name: "dialog.move_session.delete", default: DefaultBinding::Str("ctrl+d"), description: "Delete project copy" },
    Definition { name: "dialog.move_session.refresh", default: DefaultBinding::Str("ctrl+r"), description: "Refresh project copies" },
    Definition { name: "prompt.autocomplete.prev", default: DefaultBinding::Str("up,ctrl+p"), description: "Move to previous autocomplete item" },
    Definition { name: "prompt.autocomplete.next", default: DefaultBinding::Str("down,ctrl+n"), description: "Move to next autocomplete item" },
    Definition { name: "prompt.autocomplete.hide", default: DefaultBinding::Str("escape"), description: "Hide autocomplete" },
    Definition { name: "prompt.autocomplete.select", default: DefaultBinding::Str("return"), description: "Select autocomplete item" },
    Definition { name: "prompt.autocomplete.complete", default: DefaultBinding::Str("tab"), description: "Complete autocomplete item" },
    Definition { name: "permission.prompt.fullscreen", default: DefaultBinding::Str("ctrl+f"), description: "Toggle permission prompt fullscreen" },
    Definition { name: "plugins.toggle", default: DefaultBinding::Str("return"), description: "Toggle plugin" },
    Definition { name: "dialog.mcp.toggle", default: DefaultBinding::Str("space"), description: "Toggle MCP server" },
    Definition { name: "terminal_suspend", default: DefaultBinding::Str("ctrl+z"), description: "Suspend terminal" },
    Definition { name: "terminal_title_toggle", default: DefaultBinding::Str("none"), description: "Toggle terminal title" },
];

/// v1 `TuiKeybind.CommandMap` (v1 name -> v2 command), verbatim and in source order.
pub static COMMAND_MAP: &[(&str, &str)] = &[
    ("app_exit", "app.exit"),
    ("app_debug", "app.debug"),
    ("app_console", "app.console"),
    ("app_heap_snapshot", "app.heap_snapshot"),
    ("app_toggle_animations", "app.toggle.animations"),
    ("app_toggle_file_context", "app.toggle.file_context"),
    ("app_toggle_diffwrap", "app.toggle.diffwrap"),
    ("app_toggle_paste_summary", "app.toggle.paste_summary"),
    ("command_list", "command.palette.show"),
    ("help_show", "help.show"),
    ("docs_open", "docs.open"),
    ("diff_open", "diff.open"),
    ("diff_close", "diff.close"),
    ("diff_toggle", "diff.toggle"),
    ("diff_expand", "diff.expand"),
    ("diff_expand_all", "diff.expand_all"),
    ("diff_collapse", "diff.collapse"),
    ("diff_switch_focus", "diff.switch_focus"),
    ("diff_next_hunk", "diff.next_hunk"),
    ("diff_previous_hunk", "diff.previous_hunk"),
    ("diff_next_file", "diff.next_file"),
    ("diff_previous_file", "diff.previous_file"),
    ("diff_toggle_file_tree", "diff.toggle_file_tree"),
    ("diff_single_patch", "diff.single_patch"),
    ("diff_switch_source", "diff.switch_source"),
    ("diff_toggle_view", "diff.toggle_view"),
    ("diff_help", "diff.help"),
    ("editor_open", "prompt.editor"),
    ("theme_list", "theme.switch"),
    ("theme_switch_mode", "theme.switch_mode"),
    ("theme_mode_lock", "theme.mode.lock"),
    ("sidebar_toggle", "session.sidebar.toggle"),
    ("scrollbar_toggle", "session.toggle.scrollbar"),
    ("status_view", "opencode.status"),
    ("debug_view", "opencode.debug"),
    ("session_export", "session.export"),
    ("session_copy", "session.copy"),
    ("session_move", "session.move"),
    ("session_new", "session.new"),
    ("session_list", "session.list"),
    ("open_menu", "open.menu"),
    ("session_tab_next", "session.tab.next"),
    ("session_tab_previous", "session.tab.previous"),
    ("session_tab_next_unread", "session.tab.next_unread"),
    ("session_tab_previous_unread", "session.tab.previous_unread"),
    ("session_tab_close", "session.tab.close"),
    ("session_tab_reopen", "session.tab.reopen"),
    ("session_timeline", "session.timeline"),
    ("session_fork", "session.fork"),
    ("session_rename", "session.rename"),
    ("session_delete", "session.delete"),
    ("session_share", "session.share"),
    ("session_unshare", "session.unshare"),
    ("session_interrupt", "session.interrupt"),
    ("session_background", "session.background"),
    ("session_compact", "session.compact"),
    ("session_queued_prompts", "session.queued_prompts"),
    ("queued_prompt_delete", "queued_prompt.delete"),
    ("session_child_first", "session.child.first"),
    ("session_parent", "session.parent"),
    ("session_pin_toggle", "session.pin.toggle"),
    ("session_quick_switch_1", "session.quick_switch.1"),
    ("session_quick_switch_2", "session.quick_switch.2"),
    ("session_quick_switch_3", "session.quick_switch.3"),
    ("session_quick_switch_4", "session.quick_switch.4"),
    ("session_quick_switch_5", "session.quick_switch.5"),
    ("session_quick_switch_6", "session.quick_switch.6"),
    ("session_quick_switch_7", "session.quick_switch.7"),
    ("session_quick_switch_8", "session.quick_switch.8"),
    ("session_quick_switch_9", "session.quick_switch.9"),
    ("session_tab_select_1", "session.tab.select.1"),
    ("session_tab_select_2", "session.tab.select.2"),
    ("session_tab_select_3", "session.tab.select.3"),
    ("session_tab_select_4", "session.tab.select.4"),
    ("session_tab_select_5", "session.tab.select.5"),
    ("session_tab_select_6", "session.tab.select.6"),
    ("session_tab_select_7", "session.tab.select.7"),
    ("session_tab_select_8", "session.tab.select.8"),
    ("session_tab_select_9", "session.tab.select.9"),
    ("session_tab_select_10", "session.tab.select.10"),
    ("stash_delete", "stash.delete"),
    ("model_provider_list", "model.dialog.provider"),
    ("model_favorite_toggle", "model.dialog.favorite"),
    ("model_list", "model.list"),
    ("model_cycle_recent", "model.cycle_recent"),
    ("model_cycle_recent_reverse", "model.cycle_recent_reverse"),
    ("model_cycle_favorite", "model.cycle_favorite"),
    ("model_cycle_favorite_reverse", "model.cycle_favorite_reverse"),
    ("mcp_list", "mcp.list"),
    ("provider_connect", "provider.connect"),
    ("agent_list", "agent.list"),
    ("agent_cycle", "agent.cycle"),
    ("agent_cycle_reverse", "agent.cycle.reverse"),
    ("variant_cycle", "variant.cycle"),
    ("variant_list", "variant.list"),
    ("messages_page_up", "session.page.up"),
    ("messages_page_down", "session.page.down"),
    ("messages_line_up", "session.line.up"),
    ("messages_line_down", "session.line.down"),
    ("messages_half_page_up", "session.half.page.up"),
    ("messages_half_page_down", "session.half.page.down"),
    ("messages_first", "session.first"),
    ("messages_last", "session.last"),
    ("messages_next", "session.message.next"),
    ("messages_previous", "session.message.previous"),
    ("messages_next_user", "session.message.user.next"),
    ("messages_previous_user", "session.message.user.previous"),
    ("messages_last_user", "session.messages_last_user"),
    ("messages_copy", "messages.copy"),
    ("messages_undo", "session.undo"),
    ("messages_redo", "session.redo"),
    ("display_thinking", "session.toggle.thinking"),
    ("prompt_submit", "prompt.submit"),
    ("prompt_queue", "prompt.queue"),
    ("prompt_editor_context_clear", "prompt.editor_context.clear"),
    ("prompt_images_view", "prompt.images.view"),
    ("prompt_skills", "prompt.skills"),
    ("prompt_stash", "prompt.stash"),
    ("prompt_stash_pop", "prompt.stash.pop"),
    ("prompt_stash_list", "prompt.stash.list"),
    ("input_clear", "prompt.clear"),
    ("input_paste", "prompt.paste"),
    ("input_submit", "input.submit"),
    ("input_newline", "input.newline"),
    ("input_move_left", "input.move.left"),
    ("input_move_right", "input.move.right"),
    ("input_move_up", "input.move.up"),
    ("input_move_down", "input.move.down"),
    ("input_select_left", "input.select.left"),
    ("input_select_right", "input.select.right"),
    ("input_select_up", "input.select.up"),
    ("input_select_down", "input.select.down"),
    ("input_line_home", "input.line.home"),
    ("input_line_end", "input.line.end"),
    ("input_select_line_home", "input.select.line.home"),
    ("input_select_line_end", "input.select.line.end"),
    ("input_visual_line_home", "input.visual.line.home"),
    ("input_visual_line_end", "input.visual.line.end"),
    ("input_select_visual_line_home", "input.select.visual.line.home"),
    ("input_select_visual_line_end", "input.select.visual.line.end"),
    ("input_buffer_home", "input.buffer.home"),
    ("input_buffer_end", "input.buffer.end"),
    ("input_select_buffer_home", "input.select.buffer.home"),
    ("input_select_buffer_end", "input.select.buffer.end"),
    ("input_delete_line", "input.delete.line"),
    ("input_delete_to_line_end", "input.delete.to.line.end"),
    ("input_delete_to_line_start", "input.delete.to.line.start"),
    ("input_backspace", "input.backspace"),
    ("input_delete", "input.delete"),
    ("input_undo", "input.undo"),
    ("input_redo", "input.redo"),
    ("input_word_forward", "input.word.forward"),
    ("input_word_backward", "input.word.backward"),
    ("input_select_word_forward", "input.select.word.forward"),
    ("input_select_word_backward", "input.select.word.backward"),
    ("input_delete_word_forward", "input.delete.word.forward"),
    ("input_delete_word_backward", "input.delete.word.backward"),
    ("input_select_all", "input.select.all"),
    ("history_previous", "prompt.history.previous"),
    ("history_next", "prompt.history.next"),
    ("terminal_suspend", "terminal.suspend"),
    ("terminal_title_toggle", "terminal.title.toggle"),
];

/// `TuiKeybind.CommandMap[name]`.
pub fn command_map(name: &str) -> Option<&'static str> {
    COMMAND_MAP.iter().find(|(from, _)| *from == name).map(|(_, to)| *to)
}

/// `new Set(Object.values(TuiKeybind.CommandMap))`, in first-appearance order.
pub fn legacy_targets() -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    for (_, to) in COMMAND_MAP {
        if !out.contains(to) {
            out.push(to);
        }
    }
    out
}

/// `name in TuiKeybind.CommandMap values`.
pub fn is_legacy_target(name: &str) -> bool {
    COMMAND_MAP.iter().any(|(_, to)| *to == name)
}

/// Decode v1 `KeybindOverrides` (v1 definition names; unknown keys stripped).
pub fn decode_v1_overrides(input: &Value) -> Option<IndexMap<String, Value>> {
    super::keybind::decode_overrides_in(V1_DEFINITIONS, input)
}

/// v1 `PluginSpec = string | [string, Record<string, unknown>]`.
#[derive(Debug, Clone, PartialEq)]
pub enum PluginSpec {
    Name(String),
    Tuple(String, Map<String, Value>),
}

/// `ScrollAcceleration = { enabled: boolean }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollAcceleration {
    pub enabled: bool,
}

/// Decoded `TuiConfigV1.Info`. Object-valued fields keep the decoded shape (schema field order).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TuiConfigV1 {
    pub schema: Option<String>,
    pub theme: Option<String>,
    /// v1 keybind overrides keyed by v1 names, in v1 definition order.
    pub keybinds: Option<IndexMap<String, Value>>,
    pub plugin: Option<Vec<PluginSpec>>,
    pub plugin_enabled: Option<IndexMap<String, bool>>,
    /// Positive safe integer.
    pub leader_timeout: Option<Number>,
    /// `{ enabled?, notifications?, sound?, volume? (0..1), sound_pack?, sounds? }`.
    pub attention: Option<IndexMap<String, Value>>,
    /// `{ max_height?: PositiveInt, max_width?: PositiveInt | "auto" }`.
    pub prompt: Option<IndexMap<String, Value>>,
    /// Number >= 0.001.
    pub scroll_speed: Option<Number>,
    pub scroll_acceleration: Option<ScrollAcceleration>,
    /// `"auto" | "stacked"`.
    pub diff_style: Option<String>,
    pub mouse: Option<bool>,
    /// `{ style?: "block"|"underline"|"line"|"default", blinking?: boolean }`.
    pub cursor: Option<Value>,
}

/// `TuiConfigV1.AttentionSoundName` literals (record key order).
pub const ATTENTION_SOUND_NAMES: [&str; 6] = ["default", "question", "permission", "error", "done", "subagent_done"];

/// Decode `TuiConfigV1.Info`. Returns `None` when the input is not an object or any declared
/// field is invalid.
pub fn decode_tui_v1(input: &Value) -> Option<TuiConfigV1> {
    let o = input.as_object()?;
    let mut out = TuiConfigV1::default();
    if let Some(v) = o.get("$schema") {
        out.schema = Some(v.as_str()?.to_string());
    }
    if let Some(v) = o.get("theme") {
        out.theme = Some(v.as_str()?.to_string());
    }
    if let Some(v) = o.get("keybinds") {
        out.keybinds = Some(decode_v1_overrides(v)?);
    }
    if let Some(v) = o.get("plugin") {
        out.plugin = Some(v.as_array()?.iter().map(decode_plugin_spec).collect::<Option<Vec<_>>>()?);
    }
    if let Some(v) = o.get("plugin_enabled") {
        let mut map = IndexMap::new();
        for (k, b) in v.as_object()? {
            map.insert(k.clone(), b.as_bool()?);
        }
        out.plugin_enabled = Some(map);
    }
    if let Some(v) = o.get("leader_timeout") {
        out.leader_timeout = Some(positive_int(v)?);
    }
    if let Some(v) = o.get("attention") {
        out.attention = Some(decode_attention(v)?);
    }
    if let Some(v) = o.get("prompt") {
        out.prompt = Some(decode_prompt(v)?);
    }
    if let Some(v) = o.get("scroll_speed") {
        let n = number(v)?;
        if n.as_f64()? < 0.001 {
            return None;
        }
        out.scroll_speed = Some(n);
    }
    if let Some(v) = o.get("scroll_acceleration") {
        let enabled = v.as_object()?.get("enabled")?.as_bool()?;
        out.scroll_acceleration = Some(ScrollAcceleration { enabled });
    }
    if let Some(v) = o.get("diff_style") {
        match v.as_str() {
            Some(s @ ("auto" | "stacked")) => out.diff_style = Some(s.to_string()),
            _ => return None,
        }
    }
    if let Some(v) = o.get("mouse") {
        out.mouse = Some(v.as_bool()?);
    }
    if let Some(v) = o.get("cursor") {
        out.cursor = Some(decode_cursor(v)?);
    }
    Some(out)
}

fn decode_plugin_spec(value: &Value) -> Option<PluginSpec> {
    match value {
        Value::String(s) => Some(PluginSpec::Name(s.clone())),
        Value::Array(items) if items.len() == 2 => {
            let name = items[0].as_str()?.to_string();
            let options = items[1].as_object()?.clone();
            Some(PluginSpec::Tuple(name, options))
        }
        _ => None,
    }
}

/// JSON number with integral floats normalized to integers (JavaScript prints `2000.0` as `2000`).
fn number(value: &Value) -> Option<Number> {
    let n = match value {
        Value::Number(n) => n.clone(),
        _ => return None,
    };
    if n.is_f64() {
        let f = n.as_f64()?;
        if f.fract() == 0.0 && f.abs() <= 9_007_199_254_740_991.0 {
            return Some(Number::from(f as i64));
        }
    }
    Some(n)
}

/// `Schema.Int.check(isGreaterThan(0))` (safe integer).
fn positive_int(value: &Value) -> Option<Number> {
    let n = number(value)?;
    let f = n.as_f64()?;
    if f.fract() != 0.0 || f.abs() > 9_007_199_254_740_991.0 || f <= 0.0 {
        return None;
    }
    Some(n)
}

fn decode_attention(value: &Value) -> Option<IndexMap<String, Value>> {
    let o = value.as_object()?;
    let mut out = IndexMap::new();
    for key in ["enabled", "notifications", "sound"] {
        if let Some(v) = o.get(key) {
            out.insert(key.to_string(), Value::Bool(v.as_bool()?));
        }
    }
    if let Some(v) = o.get("volume") {
        let n = number(v)?;
        let f = n.as_f64()?;
        if !(0.0..=1.0).contains(&f) {
            return None;
        }
        out.insert("volume".into(), Value::Number(n));
    }
    if let Some(v) = o.get("sound_pack") {
        out.insert("sound_pack".into(), Value::String(v.as_str()?.to_string()));
    }
    if let Some(v) = o.get("sounds") {
        out.insert("sounds".into(), decode_sounds(v)?);
    }
    Some(out)
}

/// `Record(AttentionSoundName, optionalKey(String))`: literal keys in literal order, others stripped.
pub fn decode_sounds(value: &Value) -> Option<Value> {
    let o = value.as_object()?;
    let mut out = Map::new();
    for name in ATTENTION_SOUND_NAMES {
        if let Some(v) = o.get(name) {
            out.insert(name.into(), Value::String(v.as_str()?.to_string()));
        }
    }
    Some(Value::Object(out))
}

fn decode_prompt(value: &Value) -> Option<IndexMap<String, Value>> {
    let o = value.as_object()?;
    let mut out = IndexMap::new();
    if let Some(v) = o.get("max_height") {
        out.insert("max_height".into(), Value::Number(positive_int(v)?));
    }
    if let Some(v) = o.get("max_width") {
        let decoded = if v.as_str() == Some("auto") { v.clone() } else { Value::Number(positive_int(v)?) };
        out.insert("max_width".into(), decoded);
    }
    Some(out)
}

/// `Cursor = { style?: "block"|"underline"|"line"|"default", blinking?: boolean }`.
pub fn decode_cursor(value: &Value) -> Option<Value> {
    let o = value.as_object()?;
    let mut out = Map::new();
    if let Some(v) = o.get("style") {
        match v.as_str() {
            Some("block" | "underline" | "line" | "default") => {
                out.insert("style".into(), v.clone());
            }
            _ => return None,
        }
    }
    if let Some(v) = o.get("blinking") {
        out.insert("blinking".into(), Value::Bool(v.as_bool()?));
    }
    Some(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tables() {
        assert_eq!(V1_DEFINITIONS.len(), 183);
        assert_eq!(COMMAND_MAP.len(), 162);
        assert_eq!(command_map("app_exit"), Some("app.exit"));
        assert_eq!(command_map("command_list"), Some("command.palette.show"));
        assert_eq!(command_map("editor_open"), Some("prompt.editor"));
        assert_eq!(command_map("theme_list"), Some("theme.switch"));
        assert_eq!(command_map("sidebar_toggle"), Some("session.sidebar.toggle"));
        assert_eq!(command_map("status_view"), Some("opencode.status"));
        assert_eq!(command_map("messages_page_up"), Some("session.page.up"));
        assert_eq!(command_map("messages_undo"), Some("session.undo"));
        assert_eq!(command_map("display_thinking"), Some("session.toggle.thinking"));
        assert_eq!(command_map("input_clear"), Some("prompt.clear"));
        assert_eq!(command_map("input_paste"), Some("prompt.paste"));
        assert_eq!(command_map("history_previous"), Some("prompt.history.previous"));
        assert_eq!(command_map("model_provider_list"), Some("model.dialog.provider"));
        assert_eq!(command_map("model_favorite_toggle"), Some("model.dialog.favorite"));
        assert_eq!(command_map("app_heap_snapshot"), Some("app.heap_snapshot"));
        assert!(!super::super::keybind::is_definition("app.heap_snapshot"));
        assert_eq!(command_map("leader"), None);
        // `leader` stays `leader` via the Definitions fallback in migration.
        assert!(super::super::keybind::is_definition("leader"));
        assert!(is_legacy_target("app.heap_snapshot"));
        assert_eq!(legacy_targets().len(), 162);
        // every mapped target except app.heap_snapshot is a v2 definition
        let missing: Vec<_> = COMMAND_MAP
            .iter()
            .filter(|(_, to)| !super::super::keybind::is_definition(to))
            .map(|(_, to)| *to)
            .collect();
        assert_eq!(missing, vec!["app.heap_snapshot"]);
    }

    #[test]
    fn decodes_full_v1_config() {
        let decoded = decode_tui_v1(&json!({
            "$schema": "https://opencode.ai/tui.json",
            "theme": "tokyonight",
            "keybinds": {"app_exit": "ctrl+q", "unknown_thing": "x", "leader": "ctrl+a"},
            "plugin": ["a", ["b", {"x": 1}]],
            "plugin_enabled": {"a": true, "c": false},
            "leader_timeout": 1500,
            "attention": {"sound": false, "enabled": true, "sounds": {"error": "/e.wav", "zzz": "q"}},
            "prompt": {"max_width": "auto"},
            "scroll_speed": 2.5,
            "scroll_acceleration": {"enabled": true},
            "diff_style": "stacked",
            "mouse": false,
            "cursor": {"blinking": false, "style": "line", "x": 1},
            "extra": {"ignored": true}
        }))
        .unwrap();
        assert_eq!(decoded.theme.as_deref(), Some("tokyonight"));
        let keybinds = decoded.keybinds.unwrap();
        assert_eq!(keybinds.keys().collect::<Vec<_>>(), vec!["leader", "app_exit"]);
        assert_eq!(
            decoded.plugin.unwrap(),
            vec![PluginSpec::Name("a".into()), PluginSpec::Tuple("b".into(), json!({"x": 1}).as_object().unwrap().clone())]
        );
        assert_eq!(decoded.plugin_enabled.unwrap().get("c"), Some(&false));
        assert_eq!(decoded.leader_timeout, Some(Number::from(1500)));
        let attention = decoded.attention.unwrap();
        assert_eq!(attention.keys().collect::<Vec<_>>(), vec!["enabled", "sound", "sounds"]);
        assert_eq!(attention["sounds"], json!({"error": "/e.wav"}));
        assert_eq!(decoded.scroll_acceleration, Some(ScrollAcceleration { enabled: true }));
        assert_eq!(decoded.diff_style.as_deref(), Some("stacked"));
        assert_eq!(decoded.mouse, Some(false));
        assert_eq!(decoded.cursor, Some(json!({"style": "line", "blinking": false})));
    }

    #[test]
    fn rejects_invalid_declared_fields() {
        assert!(decode_tui_v1(&json!([])).is_none());
        assert!(decode_tui_v1(&json!({"theme": 1})).is_none());
        assert!(decode_tui_v1(&json!({"leader_timeout": 0})).is_none());
        assert!(decode_tui_v1(&json!({"leader_timeout": 1.5})).is_none());
        assert!(decode_tui_v1(&json!({"scroll_speed": 0})).is_none());
        assert!(decode_tui_v1(&json!({"attention": {"volume": 2}})).is_none());
        assert!(decode_tui_v1(&json!({"attention": {"sounds": {"error": 1}}})).is_none());
        assert!(decode_tui_v1(&json!({"diff_style": "split"})).is_none());
        assert!(decode_tui_v1(&json!({"plugin": [["a"]]})).is_none());
        assert!(decode_tui_v1(&json!({"plugin": [["a", {}, 1]]})).is_none());
        assert!(decode_tui_v1(&json!({"scroll_acceleration": {}})).is_none());
        assert!(decode_tui_v1(&json!({"keybinds": {"app_exit": true}})).is_none());
        assert!(decode_tui_v1(&json!({"cursor": {"style": "bar"}})).is_none());
        assert!(decode_tui_v1(&json!({"prompt": {"max_width": "wide"}})).is_none());
        assert!(decode_tui_v1(&json!({"mouse": null})).is_none());
        // unknown fields are ignored
        assert!(decode_tui_v1(&json!({"whatever": null})).is_some());
    }

    #[test]
    fn integral_floats_normalize() {
        let decoded = decode_tui_v1(&json!({"leader_timeout": 2000.0, "scroll_speed": 1.0})).unwrap();
        assert_eq!(serde_json::to_string(&decoded.leader_timeout).unwrap(), "2000");
        assert_eq!(serde_json::to_string(&decoded.scroll_speed).unwrap(), "1");
    }
}
