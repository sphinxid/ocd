//! Every built-in theme, the generated `system` theme over six terminal palettes and mini's V1 migration, resolved
//! like upstream 2.4.1's TUI resolves them. `testdata/tokens_241.json` is `parity/fixtures/U.1/tokens.json` from
//! upstream's `packages/cli-rs` (recorded through the TypeScript TUI's own functions with the new designs on: every
//! V1 theme through `migrateV1NewDesigns`, mini through `migrateV1`). Colors are packed as `rrggbbaa`, tokens in the
//! order of `paths`, hues one 9-step scale each.

use super::*;
use serde_json::Value;

const FIXTURE: &str = include_str!("testdata/tokens_241.json");

fn pack(c: Rgba) -> String {
    format!("{:02x}{:02x}{:02x}{:02x}", c.r, c.g, c.b, c.a)
}

fn unpack(packed: &str) -> Vec<&str> {
    (0..packed.len() / 8).map(|i| &packed[i * 8..i * 8 + 8]).collect()
}

fn mode(name: &str) -> Mode {
    if name == "light" { Mode::Light } else { Mode::Dark }
}

fn mode_name(m: Mode) -> &'static str {
    if m == Mode::Light { "light" } else { "dark" }
}

fn stateful(s: &resolve::Stateful, key: &str) -> Option<Rgba> {
    Some(match key {
        "base" => s.base,
        "disabled" => s.disabled,
        "focused" => s.focused,
        "hovered" => s.hovered,
        "pressed" => s.pressed,
        "selected" => s.selected,
        _ => return None,
    })
}

/// A resolved token by its dotted path.
fn token(t: &Theme, path: &str) -> Option<Rgba> {
    let p: Vec<&str> = path.split('.').collect();
    let actions = |a: &resolve::Actions, v: &str, k: &str| match v {
        "primary" => stateful(&a.primary, k),
        "secondary" => stateful(&a.secondary, k),
        "destructive" => stateful(&a.destructive, k),
        _ => None,
    };
    let (d, s, m) = (&t.diff, &t.syntax, &t.markdown);
    Some(match p.as_slice() {
        ["background", "action", v, k] => return actions(&t.background.action, v, k),
        ["background", "base"] => t.background.base,
        ["background", "feedback", f] => match *f {
            "error" => t.background.feedback.error,
            "info" => t.background.feedback.info,
            "success" => t.background.feedback.success,
            "warning" => t.background.feedback.warning,
            _ => return None,
        },
        ["background", "feedback", f, "base"] => return token(t, &format!("background.feedback.{f}")),
        ["background", "formfield", k] => return stateful(&t.background.formfield, k),
        ["background", "raised", "base"] => t.background.raised.base,
        ["background", "raised", "high"] => t.background.raised.high,
        ["background", "raised", "max"] => t.background.raised.max,
        ["border", "base"] => t.border,
        ["scrollbar", "base"] => t.scrollbar,
        ["diff", "background", "added"] => d.bg_added,
        ["diff", "background", "context"] => d.bg_context,
        ["diff", "background", "removed"] => d.bg_removed,
        ["diff", "highlight", "added"] => d.highlight_added,
        ["diff", "highlight", "removed"] => d.highlight_removed,
        ["diff", "lineNumber", "background", "added"] => d.line_number_bg_added,
        ["diff", "lineNumber", "background", "removed"] => d.line_number_bg_removed,
        ["diff", "lineNumber", "text"] => d.line_number_text,
        ["diff", "text", "added"] => d.text_added,
        ["diff", "text", "context"] => d.text_context,
        ["diff", "text", "hunkHeader"] => d.text_hunk_header,
        ["diff", "text", "removed"] => d.text_removed,
        ["markdown", k] => match *k {
            "blockQuote" => m.block_quote,
            "code" => m.code,
            "codeBlock" => m.code_block,
            "emphasis" => m.emphasis,
            "heading" => m.heading,
            "horizontalRule" => m.horizontal_rule,
            "image" => m.image,
            "imageText" => m.image_text,
            "link" => m.link,
            "linkText" => m.link_text,
            "listEnumeration" => m.list_enumeration,
            "listItem" => m.list_item,
            "strong" => m.strong,
            "text" => m.text,
            _ => return None,
        },
        ["syntax", k] => match *k {
            "comment" => s.comment,
            "function" => s.function,
            "keyword" => s.keyword,
            "number" => s.number,
            "operator" => s.operator,
            "punctuation" => s.punctuation,
            "string" => s.string,
            "type" => s.type_,
            "variable" => s.variable,
            _ => return None,
        },
        ["text", "action", v, k] => return actions(&t.text.action, v, k),
        ["text", "base"] => t.text.base,
        ["text", "faint"] => t.text.faint,
        ["text", "muted"] => t.text.muted,
        ["text", "feedback", f, k] => {
            let fb = match *f {
                "error" => t.text.feedback.error,
                "info" => t.text.feedback.info,
                "success" => t.text.feedback.success,
                "warning" => t.text.feedback.warning,
                _ => return None,
            };
            if *k == "muted" { fb.muted } else { fb.base }
        }
        ["text", "formfield", k] => return stateful(&t.text.formfield, k),
        _ => return None,
    })
}

/// Every difference between a resolved theme and its upstream entry, named by path.
fn compare(key: &str, modes: &[Mode], t: &Theme, expected: &Value, paths: &[String], failures: &mut Vec<String>) {
    let modes: Vec<&str> = modes.iter().map(|m| mode_name(*m)).collect();
    let want: Vec<&str> = expected["modes"].as_array().unwrap().iter().map(|m| m.as_str().unwrap()).collect();
    if modes != want {
        failures.push(format!("{key}: modes {modes:?}, expected {want:?}"));
    }
    let hues: Vec<&str> = t.hues.iter().map(|(n, _)| n.as_str()).collect();
    let want: Vec<&str> = expected["hues"].as_array().unwrap().iter().map(|h| h.as_str().unwrap()).collect();
    if hues != want {
        failures.push(format!("{key}: hues {hues:?}, expected {want:?}"));
    }
    let mut check = |what: &str, names: Vec<String>, actual: Vec<Rgba>| {
        let Some(wanted) = expected[what].as_str().map(unpack) else { return };
        if wanted.len() != actual.len() {
            failures.push(format!("{key}: {what} has {} colors, expected {}", actual.len(), wanted.len()));
            return;
        }
        for ((name, actual), wanted) in names.iter().zip(actual).zip(wanted) {
            if pack(actual) != wanted {
                failures.push(format!("{key}: {what} {name} is {}, expected {wanted}", pack(actual)));
            }
        }
    };
    let steps = |name: &str| (1..=9).map(|s| format!("{name}.{}", s * 100)).collect::<Vec<_>>();
    check("hue", t.hues.iter().flat_map(|(n, _)| steps(n)).collect(), t.hues.iter().flat_map(|(_, s)| *s).collect());
    check(
        "categorical",
        (0..t.categorical.len()).flat_map(|i| steps(&i.to_string())).collect(),
        t.categorical.iter().flat_map(|s| *s).collect(),
    );
    let sorted = |view: &Theme| -> Vec<Rgba> {
        paths.iter().map(|p| token(view, p).unwrap_or(Rgba::rgba(1, 2, 3, 4))).collect()
    };
    check("base", paths.to_vec(), sorted(t));
    check("dialog", paths.to_vec(), sorted(t.dialog()));
    check("footer", paths.to_vec(), sorted(t.dialog_footer()));
    check("decrease", paths.to_vec(), sorted(t).into_iter().map(|c| t.dec(c)).collect());
    check("increase", paths.to_vec(), sorted(t).into_iter().map(|c| t.increase(c, 2)).collect());
}

fn paths(fixture: &Value) -> Vec<String> {
    fixture["paths"].as_array().unwrap().iter().map(|p| p.as_str().unwrap().to_owned()).collect()
}

fn report(failures: Vec<String>, checked: usize) {
    assert!(checked > 0, "nothing checked");
    let shown: Vec<_> = failures.iter().take(40).cloned().collect();
    assert!(failures.is_empty(), "{} mismatches over {checked} views:\n{}", failures.len(), shown.join("\n"));
}

fn manager(new_designs: bool) -> ThemeManager {
    let mut m = ThemeManager::new(None, Mode::Dark, None);
    m.set_new_designs(new_designs);
    m
}

#[test]
fn builtins_match_upstream_with_new_designs() {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    let paths = paths(&fixture);
    let mut m = manager(true);
    let mut failures = Vec::new();
    let mut checked = 0;
    let entries = fixture["builtins"].as_object().unwrap();
    let names: Vec<String> = m.names().into_iter().filter(|n| n != "system").collect();
    let listed: std::collections::BTreeSet<&str> = entries.keys().map(|k| k.split('/').next().unwrap()).collect();
    assert_eq!(names.iter().map(|n| n.as_str()).collect::<std::collections::BTreeSet<_>>(), listed);
    for (key, expected) in entries {
        let (name, want) = key.split_once('/').unwrap();
        let doc = m.document(name).unwrap_or_else(|e| panic!("{key}: {e}"));
        let t = resolve::resolve_document(&doc, mode(want)).unwrap_or_else(|e| panic!("{key}: {e}"));
        compare(key, &resolve::modes(&doc), &t, expected, &paths, &mut failures);
        checked += 1;
    }
    report(failures, checked);
}

fn colors_of(v: &Value) -> TerminalColors {
    TerminalColors {
        palette: v["palette"].as_array().unwrap().iter().map(|c| c.as_str().map(str::to_owned)).collect(),
        default_foreground: v["defaultForeground"].as_str().map(str::to_owned),
        default_background: v["defaultBackground"].as_str().map(str::to_owned),
    }
}

#[test]
fn system_theme_matches_upstream_with_new_designs() {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    let paths = paths(&fixture);
    let mut failures = Vec::new();
    let mut checked = 0;
    for (key, expected) in fixture["system"].as_object().unwrap() {
        let (palette, want) = key.split_once('/').unwrap();
        let colors = colors_of(&fixture["palettes"][palette]);
        let terminal = terminal_mode(&colors).map(mode_name);
        assert_eq!(terminal, expected["terminal"].as_str(), "{key}: terminal mode");
        let mut m = ThemeManager::new(Some("system"), mode(want), Some(mode(want)));
        m.set_new_designs(true);
        m.set_system_palette(Some(colors));
        let doc = m.document("system").unwrap();
        let t = resolve::resolve_document(&doc, mode(want)).unwrap();
        compare(key, &resolve::modes(&doc), &t, expected, &paths, &mut failures);
        checked += 1;
    }
    report(failures, checked);
}

/// Mini parses without the experiment, so V1 themes (and the system theme) keep `migrateV1`.
#[test]
fn mini_keeps_the_classic_migration() {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    let paths = paths(&fixture);
    let mut failures = Vec::new();
    let mut checked = 0;
    let mut m = manager(false);
    for (key, expected) in fixture["mini"].as_object().unwrap() {
        let (name, want) = key.split_once('/').unwrap();
        let doc = if let Some(palette) = name.strip_prefix("system:") {
            let mut s = ThemeManager::new(Some("system"), mode(want), Some(mode(want)));
            s.set_system_palette(Some(colors_of(&fixture["palettes"][palette])));
            s.document("system").unwrap()
        } else {
            m.document(name).unwrap()
        };
        let t = resolve::resolve_document(&doc, mode(want)).unwrap();
        compare(key, &resolve::modes(&doc), &t, expected, &paths, &mut failures);
        checked += 1;
    }
    report(failures, checked);
}
