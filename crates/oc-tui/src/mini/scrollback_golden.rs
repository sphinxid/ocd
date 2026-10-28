//! Golden tests: scrollback commits (rows and per-cell styles) captured from upstream's
//! `RunScrollbackStream` on OpenTUI 0.6.8 (`testdata/scrollback.json`). Each case replays the same
//! `StreamCommit`s and compares every commit snapshot.

use super::super::cell::{Color, Glyph, Intent};
use super::super::theme::{self, RunTheme};
use super::super::types::StreamCommit;
use super::*;
use crate::theme::Mode;
use serde_json::Value;

struct TestSink {
    width: usize,
    tail: usize,
    commits: Vec<Commit>,
}

impl Sink for TestSink {
    fn width(&self) -> usize {
        self.width
    }
    fn tail_column(&self) -> usize {
        self.tail
    }
    fn commit(&mut self, c: Commit) {
        self.tail = tail_after_commit(&c, self.tail, self.width);
        self.commits.push(c);
    }
}

fn color(c: Color) -> String {
    let hex = format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b);
    match c.intent {
        Intent::Indexed(slot) => format!("i{slot}"),
        Intent::Default => {
            if c.a == 255 {
                format!("d{hex}")
            } else {
                format!("d{hex}:{}", c.a)
            }
        }
        Intent::Rgb => {
            if c.a == 255 {
                hex
            } else {
                format!("{hex}:{}", c.a)
            }
        }
    }
}

fn styles(buf: &Buffer) -> Vec<Vec<String>> {
    (0..buf.height)
        .map(|y| {
            let key = |x: u16| {
                let cell = buf.get(x, y);
                if cell.glyph == Glyph::Empty { "-".to_string() } else { format!("{}/{}/{}", color(cell.fg), color(cell.bg), cell.attrs) }
            };
            let mut runs = Vec::new();
            let mut last = key(0);
            let mut start = 0;
            for x in 1..buf.width {
                let k = key(x);
                if k != last {
                    runs.push(format!("{start}-{x}:{last}"));
                    last = k;
                    start = x;
                }
            }
            runs.push(format!("{start}-{}:{last}", buf.width));
            runs
        })
        .collect()
}

fn dump(c: &Commit) -> Value {
    serde_json::json!({
        "rows": (0..c.buf.height).map(|y| c.buf.row_text(y)).collect::<Vec<_>>(),
        "styles": styles(&c.buf),
        "startOnNewLine": c.start_on_new_line,
        "trailingNewline": c.trailing_newline,
    })
}

fn expected(c: &Value) -> Value {
    serde_json::json!({
        "rows": c["rows"],
        "styles": c["styles"],
        "startOnNewLine": c["startOnNewLine"],
        "trailingNewline": c["trailingNewline"],
    })
}

fn resolved() -> RunTheme {
    let doc: Value = serde_json::from_str(crate::theme::OPENCODE_V2).unwrap();
    let t = crate::theme::resolve::resolve_document(&doc, Mode::Dark).unwrap();
    theme::resolve(&t, None, false)
}

fn run(case: &Value) -> Vec<Value> {
    let width = case["width"].as_u64().unwrap() as usize;
    let theme = if case["mono"].as_bool().unwrap() {
        theme::mono_theme(Mode::Dark)
    } else if case["theme"] == "fallback" {
        theme::fallback(Mode::Dark)
    } else {
        resolved()
    };
    let mut sink = TestSink { width, tail: 0, commits: Vec::new() };
    let mut stream = ScrollbackStream::new(theme, case["wrote"].as_bool().unwrap(), case["mono"].as_bool().unwrap(), case["shellOutput"].as_bool().unwrap(), false);
    for step in case["steps"].as_array().unwrap() {
        if let Some(c) = step.get("commit") {
            stream.append(&mut sink, StreamCommit::from_value(c));
        }
        if step.get("complete").is_some() {
            stream.complete(&mut sink, false);
        }
        if let Some(s) = step.get("summary") {
            stream.write_turn_summary(&mut sink, s["agent"].as_str().unwrap(), s["model"].as_str().unwrap(), s["duration"].as_str().unwrap());
        }
    }
    sink.commits.iter().map(dump).collect()
}

#[test]
fn matches_upstream_scrollback_commits() {
    let cases: Vec<Value> = serde_json::from_str(include_str!("testdata/scrollback.json")).unwrap();
    let only = std::env::var("MINI_GOLDEN").ok();
    let mut failed = Vec::new();
    for case in &cases {
        let name = case["name"].as_str().unwrap();
        if only.as_deref().is_some_and(|o| !name.contains(o)) {
            continue;
        }
        let got = run(case);
        let want: Vec<Value> = case["commits"].as_array().unwrap().iter().map(expected).collect();
        if got != want {
            if only.is_some() {
                eprintln!("== {name}\n-- want\n{}\n-- got\n{}", serde_json::to_string_pretty(&want).unwrap(), serde_json::to_string_pretty(&got).unwrap());
            }
            failed.push(name.to_string());
        }
    }
    assert!(failed.is_empty(), "{} of {} cases differ: {failed:?}", failed.len(), cases.len());
}
