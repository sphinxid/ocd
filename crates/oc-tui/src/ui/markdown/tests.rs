use super::*;
use crate::theme::{Mode, OPENCODE_V2, resolve};
use serde_json::Value;

fn theme() -> Theme {
    let doc: serde_json::Value = serde_json::from_str(OPENCODE_V2).unwrap();
    resolve::resolve_document(&doc, Mode::Dark).unwrap()
}

fn rows_of(lines: &[SLine]) -> Vec<String> {
    lines.iter().map(|l| l.text().trim_end().to_string()).collect()
}

fn lines(text: &str, conceal: bool) -> Vec<String> {
    let t = theme();
    let st = SyntaxStyle::from_theme(&t);
    let o = MdOpts { theme: &t, syntax: &st, conceal, fg: t.markdown.text, streaming: false, links: None };
    rows_of(&render(text, 40, &o))
}

const KNOWN: &str = "ses_knownknownknownknownknownk";
const OTHER: &str = "ses_otherotherotherotherothero";

type Linked = (Vec<String>, Vec<(String, Click)>, Vec<(String, Option<String>)>);

/// Renders with links for `KNOWN` (titled) only: (rows, linked texts with their targets, mentioned IDs).
fn linked(text: &str, conceal: bool, width: usize) -> Linked {
    let t = theme();
    let st = SyntaxStyle::from_theme(&t);
    let label = |id: &str| (id == KNOWN).then(|| "Fix the flaky reconnect test on Windows".to_string());
    let links = SessionLinks::new(&label);
    let o = MdOpts { theme: &t, syntax: &st, conceal, fg: t.markdown.text, streaming: false, links: Some(&links) };
    let out = render(text, width, &o);
    let found = out.iter().flat_map(|l| l.segs.iter()).filter_map(|s| s.link.as_ref().map(|c| (s.text.clone(), (**c).clone()))).collect();
    for s in out.iter().flat_map(|l| l.segs.iter()).filter(|s| s.link.is_some()) {
        assert!(s.style.add_modifier.contains(Modifier::UNDERLINED));
        assert_eq!(s.style.fg, Some(t.markdown.link.tui()));
    }
    (rows_of(&out), found, links.take())
}

/// `session-link.test.tsx` "session links in … markdown": only loaded sessions link; a long
/// title wraps and both rows open it; fenced code stays plain.
#[test]
fn session_ids_link_in_prose() {
    let text = format!("**中文** see {KNOWN} now\n\n- other {OTHER}\n\n```\n{KNOWN}\n```");
    let (rows, found, used) = linked(&text, true, 30);
    let link = Click::SessionLink(KNOWN.into());
    // OpenTUI's word wrap keeps the space a row breaks after.
    assert_eq!(found, vec![("@Fix the flaky ".to_string(), link.clone()), ("reconnect test on Windows".to_string(), link)]);
    assert_eq!(rows[0], "中文 see @Fix the flaky");
    assert_eq!(rows[1], "reconnect test on Windows now");
    // The ID is wider than the item: it starts a row of its own and breaks by character.
    assert_eq!(rows[3..6], ["- other", "  ses_otherotherotherotherothe", "  ro"], "{rows:?}");
    assert_eq!(rows.last().unwrap(), KNOWN);
    assert_eq!(used, vec![(KNOWN.to_string(), Some("Fix the flaky reconnect test on Windows".to_string())), (OTHER.to_string(), None)]);
    // Source mode keeps the ID text, still a link; `@session:` mentions link too.
    let (rows, found, _) = linked(&format!("see @session:{KNOWN} now"), false, 80);
    assert_eq!(rows, [format!("see @session:{KNOWN} now")]);
    assert_eq!(found, vec![(format!("@session:{KNOWN}"), Click::SessionLink(KNOWN.into()))]);
}

/// "code spans link only when they are exactly an ID".
#[test]
fn code_spans_link_only_when_exactly_an_id() {
    let text = format!("Run `opencode --session {KNOWN}` or open `{KNOWN}`.");
    let (rows, found, _) = linked(&text, true, 200);
    assert_eq!(found.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>(), ["@Fix the flaky reconnect test on Windows"]);
    assert!(rows[0].contains(&format!("opencode --session {KNOWN}")));
    let (rows, found, _) = linked(&text, false, 200);
    assert_eq!(found.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>(), [KNOWN]);
    assert!(rows[0].contains(&format!("`opencode --session {KNOWN}`")));
}

#[test]
fn urls_and_markdown_links_keep_their_ids() {
    let text = format!("https://host/{KNOWN} and [x](https://h/{KNOWN}) and [{KNOWN}](https://h)");
    let (_, found, used) = linked(&text, true, 200);
    assert!(found.is_empty() && used.is_empty(), "{found:?}");
}

#[test]
fn basics() {
    assert_eq!(lines("# Title\n\nSome **bold** text", true), vec!["Title", "", "Some bold text"]);
    assert_eq!(lines("Some **bold**", false), vec!["Some **bold**"]);
    assert_eq!(lines("- a\n- b", true), vec!["- a", "- b"]);
    assert_eq!(lines("1. a\n2. b", true), vec!["1. a", "2. b"]);
    assert_eq!(lines("```rust\nfn x() {}\n```", true), vec!["fn x() {}"]);
    let t = lines("| a | b |\n|---|---|\n| 1 | 2 |", true);
    assert_eq!(t[0], format!("┌{}┬{}┐", "─".repeat(19), "─".repeat(18)));
    assert_eq!(t[1], format!("│ a{}│ b{}│", " ".repeat(17), " ".repeat(16)));
    assert_eq!(t.len(), 5);
}

/// Every row of the answers and the reasoning block opencode 2.4.1 drew on a pty (`testdata/markdown_241.json`):
/// setext headings with their underline row, `- - -` and the other rules, one blank row between blocks, soft line
/// breaks kept, task boxes, loose and tight lists, numbers aligned on their right, quotes with their bars and gaps,
/// full-width grid tables aligned by column, links, autolinks and images, HTML, source mode.
#[test]
fn matches_opencode_241_frames() {
    let doc: Value = serde_json::from_str(include_str!("../testdata/markdown_241.json")).unwrap();
    let t = theme();
    let st = SyntaxStyle::from_theme(&t);
    let thinking = SyntaxStyle::muted(&t);
    let mut failed = Vec::new();
    for case in doc["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let reasoning = case["reasoning"].as_bool().unwrap_or(false);
        let o = MdOpts {
            theme: &t,
            syntax: if reasoning { &thinking } else { &st },
            conceal: case["conceal"].as_bool().unwrap(),
            fg: if reasoning { t.text.muted } else { t.markdown.text },
            streaming: false,
            links: None,
        };
        let got = rows_of(&render(case["markdown"].as_str().unwrap(), case["width"].as_u64().unwrap() as usize, &o));
        let want: Vec<String> = case["rows"].as_array().unwrap().iter().map(|r| r.as_str().unwrap().to_string()).collect();
        if got != want {
            eprintln!("== {name}\n-- want\n{}\n-- got\n{}", want.join("\n"), got.join("\n"));
            failed.push(name.to_string());
        }
    }
    assert!(failed.is_empty(), "{failed:?}");
}

/// OpenTUI's own Markdown corpus (`testdata/markdown_conformance.json`): each row streamed in its chunks, the frame
/// after each chunk at width 60 with content-width tables and no cell padding, as Core's test draws it.
#[test]
fn matches_opentui_conformance_frames() {
    let doc: Value = serde_json::from_str(include_str!("../testdata/markdown_conformance.json")).unwrap();
    let t = theme();
    let st = SyntaxStyle::from_theme(&t);
    let mut failed = Vec::new();
    for row in doc["rows"].as_array().unwrap() {
        let name = row["name"].as_str().unwrap();
        let chunks: Vec<&str> = row["chunks"].as_array().unwrap().iter().map(|c| c.as_str().unwrap()).collect();
        let skip: Vec<&str> = row["skip"].as_array().unwrap().iter().map(|c| c.as_str().unwrap()).collect();
        let mut content = String::new();
        for (i, chunk) in chunks.iter().enumerate() {
            content.push_str(chunk);
            if skip.contains(&format!("core-text:{}", i + 1).as_str()) {
                continue;
            }
            let o = MdOpts { theme: &t, syntax: &st, conceal: true, fg: t.markdown.text, streaming: i + 1 < chunks.len(), links: None };
            let got = rows_of(&render_with(&content, 60, &o, TableOpts { full: false, pad: 0 })).join("\n").trim_end().to_string();
            let want = row["frames"][i].as_str().unwrap();
            if got != want {
                eprintln!("== {name} #{}\n-- want\n{want}\n-- got\n{got}", i + 1);
                failed.push(format!("{name} #{}", i + 1));
            }
        }
    }
    assert!(failed.is_empty(), "{} differ: {failed:?}", failed.len());
}

fn style_at(lines: &[SLine], row: usize, text: &str) -> Style {
    let mut seen = String::new();
    for s in &lines[row].segs {
        seen.push_str(&s.text);
        if seen.contains(text) {
            return s.style;
        }
    }
    panic!("{text:?} not in row {row}: {:?}", lines[row].text());
}

/// The styles 2.4.1 draws: setext headings in the heading style, rules in the Markdown color, quote bars in the
/// `default` color with quote text in `markup.quote`, task boxes in the list color, `#888888` table borders and a
/// header in the heading style, links as label + `(url)`, autolinks in the label style, escapes in `string.escape`,
/// inline code in `markup.raw` without its background.
#[test]
fn styles_follow_the_syntax_groups() {
    let t = theme();
    let st = SyntaxStyle::from_theme(&t);
    let o = MdOpts { theme: &t, syntax: &st, conceal: true, fg: t.markdown.text, streaming: false, links: None };
    let text = "Title\n===\n\n- - -\n\n> quote **b**\n\n- [ ] task\n\n| h |\n|---|\n| `c` |\n\nSee [x](https://u.v) and https://bare.w and \\* and `code`.";
    let out = render(text, 80, &o);
    let fg = |c: Rgba| Some(c.tui());
    assert_eq!(style_at(&out, 0, "Title").fg, fg(t.markdown.heading));
    assert!(style_at(&out, 1, "===").add_modifier.contains(Modifier::BOLD | Modifier::UNDERLINED));
    assert_eq!(style_at(&out, 3, "─").fg, fg(t.markdown.text));
    assert_eq!(style_at(&out, 5, "│").fg, fg(t.text.base));
    let q = style_at(&out, 5, "quote");
    assert_eq!(q.fg, fg(t.markdown.block_quote));
    assert!(q.add_modifier.contains(Modifier::ITALIC));
    let b = style_at(&out, 5, "b");
    assert_eq!(b.fg, fg(t.markdown.strong));
    assert!(b.add_modifier.contains(Modifier::BOLD | Modifier::ITALIC));
    assert_eq!(out[7].text(), "- ☐ task");
    assert_eq!(style_at(&out, 7, "☐").fg, fg(t.markdown.list_item));
    assert_eq!(style_at(&out, 9, "┌").fg, Some(TABLE_BORDER.tui()));
    let h = style_at(&out, 10, "h");
    assert_eq!(h.fg, fg(t.markdown.heading));
    assert!(h.add_modifier.contains(Modifier::BOLD));
    let c = style_at(&out, 12, "c");
    assert_eq!(c.fg, fg(t.markdown.code));
    assert_eq!(c.bg, None);
    assert_eq!(out[15].text(), "See x (https://u.v) and https://bare.w and * and code.");
    assert_eq!(style_at(&out, 15, "x").fg, fg(t.markdown.link_text));
    assert_eq!(style_at(&out, 15, "(https").fg, fg(t.markdown.link));
    assert_eq!(style_at(&out, 15, "https://bare").fg, fg(t.markdown.link_text));
    assert_eq!(style_at(&out, 15, "and *").fg, fg(t.syntax.keyword));
}

/// Reasoning draws Markdown in the reasoning colors: every group in `text.muted`, its attributes kept.
#[test]
fn reasoning_draws_markdown_in_muted_colors() {
    let t = theme();
    let st = SyntaxStyle::muted(&t);
    let o = MdOpts { theme: &t, syntax: &st, conceal: true, fg: t.text.muted, streaming: false, links: None };
    let out = render("## Plan\n\n**bold** `code`\n\n> q", 40, &o);
    let muted = Some(t.text.muted.tui());
    let h = style_at(&out, 0, "Plan");
    assert_eq!(h.fg, muted);
    assert!(h.add_modifier.contains(Modifier::BOLD));
    assert_eq!(style_at(&out, 2, "bold").fg, muted);
    assert_eq!(style_at(&out, 2, "code").fg, muted);
    assert_eq!(style_at(&out, 4, "│").fg, muted);
}

/// While a message streams, a closing fence, a setext underline or a table row still arriving waits, and an open
/// emphasis or code span draws as if it had closed (the 2.4.1 frames of a stalled stream).
#[test]
fn streaming_shows_what_is_decided() {
    let t = theme();
    let st = SyntaxStyle::from_theme(&t);
    let o = MdOpts { theme: &t, syntax: &st, conceal: true, fg: t.markdown.text, streaming: true, links: None };
    let show = |s: &str| rows_of(&render(s, 60, &o));
    assert_eq!(show("Text **bold part"), ["Text bold part"]);
    assert!(style_at(&render("Text **bold part", 60, &o), 0, "bold").add_modifier.contains(Modifier::BOLD));
    assert_eq!(show("Run `cmd --fl"), ["Run cmd --fl"]);
    assert_eq!(show("Intro line\n\n| a | b"), ["Intro line"]);
    assert_eq!(show("Start\n\n```py\nx = 1\ny"), ["Start", "", "x = 1", "y"]);
    assert_eq!(show("Start\n\n```py\nx = 1\n``"), ["Start", "", "x = 1"]);
    assert_eq!(show("Title\n=="), ["Title"]);
    assert_eq!(show("See [the docs](https://exa"), ["See [the docs"]);
    assert_eq!(show("Items:\n\n- one\n- tw"), ["Items:", "", "- one", "- tw"]);
}

/// Containers nest at most 32 deep (`depth_max`), and a stream of table-like lines holds without recursing.
#[test]
fn deep_nesting_and_long_streams_stay_bounded() {
    let t = theme();
    let st = SyntaxStyle::from_theme(&t);
    let o = MdOpts { theme: &t, syntax: &st, conceal: true, fg: t.markdown.text, streaming: true, links: None };
    let deep = format!("{}deep", "> ".repeat(5000));
    let out = render(&deep, 80, &o);
    assert_eq!(out.len(), 1);
    assert!(out[0].text().ends_with("│ deep"));
    let pipes = "a | b\n".repeat(20_000);
    let _ = render(&pipes, 80, &o);
    let lists = format!("{}x", "- ".repeat(5000));
    let _ = render(&lists, 80, &o);
}
