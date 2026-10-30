//! The diff side panel and the compact diff layout, checked against upstream 2.1.4 captures.

use super::tests::{app, render, session};
use super::*;
use crate::api::types::FileDiff;
use crate::keymap::Stroke;

const APP_PATCH: &str = "@@ -1,9 +1,10 @@\n export function add(a: number, b: number) {\n+  // sum\n   return a + b\n }\n \n export function sub(a: number, b: number) {\n-  return a - b\n+  return b - a\n }\n \n-export const name = \"app\"\n+export const name = \"app2\"\n";
const NEW_PATCH: &str = "@@ -0,0 +1 @@\n+export const created = true\n";

fn files() -> Vec<FileDiff> {
    vec![
        FileDiff { file: "src/new.ts".into(), patch: NEW_PATCH.into(), additions: 1, deletions: 0, status: Some("added".into()) },
        FileDiff { file: "src/app.ts".into(), patch: APP_PATCH.into(), additions: 3, deletions: 2, status: Some("modified".into()) },
    ]
}

fn loaded(d: &mut diffview::DiffViewer) {
    d.files = files();
    d.loading = false;
    d.base = Some("main".into());
    d.base_loaded = true;
}

/// A session with the diff panel open and loaded (as `/diff` does, without the server).
fn with_panel(a: &mut App) -> String {
    let sid = session(a);
    a.run_command("diff.open", None);
    loaded(a.diff.as_mut().unwrap());
    sid
}

fn col(line: &str, needle: &str) -> usize {
    let byte = line.find(needle).unwrap_or_else(|| panic!("{needle:?} not in {line:?}"));
    crate::util::locale::width(&line[..byte])
}

#[test]
fn diff_opens_docked_beside_the_transcript_and_toggles_closed() {
    let mut a = app();
    let sid = with_panel(&mut a);
    assert_eq!(a.route, Route::Session(sid.clone()));
    assert!(a.active_panel().is_some() && a.right_focus);
    let lines = render(&mut a, 140, 40);
    let joined = lines.join("\n");
    // The pane is half of 140 columns: x = 70, the header one row down at the panel inset of 2.
    assert_eq!(col(&lines[1], "All · vs main"), 72, "{joined}");
    assert_eq!(col(&lines[1], "0/2") + 3, 139, "{joined}");
    // Cards are flush; files follow the tree (app.ts before new.ts).
    assert_eq!(col(&lines[3], "src/app.ts"), 72, "{joined}");
    assert_eq!(col(&lines[3], "+3 -2") + 5, 139, "{joined}");
    // Gutter padding 1 + 1 cell + 2 digits: line 1 at x = 73, code at x = 77.
    assert_eq!(col(&lines[5], "1   export"), 73, "{joined}");
    assert_eq!(col(&lines[6], "2 +   // sum"), 73, "{joined}");
    // The footer: a ▀ row on the base background, the hints, then a blank row.
    let n = lines.len();
    assert!(lines[n - 3][lines[n - 3].char_indices().nth(70).unwrap().0..].starts_with("▀▀▀"), "{joined}");
    assert_eq!(col(&lines[n - 2], "ctrl+x ← focus prompt"), 72, "{joined}");
    assert!(lines[n - 2].ends_with("? see all"), "{joined}");
    assert_eq!(crate::util::locale::width(&lines[n - 2]), 139);
    // Unified: the pane is narrower than the split threshold.
    assert!(!contains(&lines, "1    export"), "{joined}");
    // /diff again closes it; the transcript gets the whole width back.
    a.run_command("diff.open", None);
    assert!(a.panel.is_none() && a.diff.is_none() && !a.right_focus);
}

fn contains(lines: &[String], needle: &str) -> bool {
    lines.iter().any(|l| l.contains(needle))
}

#[test]
fn focus_moves_between_prompt_and_panel() {
    let mut a = app();
    with_panel(&mut a);
    let _ = render(&mut a, 140, 40);
    // Diff keys reach the focused panel; typing does not reach the prompt.
    a.on_key(Stroke::key("x"));
    assert!(a.editor.is_empty());
    // <leader>left focuses the prompt and dims the panel footer.
    a.on_key(Stroke::ctrl("x"));
    a.on_key(Stroke::key("left"));
    assert!(!a.right_focus);
    a.on_key(Stroke::key("x"));
    assert_eq!(a.editor.text, "x");
    let buf = super::tests::render_buf(&mut a, 140, 40);
    let y = 38;
    let footer: String = (70..140).map(|x| buf[(x, y)].symbol().to_string()).collect();
    assert!(footer.contains("ctrl+x ← focus prompt"), "{footer}");
    assert!(buf[(72, y)].modifier.contains(ratatui::style::Modifier::DIM));
    // While the panel has focus the prompt hints read as disabled.
    a.on_key(Stroke::ctrl("x"));
    a.on_key(Stroke::key("right"));
    assert!(a.right_focus);
    let lines = render(&mut a, 140, 40);
    let row = lines.iter().position(|l| l.contains("ctrl+p commands")).unwrap();
    let buf = super::tests::render_buf(&mut a, 140, 40);
    let x = col(&lines[row], "ctrl+p") as u16;
    assert!(buf[(x, row as u16)].modifier.contains(ratatui::style::Modifier::DIM));
}

#[test]
fn narrow_frames_take_the_panel_full_screen_and_f_toggles() {
    let mut a = app();
    with_panel(&mut a);
    // 100 columns cannot split (64 + 44): full screen with the full layout and its file tree.
    let lines = render(&mut a, 100, 30);
    let joined = lines.join("\n");
    assert!(a.panel_fullscreen());
    assert!(contains(&lines, "▾ src"), "{joined}");
    assert!(contains(&lines, "? help"), "{joined}");
    // `f` is ignored while the frame cannot split, so nothing is remembered.
    a.on_key(Stroke::key("f"));
    assert_eq!(a.config.bool("diffs.fullscreen"), None);
    // Wide: `f` toggles full screen and remembers it.
    let _ = render(&mut a, 140, 40);
    assert!(!a.panel_fullscreen());
    a.on_key(Stroke::key("f"));
    assert!(a.panel_fullscreen());
    assert_eq!(a.config.bool("diffs.fullscreen"), Some(true));
    let lines = render(&mut a, 140, 40);
    // Full screen hides the transcript and the help lists the toggle.
    assert!(!contains(&lines, "I fixed the bug."), "{}", lines.join("\n"));
    a.on_key(Stroke::key("?"));
    let lines = render(&mut a, 140, 40);
    assert!(contains(&lines, "Full screen / split view"), "{}", lines.join("\n"));
    a.close_dialog();
    a.on_key(Stroke::key("f"));
    assert!(!a.panel_fullscreen());
    assert_eq!(a.config.bool("diffs.fullscreen"), Some(false));
    // q closes the panel.
    a.on_key(Stroke::key("q"));
    assert!(a.panel.is_none());
}

#[test]
fn compact_full_screen_layout_below_88_columns() {
    let mut a = app();
    a.run_command("diff.open", None);
    assert_eq!(a.route, Route::Plugin("diff".into()));
    loaded(a.diff.as_mut().unwrap());
    let lines = render(&mut a, 80, 30);
    let joined = lines.join("\n");
    // No tree and no `?` corner: the header ends 1 cell in, cards are flush, a footer at the bottom.
    assert_eq!(lines[0], format!("  All · vs main{}0/2", " ".repeat(76 - 15)), "{joined}");
    assert_eq!(col(&lines[2], "src/app.ts"), 2, "{joined}");
    assert_eq!(col(&lines[2], "+3 -2") + 5, 79, "{joined}");
    assert_eq!(lines[4], "   1   export function add(a: number, b: number) {", "{joined}");
    assert_eq!(lines[5], "   2 +   // sum", "{joined}");
    assert_eq!(lines[15], "  10 + export const name = \"app2\"", "{joined}");
    assert_eq!(lines[16], "▄".repeat(80), "{joined}");
    assert_eq!(col(&lines[17], "src/new.ts"), 2, "{joined}");
    assert_eq!(lines[19], "  1 + export const created = true", "{joined}");
    assert_eq!(lines[29], format!("  escape close{}? see all", " ".repeat(70 - 14)), "{joined}");
    // The help drops the file tree row in the compact layout and has no full-screen toggle.
    a.on_key(Stroke::key("?"));
    let lines = render(&mut a, 80, 30);
    assert!(!contains(&lines, "Show / hide file tree") && !contains(&lines, "Full screen"), "{}", lines.join("\n"));
}

#[test]
fn full_layout_split_gutters_and_card_gaps() {
    let mut a = app();
    a.run_command("diff.open", None);
    loaded(a.diff.as_mut().unwrap());
    let lines = render(&mut a, 140, 40);
    let joined = lines.join("\n");
    // Tree of 35 columns, cards inset by 2: both sides size their numbers to the digits of the
    // furthest hunk range end (`@@ -1,9 +1,10 @@`: 2, OpenTUI 0.7 `minDigits`); the right side starts at x = 88.
    assert_eq!(col(&lines[3], "1   export"), 39, "{joined}");
    assert_eq!(col(&lines[9], "6 -   return a - b"), 39, "{joined}");
    assert_eq!(col(&lines[9], "7 +   return b - a"), 90, "{joined}");
    assert_eq!(col(&lines[12], "10 + export"), 89, "{joined}");
    // The card gap spans the insets and runs straight after the last patch row.
    assert_eq!(col(&lines[13], "▄"), 35, "{joined}");
    assert_eq!(col(&lines[14], "src/new.ts"), 38, "{joined}");
    assert_eq!(col(&lines[37], "? help"), 2, "{joined}");
}

#[test]
fn reviewing_the_last_file_in_single_patch_mode_stays_put() {
    let mut a = app();
    a.run_command("diff.open", None);
    loaded(a.diff.as_mut().unwrap());
    a.config.set("diffs.single", Some(serde_json::json!(true)));
    let _ = render(&mut a, 140, 40);
    // app.ts (index 1) is first in the tree, new.ts (0) last.
    a.on_key(Stroke::key("m"));
    assert_eq!(a.diff.as_ref().unwrap().selected, Some(0));
    a.on_key(Stroke::key("m"));
    let d = a.diff.as_ref().unwrap();
    assert!(d.reviewed.contains("src/new.ts"));
    assert_eq!(d.exhausted, Some(0));
    // Pressing it again does not reopen the exhausted file.
    a.on_key(Stroke::key("m"));
    assert!(a.diff.as_ref().unwrap().reviewed.contains("src/new.ts"));
    // The selected file uses the selected action color in the tree.
    let lines = render(&mut a, 140, 40);
    let row = lines.iter().position(|l| l.contains("≡ new.ts")).unwrap();
    let buf = super::tests::render_buf(&mut a, 140, 40);
    let x = col(&lines[row], "new.ts") as u16;
    assert_eq!(buf[(x, row as u16)].fg, a.theme().text.action.primary.selected.tui());
}

#[test]
fn another_session_or_a_terminal_closes_the_panel() {
    let mut a = app();
    with_panel(&mut a);
    a.navigate(Route::Home);
    assert!(a.panel.is_none());
}
