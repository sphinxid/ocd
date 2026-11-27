//! Image previews in the full-screen TUI: `SessionImages` (`session.image_preview`), the prompt's previews
//! (`prompt.image_preview`), `DialogImagePreview` and what each graphics protocol sends.

use super::tests::{app, session};
use super::*;
use crate::graphics::Caps;
use crate::keymap::Stroke;
use base64::Engine as _;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::json;

/// The 96 x 48 landscape of upstream's `test/fixture/diff-image.ts`.
const LANDSCAPE: &[u8] = include_bytes!("../graphics/testdata/landscape.png");

fn uri() -> String {
    format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(LANDSCAPE))
}

fn caps(reply: &[u8]) -> Caps {
    let mut c = Caps::from_env(&|_| None);
    c.apply_replies(reply);
    c
}

const KITTY: &[u8] = b"\x1bP>|kitty(0.40.1)\x1b\\\x1b_Gi=31337;OK\x1b\\\x1b[4;600;1000t\x1b[?62;22c";
const SIXEL: &[u8] = b"\x1bP>|XTerm(390)\x1b\\\x1b[4;600;1000t\x1b[?62;4;22c";

/// A frame through the graphics pass: the screen's rows and the escapes sent before its cells.
fn frame(a: &mut App, term: &mut Terminal<TestBackend>) -> (Vec<String>, String) {
    let mut sent = String::new();
    images::draw_frame(term, a, &mut |b| sent.push_str(b)).unwrap();
    let buf = term.backend().buffer().clone();
    let rows = (0..buf.area.height)
        .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>().trim_end().to_string())
        .collect();
    (rows, sent)
}

/// The test session plus a user message with an image and a read whose result is an image.
fn image_session(a: &mut App, preview: bool, verbosity: &str) -> String {
    a.config = crate::config::Config::from_value(json!({
        "tabs": { "mode": "off" }, "animations": false,
        "session": { "image_preview": preview, "verbosity": verbosity }, "prompt": { "image_preview": preview },
        "experimental": { "new-designs": false }
    }));
    let sid = session(a);
    let msgs: Vec<Message> = serde_json::from_value(json!([
        {"id":"msg_1","type":"user","text":"What is in [Image 1]","time":{"created":1000},
         "files":[{"name":"shot.png","mime":"image/png","uri": uri(),"mention":{"start":11,"end":20,"text":"[Image 1]"},"source":{"type":"inline"}}]},
        {"id":"msg_2","type":"assistant","agent":"build","model":{"id":"claude-sonnet","providerID":"anthropic"},
         "time":{"created":1100,"completed":1300},"finish":"tool-calls",
         "content":[{"type":"tool","id":"call_read","name":"read","state":{"status":"completed","input":{"path":"/work/proj/shot.png"},
           "content":[{"type":"text","text":"Image read successfully"},{"type":"file","uri": uri(),"mime":"image/png","name":"shot.png"}]},
           "time":{"created":1200,"completed":1300}}]},
        {"id":"msg_3","type":"assistant","agent":"build","model":{"id":"claude-sonnet","providerID":"anthropic"},
         "time":{"created":1400,"completed":1500},"finish":"stop","content":[{"type":"text","text":"It is a landscape."}]},
        {"id":"msg_4","type":"idle","outcome":"succeeded","time":{"created":1600}}
    ]))
    .unwrap();
    a.store.messages.insert(sid.clone(), msgs);
    sid
}

#[test]
fn transcript_images_follow_the_setting() {
    let mut off = app();
    image_session(&mut off, false, "medium");
    let mut term = Terminal::new(TestBackend::new(100, 60)).unwrap();
    let (rows, sent) = frame(&mut off, &mut term);
    assert!(sent.is_empty(), "{sent:?}");
    let joined = rows.join("\n");
    assert!(joined.contains("What is in [Image 1]"), "{joined}");
    assert_eq!(off.session_image_height(), 0);
    let off_lines = off.render_transcript_lines("ses_test", 96).0.len();

    let mut on = app();
    image_session(&mut on, true, "medium");
    on.gfx.caps = caps(KITTY);
    let mut term = Terminal::new(TestBackend::new(100, 60)).unwrap();
    let (_, sent) = frame(&mut on, &mut term);
    // 60 rows: strips of 8 rows plus a blank above and below, for the user message and for the read.
    assert_eq!(on.session_image_height(), 8);
    assert_eq!(on.render_transcript_lines("ses_test", 96).0.len(), off_lines + 2 * 10);
    // One transmission per placement (the same PNG twice), then each placement: 16 x 8 cells of 10 x 10 pixels,
    // the 2:1 box shows the whole 96 x 48 source.
    assert_eq!(sent.matches("a=t,f=100,").count(), 2, "{sent:?}");
    assert_eq!(sent.matches("c=16,r=8,x=0,y=0,w=96,h=48,C=1,").count(), 2, "{sent:?}");
}

#[test]
fn kitty_escapes_match_upstream() {
    // `tui_capture.py --terminal kitty --pixels 1000x600` at 100 x 30 against the 2.3.1 binary, the session opened
    // from the list (first frame): the user image's last row at the top, the read's image below its group:
    //   kitty a=t,f=100,i=1,m=0,q=2 payload=544:df95bb6de5856562
    //   ESC[1;6H kitty a=p,i=1,p=1,c=14,r=1,x=24,y=41,w=48,h=7,C=1,z=-1073741823,q=2
    //   kitty a=t,f=100,i=2,m=0,q=2 payload=544:df95bb6de5856562
    //   ESC[11;6H kitty a=p,i=2,p=2,c=14,r=7,x=24,y=0,w=48,h=48,C=1,z=-1073741822,q=2
    let mut a = app();
    image_session(&mut a, true, "medium");
    a.gfx.caps = caps(KITTY);
    a.gfx.salt = 1000;
    let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
    let (rows, sent) = frame(&mut a, &mut term);
    let png = base64::engine::general_purpose::STANDARD.encode(LANDSCAPE);
    assert_eq!(png.len(), 544);
    let joined = rows.join("\n");
    assert!(sent.starts_with(&format!("\x1b_Ga=t,f=100,i=1001,m=0,q=2;{png}\x1b\\")), "{sent:?}\n{joined}");
    assert!(sent.contains(&format!("\x1b[1;6H\x1b_Ga=p,i=1001,p=1,c=14,r=1,x=24,y=41,w=48,h=7,C=1,z=-1073741823,q=2\x1b\\\x1b_Ga=t,f=100,i=1002,m=0,q=2;{png}\x1b\\")), "{sent:?}");
    assert!(sent.contains("\x1b[11;6H\x1b_Ga=p,i=1002,p=2,c=14,r=7,x=24,y=0,w=48,h=48,C=1,z=-1073741822,q=2\x1b\\"), "{sent:?}");
    // The image's cells are blanked before the image, left out of the cell diff afterwards.
    assert!(sent.contains("\x1b[0m\x1b["), "{sent:?}");
    // Unchanged: nothing more is sent.
    let (_, again) = frame(&mut a, &mut term);
    assert_eq!(again, "");
    // Leaving the screen frees the image.
    assert_eq!(a.gfx.shutdown(), "\x1b_Ga=d,d=I,i=1001,q=2\x1b\\\x1b_Ga=d,d=I,i=1002,q=2\x1b\\");
}

#[test]
fn sixel_paints_at_the_box() {
    let mut a = app();
    image_session(&mut a, true, "medium");
    a.gfx.caps = caps(SIXEL);
    let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
    let (_, sent) = frame(&mut a, &mut term);
    let at = sent.find("\x1bP0;1;0q\"1;1;140;140").expect("a sixel of the box's 140 x 140 pixels");
    assert!(sent[..at].ends_with("\x1b[11;6H"), "{:?}", &sent[..at.min(200)]);
    assert!(sent.ends_with("\x1b\\"));
    let (_, again) = frame(&mut a, &mut term);
    assert_eq!(again, "");
}

#[test]
fn blocks_without_graphics() {
    let mut a = app();
    image_session(&mut a, true, "medium");
    a.gfx.caps = caps(b"\x1b[?62;22c");
    let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
    let (rows, sent) = frame(&mut a, &mut term);
    assert_eq!(sent, "");
    let quadrants = rows.iter().map(|r| r.chars().filter(|c| crate::graphics::blocks::QUADRANTS[1..].contains(c)).count()).sum::<usize>();
    assert!(quadrants > 14 * 7 / 2, "{}", rows.join("\n"));
    assert!(!rows.join("").contains(crate::graphics::frame::MARKER));
}

#[test]
fn collapsed_activity_hides_tool_images() {
    // At `verbosity: low` the read sits in a collapsed activity row, which shows no images.
    let mut a = app();
    image_session(&mut a, true, "low");
    a.gfx.caps = caps(KITTY);
    let lines = a.render_transcript_lines("ses_test", 96).0;
    assert_eq!(lines.iter().filter(|l| l.image.as_ref().is_some_and(|r| r.row == 0)).count(), 1);
}

#[test]
fn a_click_opens_the_preview_and_arrows_page() {
    let mut a = app();
    image_session(&mut a, true, "medium");
    a.gfx.caps = caps(KITTY);
    let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
    frame(&mut a, &mut term);
    let (rect, click) = a.hits.iter().find(|(_, c)| matches!(c, Click::Custom(x) if x.starts_with(crate::ui::images::CLICK))).cloned().unwrap();
    assert_eq!((rect.x, rect.width, rect.height), (5, 14, 7));
    a.on_click(click);
    let (rows, sent) = frame(&mut a, &mut term);
    let joined = rows.join("\n");
    assert!(joined.contains("Image 1 of 1"), "{joined}");
    // One image: no paging labels, the footer names the image.
    assert!(!joined.contains("← previous"), "{joined}");
    assert!(joined.contains("Image 1"), "{joined}");
    // The dialog's image is fitted: 96 x 48 into 92 x 22 cells twice as tall as wide gives 88 x 22.
    assert!(sent.contains("\x1b[5;7H\x1b_Ga=p,i="), "{sent:?}");
    assert!(sent.contains(",c=88,r=22,x=0,y=0,w=96,h=48,"), "{sent:?}");
    a.on_key(Stroke::key("escape"));
    assert!(a.dialogs.is_empty());
}

#[test]
fn the_preview_pages_through_prompt_images() {
    let mut a = app();
    a.config = crate::config::Config::from_value(json!({ "tabs": { "mode": "off" }, "animations": false, "prompt": { "image_preview": true }, "experimental": { "new-designs": false } }));
    let u = uri();
    a.editor.insert_chip("[Image 1]", crate::ui::prompt::ChipKind::Paste, json!({ "uri": u, "name": "a.png", "mime": "image/png" }), true);
    a.editor.insert_chip("[Image 2]", crate::ui::prompt::ChipKind::Paste, json!({ "uri": "data:image/png;base64,AQID", "name": "b.png", "mime": "image/png" }), true);
    assert_eq!(a.prompt_images().len(), 2);
    a.gfx.caps = caps(b"\x1b[?62;22c");
    let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
    let (rows, _) = frame(&mut a, &mut term);
    let joined = rows.join("\n");
    // The broken image says so in its box; the prompt grew by the strip and its blank row.
    assert!(joined.contains("No preview"), "{joined}");
    assert!(a.run_command("prompt.images.view", None));
    let (rows, _) = frame(&mut a, &mut term);
    let joined = rows.join("\n");
    assert!(joined.contains("Image 1 of 2"), "{joined}");
    assert!(joined.contains("← previous") && joined.contains("next →") && joined.contains("[Image 1]"), "{joined}");
    a.on_key(Stroke::key("right"));
    let (rows, _) = frame(&mut a, &mut term);
    let joined = rows.join("\n");
    assert!(joined.contains("Image 2 of 2") && joined.contains("No preview"), "{joined}");
    a.on_key(Stroke::key("right"));
    let (rows, _) = frame(&mut a, &mut term);
    assert!(rows.join("\n").contains("Image 1 of 2"));
}

#[test]
fn view_command_without_images_does_nothing() {
    let mut a = app();
    assert!(!a.run_command("prompt.images.view", None));
    assert!(a.dialogs.is_empty());
}

#[test]
fn a_dialog_over_a_kitty_image_splits_its_placement() {
    let mut a = app();
    image_session(&mut a, true, "medium");
    a.gfx.caps = caps(KITTY);
    a.gfx.salt = 0;
    let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
    frame(&mut a, &mut term);
    // A dialog a quarter down covers part of the read's image: the owner placement is dropped and the uncovered
    // runs are placed with ids from 0x80000000.
    let mut d = crate::ui::dialog::Dialog::new("t", crate::ui::dialog::Body::Alert { title: "Hi".into(), message: "x".repeat(200) });
    // 88 columns centered: the image's first column stays uncovered.
    d.size = crate::ui::dialog::Size::Large;
    a.open_dialog(d);
    let (_, sent) = frame(&mut a, &mut term);
    assert!(sent.contains("c=1,r=7,x=24,y=0,w=4,h=48,C=1,z=-1073741822,q=2"), "{sent:?}");
    assert!(sent.contains("\x1b_Ga=d,d=i,i=2,p=2,q=2\x1b\\"), "{sent:?}");
    assert!(sent.contains("p=2147483648,"), "{sent:?}");
    assert!(!sent.contains("a=t,"), "{sent:?}");
    a.dialogs.clear();
    let (_, sent) = frame(&mut a, &mut term);
    assert!(sent.contains("a=d,d=i,i=2,p=2147483648,q=2"), "{sent:?}");
    assert!(sent.contains("a=p,i=2,p=2,c=14,r=7"), "{sent:?}");
}

#[test]
fn incremental_transcript_matches_reference_with_images() {
    for verbosity in ["low", "medium", "high"] {
        let mut a = app();
        let sid = image_session(&mut a, true, verbosity);
        a.size = (100, 40);
        for width in [96, 40, 12] {
            a.transcript_sync(&sid, width);
            let (lines, marks) = a.render_transcript_lines(&sid, width);
            assert_eq!(a.tcache.to_vec(), lines, "{verbosity} at {width}");
            assert_eq!(a.tcache.marks(), &marks[..]);
        }
        // Turning previews off (or a new terminal height) renders the rows again.
        a.config = crate::config::Config::from_value(json!({ "tabs": { "mode": "off" }, "session": { "verbosity": verbosity } }));
        a.transcript_sync(&sid, 96);
        let (lines, _) = a.render_transcript_lines(&sid, 96);
        assert_eq!(a.tcache.to_vec(), lines);
        assert!(lines.iter().all(|l| l.image.is_none()));
    }
}
