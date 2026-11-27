//! The dialog kit and select list under the new designs, cell for cell against upstream 2.4.1's recorded frames
//! (`testdata/nd/U.3-*.json`, from `packages/cli-rs/parity/component/U.3-*.tsx`).

use super::dialog::*;
use super::fixture::{Fixture, assert_matches};
use super::text::fill;
use crate::keymap::{Keybinds, Stroke};
use crate::theme::{Mode, Theme, ThemeManager};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

fn theme() -> Theme {
    ThemeManager::new(None, Mode::Dark, None).current
}

fn kb() -> Keybinds {
    Keybinds::new(None, 2000, true)
}

/// The dialog alone on an empty screen, as the component harness renders it.
fn frame(d: &mut Dialog, w: u16, h: u16, nd: bool) -> Buffer {
    let t = theme();
    let area = Rect::new(0, 0, w, h);
    let mut buf = Buffer::empty(area);
    fill(&mut buf, area, t.background.base.tui());
    render(&mut buf, area, d, &t, &kb(), nd);
    buf
}

fn check(name: &str, d: &mut Dialog) {
    let fx = Fixture::parse(&std::fs::read_to_string(format!("{}/src/ui/testdata/nd/{name}.json", env!("CARGO_MANIFEST_DIR"))).unwrap());
    let buf = frame(d, fx.width, fx.height, true);
    assert_matches(name, &fx, &buf);
}

#[test]
fn alert() {
    let mut d = Dialog::new("alert", Body::Alert { title: "Heads up".into(), message: "The session was compacted.".into() });
    check("U.3-alert", &mut d);
}

#[test]
fn help() {
    let mut d = Dialog::new("help", Body::Help);
    check("U.3-help", &mut d);
}

/// `DialogConfirm` with `label: { confirm: "delete" }`, then ← to Cancel.
#[test]
fn confirm() {
    let body = || Body::Confirm {
        title: "Delete session".into(),
        message: "This permanently deletes the session.".into(),
        confirm: true,
        label: Some("Delete".into()),
    };
    let mut d = Dialog::new("confirm", body());
    let mut d2 = Dialog::new("confirm", body());
    check("U.3-confirm", &mut d);
    handle_key(&mut d2, &Stroke::key("left"), &kb());
    check("U.3-confirm-left", &mut d2);
}

#[test]
fn prompt() {
    let mut p = PromptDialog::new("Rename session", "", "My session");
    p.actions = vec![("generate".into(), "dialog.worktree.generate".into())];
    check("U.3-prompt", &mut Dialog::new("prompt", Body::Prompt(p)));
    let p = PromptDialog::new("Worktree name", "feature-branch", "");
    check("U.3-prompt-empty", &mut Dialog::new("prompt", Body::Prompt(p)));
}

fn models() -> Select {
    let opt = |title: &str, value: &str, cat: &str| Opt::new(title, value).cat(cat);
    let mut gpt5 = opt("GPT-5", "gpt5", "OpenAI");
    gpt5.details = vec!["/a/very/long/path/that/gets/cut/in/the/middle/model.json".into()];
    let mut codex = opt("Codex", "codex", "OpenAI");
    codex.search_text = Some("code".into());
    let mut off = opt("Disabled", "off", "Anthropic");
    off.disabled = true;
    let mut s = Select::new(
        "Select model",
        vec![
            opt("Claude Sonnet", "sonnet", "Anthropic").foot("Free"),
            opt("Claude Opus", "opus", "Anthropic").desc("best"),
            opt("Claude Haiku", "haiku", "Anthropic"),
            off,
            gpt5,
            opt("GPT-5 mini", "gpt5-mini", "OpenAI").foot("$0.25"),
            opt("o4", "o4", "OpenAI"),
            codex,
        ],
    );
    s.current = Some("haiku".into());
    s.actions = vec![Action::new("model.dialog.favorite", "favorite")];
    s
}

#[test]
fn select() {
    check("U.3-select", &mut Dialog::select("models", models(), Size::Medium));
    let mut d = Dialog::select("models", models(), Size::Medium);
    // Opened scrolled to the current option; the frame settles before the keys.
    frame(&mut d, 72, 30, true);
    handle_key(&mut d, &Stroke::key("down"), &kb());
    handle_key(&mut d, &Stroke::key("down"), &kb());
    check("U.3-select-down", &mut d);
    let mut d = Dialog::select("models", models(), Size::Medium);
    for c in ["g", "p", "t"] {
        handle_key(&mut d, &Stroke::key(c), &kb());
    }
    check("U.3-select-filter", &mut d);
    let mut d = Dialog::select("models", models(), Size::Medium);
    for c in ["z", "z", "z"] {
        handle_key(&mut d, &Stroke::key(c), &kb());
    }
    check("U.3-select-empty", &mut d);
}

/// The legacy layout is unchanged with the flag off: a bold title, `●` on the current option, no bar or band.
#[test]
fn legacy_layout_with_the_flag_off() {
    let mut d = Dialog::select("models", models(), Size::Medium);
    let buf = frame(&mut d, 72, 30, false);
    let text = super::fixture::buffer_text(&buf);
    assert!(text.iter().any(|l| l.contains("● Claude Haiku")), "{text:#?}");
    assert!(!text.iter().any(|l| l.contains('┃') || l.contains('▪')));
    let title = (0..72).find(|x| buf[(*x, 9)].symbol() == "S").unwrap();
    assert!(buf[(title, 9)].modifier.contains(ratatui::style::Modifier::BOLD));
}

/// The comparison sees a different frame: the alert against the help dialog's fixture, and the legacy alert against
/// its own.
#[test]
fn fixtures_catch_differences() {
    let read = |n: &str| Fixture::parse(&std::fs::read_to_string(format!("{}/src/ui/testdata/nd/{n}.json", env!("CARGO_MANIFEST_DIR"))).unwrap());
    let alert = || Dialog::new("alert", Body::Alert { title: "Heads up".into(), message: "The session was compacted.".into() });
    let help = read("U.3-help");
    assert!(!help.diff(&frame(&mut alert(), help.width, help.height, true), 1).is_empty());
    let own = read("U.3-alert");
    assert!(!own.diff(&frame(&mut alert(), own.width, own.height, false), 1).is_empty());
}

/// `DialogImagePreview` under the new designs: the label as the footer band's content, `previous ←` / `next →` as its
/// primary actions (with more than one image).
#[test]
fn image_preview_footer() {
    let p = super::images::ImagePreview::new(vec!["data:image/png;base64,AA".into(), "data:image/png;base64,AB".into()], vec![Some("[Image 1]".into()), None], 0);
    let mut d = Dialog::new(super::images::PREVIEW_PURPOSE, Body::Image(Box::new(p)));
    d.centered = true;
    d.size = Size::XLarge;
    let buf = frame(&mut d, 100, 30, true);
    let text = super::fixture::buffer_text(&buf);
    let row = text.iter().position(|l| l.contains("[Image 1]")).expect("the label");
    assert!(text[row].contains("previous ←   next →"), "{text:#?}");
    let x = text[row].find("[Image 1]").unwrap() as u16;
    assert_eq!(buf[(x, row as u16)].bg, theme().dialog().background.raised.high.tui());
    let legacy = super::fixture::buffer_text(&frame(&mut d, 100, 30, false));
    assert!(legacy.iter().any(|l| l.contains("← previous") && l.contains("next →")), "{legacy:#?}");
}
