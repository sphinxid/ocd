//! Mini themes (`src/mini/theme.ts`): footer colors are exact theme colors; scrollback colors (entries,
//! blocks, syntax) are quantized to the nearest of the 256 palette colors in OKLab so immutable history
//! follows the terminal palette; mono uses terminal default colors only.

use super::cell::{Color, Intent, ansi256};
use crate::theme::{Mode, Theme};
use std::collections::HashMap;

/// A syntax style definition (`StyleDefinition`): unset properties do not override when merged.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct StyleDef {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: Option<bool>,
    pub italic: Option<bool>,
    pub underline: Option<bool>,
    pub dim: Option<bool>,
}

impl StyleDef {
    pub fn attrs(&self) -> u8 {
        use super::cell::attr::*;
        let mut a = 0;
        if self.bold == Some(true) {
            a |= BOLD;
        }
        if self.italic == Some(true) {
            a |= ITALIC;
        }
        if self.underline == Some(true) {
            a |= UNDERLINE;
        }
        if self.dim == Some(true) {
            a |= DIM;
        }
        a
    }
}

/// `SyntaxStyle` built by `generateSyntax(theme)`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MiniSyntax {
    pub styles: HashMap<String, StyleDef>,
}

impl MiniSyntax {
    /// `getStyle(group)`: exact lookup.
    pub fn get(&self, group: &str) -> Option<&StyleDef> {
        self.styles.get(group)
    }
    /// Lookup with the one-level base-scope fallback used by highlighting.
    pub fn resolve(&self, group: &str) -> Option<&StyleDef> {
        self.get(group).or_else(|| group.split_once('.').and_then(|(b, _)| self.get(b)))
    }
    pub fn default_style(&self) -> Option<&StyleDef> {
        self.get("default")
    }

    /// `generateSyntax(theme)` with every color passed through `map`.
    pub fn generate(t: &Theme, map: &dyn Fn(Color) -> Color) -> MiniSyntax {
        let mut styles = HashMap::new();
        let c = |x: crate::theme::Rgba| map(Color::from_theme(x));
        let mut rule = |scopes: &[&str], fg: Color, bold: bool, italic: bool, underline: bool, bg: Option<Color>| {
            for s in scopes {
                styles.insert(
                    s.to_string(),
                    StyleDef { fg: Some(fg), bg, bold: bold.then_some(true), italic: italic.then_some(true), underline: underline.then_some(true), dim: None },
                );
            }
        };
        let s = &t.syntax;
        let md = &t.markdown;
        let fb = &t.text.feedback;
        rule(&["default"], c(t.text.base), false, false, false, None);
        rule(&["prompt"], c(t.accent(200)), false, false, false, None);
        rule(&["extmark.file"], c(fb.warning.base), true, false, false, None);
        rule(&["extmark.agent"], c(t.categorical(0, 200)), true, false, false, None);
        let skill = if t.categorical.len() > 1 { t.categorical(1, 200) } else { t.categorical(0, 200) };
        rule(&["extmark.skill"], c(skill), true, false, false, None);
        rule(&["extmark.paste"], c(t.text.action.primary.focused), true, false, false, Some(c(fb.warning.base)));
        rule(&["comment", "comment.documentation"], c(s.comment), false, true, false, None);
        rule(&["string", "symbol", "character.special", "character"], c(s.string), false, false, false, None);
        rule(&["number", "boolean", "constant", "float"], c(s.number), false, false, false, None);
        rule(&["keyword.return", "keyword.conditional", "keyword.repeat", "keyword.coroutine"], c(s.keyword), false, true, false, None);
        rule(&["keyword.type"], c(s.type_), true, true, false, None);
        rule(&["keyword.function", "function.method"], c(s.function), false, false, false, None);
        rule(&["keyword"], c(s.keyword), false, true, false, None);
        rule(&["keyword.import", "string.escape", "string.regexp", "tag.attribute", "keyword.export"], c(s.keyword), false, false, false, None);
        rule(&["operator", "keyword.operator", "punctuation.delimiter", "keyword.conditional.ternary"], c(s.operator), false, false, false, None);
        rule(&["variable", "variable.parameter", "function.method.call", "function.call", "property", "parameter", "field"], c(s.variable), false, false, false, None);
        rule(&["variable.member", "function", "constructor"], c(s.function), false, false, false, None);
        rule(&["type", "module", "class", "namespace"], c(s.type_), false, false, false, None);
        rule(&["type.definition"], c(s.type_), true, false, false, None);
        rule(&["punctuation", "punctuation.bracket"], c(s.punctuation), false, false, false, None);
        rule(&["variable.builtin", "type.builtin", "function.builtin", "module.builtin", "constant.builtin", "variable.super"], c(fb.error.base), false, false, false, None);
        rule(&["keyword.directive", "keyword.modifier", "keyword.exception"], c(s.keyword), false, true, false, None);
        rule(&["punctuation.special", "tag.delimiter"], c(s.operator), false, false, false, None);
        rule(&["markup.heading", "markup.heading.2", "markup.heading.3", "markup.heading.4", "markup.heading.5", "markup.heading.6"], c(md.heading), true, false, false, None);
        rule(&["markup.heading.1"], c(md.heading), true, false, true, None);
        rule(&["markup.bold", "markup.strong"], c(md.strong), true, false, false, None);
        rule(&["markup.italic"], c(md.emphasis), false, true, false, None);
        rule(&["markup.list"], c(md.list_item), false, false, false, None);
        rule(&["markup.quote"], c(md.block_quote), false, true, false, None);
        rule(&["markup.raw", "markup.raw.block"], c(md.code), false, false, false, None);
        rule(&["markup.raw.inline"], c(md.code), false, false, false, Some(c(t.background.base)));
        rule(&["markup.link", "markup.link.url", "string.special", "string.special.url"], c(md.link), false, false, true, None);
        rule(&["markup.link.label"], c(md.link_text), false, false, true, None);
        rule(&["label"], c(md.link_text), false, false, false, None);
        rule(&["spell", "nospell"], c(t.text.base), false, false, false, None);
        rule(&["markup.underline"], c(t.text.base), false, false, true, None);
        rule(&["comment.error"], c(fb.error.base), true, true, false, None);
        rule(&["comment.warning"], c(fb.warning.base), true, true, false, None);
        rule(&["comment.todo", "comment.note"], c(fb.info.base), true, true, false, None);
        rule(&["attribute", "annotation"], c(fb.warning.base), false, false, false, None);
        rule(&["tag"], c(fb.error.base), false, false, false, None);
        rule(&["markup.strikethrough", "markup.list.unchecked", "debug"], c(t.text.muted), false, false, false, None);
        rule(&["markup.list.checked"], c(fb.success.base), false, false, false, None);
        rule(&["diff.plus"], c(t.diff.text_added), false, false, false, Some(c(t.diff.bg_added)));
        rule(&["diff.minus"], c(t.diff.text_removed), false, false, false, Some(c(t.diff.bg_removed)));
        rule(&["diff.delta"], c(t.diff.text_context), false, false, false, Some(c(t.diff.bg_context)));
        rule(&["error"], c(fb.error.base), true, false, false, None);
        rule(&["warning"], c(fb.warning.base), true, false, false, None);
        rule(&["info"], c(fb.info.base), false, false, false, None);
        MiniSyntax { styles }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct FooterTheme {
    pub action_secondary_text: Color,
    pub action_focused_bg: Color,
    pub action_focused_text: Color,
    pub formfield_text: Color,
    pub formfield_focused_bg: Color,
    pub formfield_focused_text: Color,
    pub selection: Color,
    pub running: Color,
    pub question: Color,
    pub permission: Color,
    pub success: Color,
    pub link: Color,
    pub categorical: Vec<Color>,
    pub warning: Color,
    pub error: Color,
    pub muted: Color,
    pub text: Color,
    pub shade: Color,
    pub surface: Color,
    pub pane: Color,
    pub border: Color,
    pub line: Color,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tone {
    pub body: Color,
    pub start: Option<Color>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EntryTheme {
    pub system: Tone,
    pub user: Tone,
    pub assistant: Tone,
    pub reasoning: Tone,
    pub tool: Tone,
    pub error: Tone,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SplashTheme {
    pub left: Color,
    pub right: Color,
    pub left_shadow: Color,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BlockTheme {
    pub text: Color,
    pub muted: Color,
    pub faint: Color,
    pub warning: Color,
    pub syntax: Option<MiniSyntax>,
    pub diff_removed: Color,
    pub diff_added_bg: Color,
    pub diff_removed_bg: Color,
    pub diff_context_bg: Color,
    pub diff_highlight_added: Color,
    pub diff_highlight_removed: Color,
    pub diff_line_number: Color,
    pub diff_added_line_number_bg: Color,
    pub diff_removed_line_number_bg: Color,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RunTheme {
    pub background: Color,
    pub footer: FooterTheme,
    pub entry: EntryTheme,
    pub splash: SplashTheme,
    pub block: BlockTheme,
}

fn srgb_to_linear(v: f64) -> f64 {
    if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
}

fn oklab(c: Color) -> (f64, f64, f64) {
    let r = srgb_to_linear(c.r as f64 / 255.0);
    let g = srgb_to_linear(c.g as f64 / 255.0);
    let b = srgb_to_linear(c.b as f64 / 255.0);
    let l = (0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b).cbrt();
    let m = (0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b).cbrt();
    let s = (0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b).cbrt();
    (0.2104542553 * l + 0.793617785 * m - 0.0040720468 * s, 1.9779984951 * l - 2.428592205 * m + 0.4505937099 * s, 0.0259040371 * l + 0.7827717662 * m - 0.808675766 * s)
}

/// The default 256-color palette as indexed colors.
pub fn ansi_palette() -> Vec<Color> {
    (0..=255u8).map(|i| Color::indexed(i, ansi256(i))).collect()
}

/// Palette from a terminal report (`RGBA.fromIndex(index, reported)`), defaults where unknown.
pub fn indexed_palette(colors: Option<&crate::theme::TerminalColors>) -> Vec<Color> {
    let base = ansi_palette();
    let Some(colors) = colors else { return base };
    base.into_iter()
        .enumerate()
        .map(|(i, c)| match colors.palette.get(i).and_then(|p| p.as_deref()).and_then(Color::from_hex) {
            Some(hex) => Color::indexed(i as u8, hex),
            None => c,
        })
        .collect()
}

/// `nearestIndexed(indexed, color)`.
pub fn nearest_indexed(indexed: &[Color], color: Color) -> Color {
    let (tl, ta, tb) = oklab(color);
    let mut best = indexed[0];
    let mut best_dist = f64::INFINITY;
    for item in indexed {
        let (l, a, b) = oklab(*item);
        let (dl, da, db) = (l - tl, a - ta, b - tb);
        let dist = dl * dl * 2.0 + da * da + db * db;
        if dist < best_dist {
            best_dist = dist;
            best = *item;
        }
    }
    best
}

/// `map(theme, indexed, syntax, system)`.
pub fn map(t: &Theme, indexed: &[Color], with_syntax: bool, system: bool) -> RunTheme {
    let base = Color::from_theme(t.text.base);
    let bgbase = Color::from_theme(t.background.base);
    let exact = |c: crate::theme::Rgba| {
        let c = Color::from_theme(c);
        if system && c.same_rgb(&base) {
            return Color::default_fg(c);
        }
        if system && c.same_rgb(&bgbase) {
            return Color::default_bg(c);
        }
        c
    };
    let exact_c = |c: Color| {
        if system && c.same_rgb(&base) {
            return Color::default_fg(c);
        }
        if system && c.same_rgb(&bgbase) {
            return Color::default_bg(c);
        }
        c
    };
    let scrollback_c = |c: Color| {
        let v = exact_c(c);
        if v.a == 0 || v.intent != Intent::Rgb { v } else { nearest_indexed(indexed, v) }
    };
    let sb = |c: crate::theme::Rgba| scrollback_c(Color::from_theme(c));
    let syntax = with_syntax.then(|| MiniSyntax::generate(t, &scrollback_c));
    let mut categorical: Vec<Color> = Vec::new();
    for i in 0..t.categorical.len() {
        let c = exact(t.categorical(i, 200));
        if !categorical.iter().any(|x| x.same_rgb(&c) && x.intent == c.intent) {
            categorical.push(c);
        }
    }
    let fb = &t.text.feedback;
    RunTheme {
        background: Color::default_bg(bgbase),
        footer: FooterTheme {
            action_secondary_text: exact(t.text.action.secondary.base),
            action_focused_bg: exact(t.background.action.primary.focused),
            action_focused_text: exact(t.text.action.primary.focused),
            formfield_text: exact(t.text.formfield.base),
            formfield_focused_bg: exact(t.background.formfield.focused),
            formfield_focused_text: exact(t.text.formfield.focused),
            selection: exact(t.text.formfield.selected),
            running: exact(t.interactive(200)),
            question: exact(t.accent(200)),
            permission: exact(t.accent(200)),
            success: exact(fb.success.base),
            link: exact(t.markdown.link),
            categorical,
            warning: exact(fb.warning.base),
            error: exact(fb.error.base),
            muted: exact(t.text.muted),
            text: exact(t.text.base),
            shade: exact(t.background.raised.base),
            surface: exact(t.background.raised.base),
            pane: exact(t.background.raised.high),
            border: exact(t.border),
            line: exact(t.background.raised.high),
        },
        entry: EntryTheme {
            system: Tone { body: sb(t.text.muted), start: None },
            user: Tone { body: sb(t.text.base), start: None },
            assistant: Tone { body: sb(t.markdown.text), start: None },
            reasoning: Tone { body: sb(t.text.muted), start: None },
            tool: Tone { body: sb(t.text.muted), start: Some(sb(t.text.muted)) },
            error: Tone { body: sb(fb.error.base), start: None },
        },
        splash: SplashTheme {
            left: nearest_indexed(indexed, Color::from_theme(t.text.muted)),
            right: nearest_indexed(indexed, Color::from_theme(t.text.base)),
            left_shadow: nearest_indexed(indexed, Color::from_theme(t.background.raised.base)),
        },
        block: BlockTheme {
            text: sb(t.text.base),
            muted: sb(t.text.muted),
            faint: sb(t.text.faint),
            warning: sb(fb.warning.base),
            syntax,
            diff_removed: sb(t.diff.text_removed),
            diff_added_bg: sb(t.diff.bg_added),
            diff_removed_bg: sb(t.diff.bg_removed),
            diff_context_bg: sb(t.diff.bg_context),
            diff_highlight_added: sb(t.diff.highlight_added),
            diff_highlight_removed: sb(t.diff.highlight_removed),
            diff_line_number: sb(t.diff.line_number_text),
            diff_added_line_number_bg: sb(t.diff.line_number_bg_added),
            diff_removed_line_number_bg: sb(t.diff.line_number_bg_removed),
        },
    }
}

/// `monoTheme(mode)`.
pub fn mono_theme(mode: Mode) -> RunTheme {
    let fg = Color::default_fg(if mode == Mode::Light { Color::rgb(0, 0, 0) } else { Color::rgb(255, 255, 255) });
    let bg = Color::default_bg(if mode == Mode::Light { Color::rgb(255, 255, 255) } else { Color::rgb(0, 0, 0) });
    let tone = Tone { body: fg, start: None };
    RunTheme {
        background: bg,
        footer: FooterTheme {
            action_secondary_text: fg,
            action_focused_bg: bg,
            action_focused_text: fg,
            formfield_text: fg,
            formfield_focused_bg: bg,
            formfield_focused_text: fg,
            selection: fg,
            running: fg,
            question: fg,
            permission: fg,
            success: fg,
            link: fg,
            categorical: vec![fg],
            warning: fg,
            error: fg,
            muted: fg,
            text: fg,
            shade: bg,
            surface: bg,
            pane: bg,
            border: fg,
            line: bg,
        },
        entry: EntryTheme { system: tone, user: tone, assistant: tone, reasoning: tone, tool: tone, error: tone },
        splash: SplashTheme { left: fg, right: fg, left_shadow: bg },
        block: BlockTheme {
            text: fg,
            muted: fg,
            faint: fg,
            warning: fg,
            syntax: None,
            diff_removed: fg,
            diff_added_bg: bg,
            diff_removed_bg: bg,
            diff_context_bg: bg,
            diff_highlight_added: fg,
            diff_highlight_removed: fg,
            diff_line_number: fg,
            diff_added_line_number_bg: bg,
            diff_removed_line_number_bg: bg,
        },
    }
}

/// `RUN_THEME_FALLBACK` (opencode theme, dark, default palette, no syntax).
pub fn fallback(mode: Mode) -> RunTheme {
    let doc: serde_json::Value = serde_json::from_str(crate::theme::OPENCODE_V2).unwrap();
    let t = crate::theme::resolve::resolve_document(&doc, mode).unwrap();
    map(&t, &ansi_palette(), false, false)
}

/// `resolveRunTheme(renderer, config, mono)` for an already resolved theme: quantize against the
/// terminal palette (when reported) and keep the terminal background.
pub fn resolve(t: &Theme, colors: Option<&crate::theme::TerminalColors>, system: bool) -> RunTheme {
    let indexed = indexed_palette(colors);
    let mut theme = map(t, &indexed, true, system);
    let bg = colors.and_then(|c| c.default_background.as_deref()).and_then(Color::from_hex).unwrap_or(Color::from_theme(t.background.base));
    theme.background = Color::default_bg(bg);
    theme
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantizes_scrollback_to_palette() {
        let theme = fallback(Mode::Dark);
        assert_eq!(theme.entry.user.body.intent, Intent::Indexed(255));
        assert_eq!(theme.entry.system.body.intent, Intent::Indexed(8));
        assert_eq!(theme.footer.text.intent, Intent::Rgb);
    }
}
