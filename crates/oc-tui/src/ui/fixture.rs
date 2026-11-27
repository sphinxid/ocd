//! Cell fixtures recorded from upstream's TypeScript TUI with the new designs on (`packages/cli-rs/parity/fixtures`
//! in upstream 2.4.1: `createTestRenderer` frames of one component at a fixed size, theme `opencode` dark, animations
//! off). Each fixture lists its rows and, per row, runs of `[style index, cells]` into `styles` (`"#fg #bg attrs"`).
//! A frame matches when every cell has the same character and background, and every cell that is not a space the
//! same foreground and bold attribute.

use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};
use serde_json::Value;

pub struct Fixture {
    pub width: u16,
    pub height: u16,
    rows: Vec<Vec<(String, String, String, bool)>>,
}

fn rgb(hex: &str) -> String {
    hex.trim_start_matches('#')[..6].to_string()
}

impl Fixture {
    pub fn parse(text: &str) -> Fixture {
        let v: Value = serde_json::from_str(text).unwrap();
        let styles: Vec<(String, String, bool)> = v["styles"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| {
                let p: Vec<&str> = s.as_str().unwrap().split(' ').collect();
                (rgb(p[0]), rgb(p[1]), p[2].parse::<u32>().unwrap() & 1 == 1)
            })
            .collect();
        let rows = v["lines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| {
                let chars: Vec<String> = crate::util::locale::graphemes(l[0].as_str().unwrap()).into_iter().map(str::to_string).collect();
                let mut cells = Vec::new();
                let mut x = 0;
                for run in l[1].as_array().unwrap() {
                    let (i, n) = (run[0].as_u64().unwrap() as usize, run[1].as_u64().unwrap() as usize);
                    for _ in 0..n {
                        let (f, b, bold) = &styles[i];
                        cells.push((chars.get(x).cloned().unwrap_or(" ".into()), f.clone(), b.clone(), *bold));
                        x += 1;
                    }
                }
                cells
            })
            .collect();
        Fixture { width: v["width"].as_u64().unwrap() as u16, height: v["height"].as_u64().unwrap() as u16, rows }
    }

    /// Every differing cell, at most `limit`, as `(x, y): want … got …`.
    pub fn diff(&self, buf: &Buffer, limit: usize) -> Vec<String> {
        let hex = |c: Color| match c {
            Color::Rgb(r, g, b) => format!("{r:02x}{g:02x}{b:02x}"),
            other => format!("{other:?}"),
        };
        let mut out = Vec::new();
        for (y, row) in self.rows.iter().enumerate() {
            for (x, (ch, f, b, bold)) in row.iter().enumerate() {
                let cell = &buf[(x as u16, y as u16)];
                let got_ch = cell.symbol();
                let space = ch == " ";
                let same = got_ch == ch
                    && hex(cell.bg) == *b
                    && (space || (hex(cell.fg) == *f && cell.modifier.contains(Modifier::BOLD) == *bold));
                if !same && out.len() < limit {
                    out.push(format!(
                        "({x},{y}): want {ch:?} {f}/{b}{} got {got_ch:?} {}/{}{}",
                        if *bold { " bold" } else { "" },
                        hex(cell.fg),
                        hex(cell.bg),
                        if cell.modifier.contains(Modifier::BOLD) { " bold" } else { "" }
                    ));
                }
            }
        }
        out
    }

    pub fn text(&self) -> Vec<String> {
        self.rows.iter().map(|r| r.iter().map(|c| c.0.as_str()).collect::<String>().trim_end().to_string()).collect()
    }
}

pub fn buffer_text(buf: &Buffer) -> Vec<String> {
    (0..buf.area.height)
        .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>().trim_end().to_string())
        .collect()
}

/// Assert a frame matches a fixture, printing both texts and the first differing cells otherwise.
pub fn assert_matches(name: &str, fx: &Fixture, buf: &Buffer) {
    let diff = fx.diff(buf, 30);
    assert!(
        diff.is_empty(),
        "{name}: {} cells differ\n-- want\n{}\n-- got\n{}\n{}",
        diff.len(),
        fx.text().join("\n"),
        buffer_text(buf).join("\n"),
        diff.join("\n")
    );
}
