//! `ocd mini` (upstream `opencode mini`, `packages/tui/src/mini/` and `packages/cli/src/mini*.ts`): a
//! split-footer frontend on the main screen. Finished transcript entries are committed to the
//! terminal's native scrollback and only the footer (composer, statusline, panels, docks) repaints.
//!
//! Module map: `runtime` (lifecycle, queue, session target) drives `transport`/`subagents` (V2
//! events → commits), `footer` (state and view: `prompt`, `menu`, `panels`, `blocker`,
//! `subagent_view`), `scrollback` (entries: `markdown`, `marked`, `highlight`, `structured`, `text`,
//! `cell`) and `split` (the terminal renderer).

pub mod blocker;
pub mod catalog;
pub mod cell;
pub mod entry_body;
pub mod footer;
pub mod footer_width;
pub mod form_shared;
pub mod fuzzysort;
pub mod highlight;
pub mod markdown;
pub mod marked;
mod marked_rules;
pub mod menu;
pub mod mono;
pub mod motion;
pub mod panels;
pub mod permission_shared;
pub mod prompt;
pub mod prompt_shared;
pub mod runtime;
pub mod scrollback;
pub mod session_shared;
pub mod splash;
pub mod split;
pub mod structured;
pub mod subagent_view;
pub mod subagents;
pub mod text;
pub mod textarea;
pub mod theme;
pub mod tool;
pub mod transport;
pub mod types;
pub mod util;
pub mod variant;
pub mod verbosity;

pub use runtime::{MiniOptions, merge_input, run_mini};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge() {
        assert_eq!(merge_input(Some("from stdin"), None).as_deref(), Some("from stdin"));
        assert_eq!(merge_input(Some("from stdin"), Some("from flag")).as_deref(), Some("from stdin\nfrom flag"));
        assert_eq!(merge_input(None, Some("b")).as_deref(), Some("b"));
        assert_eq!(merge_input(Some(""), None), None);
    }
}
