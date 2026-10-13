//! Theme system: built-in/custom theme registry, V1 migration, V2 resolution, the generated
//! `system` theme, syntax styles and light/dark mode handling (spec §6).

pub mod color;
pub mod resolve;
pub mod v1;

pub use color::{Rgba, tint};
pub use resolve::{Mode, Theme};

use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

macro_rules! builtin {
    ($($name:literal),* $(,)?) => {
        &[$(($name, include_str!(concat!("../../../../assets/themes/", $name, ".json")))),*]
    };
}

/// The 32 registered V1 built-ins (`tui/src/theme/v1.ts:37-70`).
pub const BUILTIN_V1: &[(&str, &str)] = builtin!(
    "aura",
    "ayu",
    "carbonfox",
    "catppuccin",
    "catppuccin-frappe",
    "catppuccin-macchiato",
    "cobalt2",
    "cursor",
    "dracula",
    "everforest",
    "flexoki",
    "github",
    "gruvbox",
    "kanagawa",
    "lucent-orng",
    "material",
    "matrix",
    "mercury",
    "monokai",
    "nightowl",
    "nord",
    "one-dark",
    "orng",
    "osaka-jade",
    "palenight",
    "rosepine",
    "solarized",
    "synthwave84",
    "tokyonight",
    "vercel",
    "vesper",
    "zenburn",
);

/// The V2 default theme (`assets/v2/opencode.json`).
pub const OPENCODE_V2: &str = include_str!("../../../../assets/themes/v2/opencode.json");

pub const DEFAULT_THEME: &str = "opencode";

/// Terminal palette as reported by OSC 4/10/11 queries.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TerminalColors {
    pub palette: Vec<Option<String>>,
    #[serde(rename = "defaultForeground", skip_serializing_if = "Option::is_none")]
    pub default_foreground: Option<String>,
    #[serde(rename = "defaultBackground", skip_serializing_if = "Option::is_none")]
    pub default_background: Option<String>,
}

impl TerminalColors {
    pub fn usable(&self) -> bool {
        let bg = self.default_background.is_some() || self.palette.first().is_some_and(|p| p.is_some());
        let fg = self.default_foreground.is_some() || self.palette.get(7).is_some_and(|p| p.is_some());
        bg && fg
    }
}

/// `terminalMode`: light when the default background is bright.
pub fn terminal_mode(colors: &TerminalColors) -> Option<Mode> {
    let bg = Rgba::from_hex(colors.default_background.as_deref()?)?;
    Some(if bg.luminance() > 0.5 { Mode::Light } else { Mode::Dark })
}

fn gray_scale(bg: Rgba, dark: bool) -> [Rgba; 13] {
    let mut out = [Rgba::default(); 13];
    let (r, g, b) = (bg.r as f64, bg.g as f64, bg.b as f64);
    let lum = 0.299 * r + 0.587 * g + 0.114 * b;
    for (i, slot) in out.iter_mut().enumerate().skip(1) {
        let f = i as f64 / 12.0;
        if dark && lum < 10.0 {
            let v = (f * 0.4 * 255.0).floor() as u8;
            *slot = Rgba::rgb(v, v, v);
            continue;
        }
        if !dark && lum > 245.0 {
            let v = (255.0 - f * 0.4 * 255.0).floor() as u8;
            *slot = Rgba::rgb(v, v, v);
            continue;
        }
        let next = if dark { lum + (255.0 - lum) * f * 0.4 } else { lum * (1.0 - f * 0.4) };
        let ratio = next / lum;
        let c = |x: f64| (x * ratio).clamp(0.0, 255.0).floor() as u8;
        *slot = Rgba::rgb(c(r), c(g), c(b));
    }
    out
}

fn muted_text(bg: Rgba, dark: bool) -> Rgba {
    let lum = 0.299 * bg.r as f64 + 0.587 * bg.g as f64 + 0.114 * bg.b as f64;
    let v = if dark {
        if lum < 10.0 { 180.0 } else { (160.0 + lum * 0.3).floor().min(200.0) }
    } else if lum > 245.0 {
        75.0
    } else {
        (100.0 - (255.0 - lum) * 0.2).floor().max(60.0)
    };
    Rgba::rgb(v as u8, v as u8, v as u8)
}

/// `generateSystem(colors, mode)` producing a resolved V1 theme.
pub fn generate_system(colors: &TerminalColors, mode: Mode) -> v1::V1Resolved {
    let pal = |i: usize| -> Option<Rgba> { colors.palette.get(i).cloned().flatten().and_then(|h| Rgba::from_hex(&h)) };
    let bg = colors.default_background.as_deref().and_then(Rgba::from_hex).or_else(|| pal(0)).unwrap_or_default();
    let fg = colors.default_foreground.as_deref().and_then(Rgba::from_hex).or_else(|| pal(7)).unwrap_or_default();
    let transparent = Rgba::rgba(bg.r, bg.g, bg.b, 0);
    let dark = mode == Mode::Dark;
    let col = |i: usize| pal(i).unwrap_or_else(|| color::ansi(i as i64));
    let grays = gray_scale(bg, dark);
    let muted = muted_text(bg, dark);
    let (red, green, yellow, blue, magenta, cyan) = (col(1), col(2), col(3), col(4), col(5), col(6));
    let (red_b, green_b) = (col(9), col(10));
    let a = if dark { 0.22 } else { 0.14 };
    let entries: Vec<(&str, Rgba)> = vec![
        ("primary", cyan),
        ("secondary", magenta),
        ("accent", cyan),
        ("error", red),
        ("warning", yellow),
        ("success", green),
        ("info", cyan),
        ("text", fg),
        ("textMuted", muted),
        ("selectedListItemText", bg),
        ("background", transparent),
        ("backgroundPanel", grays[2]),
        ("backgroundElement", grays[3]),
        ("backgroundMenu", grays[3]),
        ("borderSubtle", grays[6]),
        ("border", grays[7]),
        ("borderActive", grays[8]),
        ("diffAdded", green),
        ("diffRemoved", red),
        ("diffContext", grays[7]),
        ("diffHunkHeader", grays[7]),
        ("diffHighlightAdded", green_b),
        ("diffHighlightRemoved", red_b),
        ("diffAddedBg", tint(bg, green, a)),
        ("diffRemovedBg", tint(bg, red, a)),
        ("diffContextBg", grays[2]),
        ("diffLineNumber", muted),
        ("diffAddedLineNumberBg", tint(grays[2], green, a)),
        ("diffRemovedLineNumberBg", tint(grays[2], red, a)),
        ("markdownText", fg),
        ("markdownHeading", fg),
        ("markdownLink", blue),
        ("markdownLinkText", cyan),
        ("markdownCode", green),
        ("markdownBlockQuote", yellow),
        ("markdownEmph", yellow),
        ("markdownStrong", fg),
        ("markdownHorizontalRule", grays[7]),
        ("markdownListItem", blue),
        ("markdownListEnumeration", cyan),
        ("markdownImage", blue),
        ("markdownImageText", cyan),
        ("markdownCodeBlock", fg),
        ("syntaxComment", muted),
        ("syntaxKeyword", magenta),
        ("syntaxFunction", blue),
        ("syntaxVariable", fg),
        ("syntaxString", green),
        ("syntaxNumber", yellow),
        ("syntaxType", cyan),
        ("syntaxOperator", cyan),
        ("syntaxPunctuation", fg),
    ];
    v1::V1Resolved {
        colors: entries.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        has_selected_list_item_text: true,
    }
}

/// A parsed theme source: always normalized to a V2 document.
#[derive(Clone, Debug)]
pub enum Source {
    Json(Value),
    Generated(Value),
}

fn to_document(name: &str, value: &Value) -> anyhow::Result<Value> {
    if value.get("theme").is_some() {
        return v1::migrate_v1(value);
    }
    if value.get("base").is_some() {
        return resolve::parse_document(value, name);
    }
    anyhow::bail!("Invalid theme: {name} value is an invalid value")
}

/// Directories searched for custom themes: global config, then each `.opencode` from root to cwd.
pub fn config_directories(cwd: &Path) -> Vec<PathBuf> {
    let mut out = vec![crate::util::paths::config_dir()];
    let mut ancestors: Vec<&Path> = cwd.ancestors().collect();
    ancestors.reverse();
    for a in ancestors {
        let p = a.join(".opencode");
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// Discover `<dir>/themes/*.json` (later directories override earlier ones).
pub fn discover(cwd: &Path) -> anyhow::Result<BTreeMap<String, Value>> {
    let mut out = BTreeMap::new();
    for dir in config_directories(cwd) {
        let themes = dir.join("themes");
        let mut entries = match std::fs::read_dir(&themes) {
            Ok(e) => e.filter_map(|e| e.ok()).map(|e| e.path()).collect::<Vec<_>>(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound || e.kind() == std::io::ErrorKind::NotADirectory => {
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        entries.sort();
        for path in entries {
            if path.extension().and_then(|e| e.to_str()) != Some("json") || !path.is_file() {
                continue;
            }
            let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_string();
            let text = std::fs::read_to_string(&path)?;
            let value: Value = serde_json::from_str(&text)?;
            if value.get("theme").is_none() && value.get("base").is_none() {
                continue;
            }
            out.insert(name, value);
        }
    }
    Ok(out)
}

/// Runtime theme registry and the currently active resolved theme.
pub struct ThemeManager {
    pub active: String,
    /// The requested mode (`store.mode`).
    pub mode: Mode,
    pub lock: Option<Mode>,
    custom: BTreeMap<String, Value>,
    docs: HashMap<String, Value>,
    system: Option<TerminalColors>,
    pub current: Theme,
    /// Load errors waiting to be shown as toasts: (name, message).
    pub errors: Vec<(String, String)>,
    /// Bumped on every resolve of `current` (render caches key on it).
    pub epoch: u64,
}

impl ThemeManager {
    pub fn new(active: Option<&str>, mode: Mode, lock: Option<Mode>) -> Self {
        let mut m = ThemeManager {
            active: active.unwrap_or(DEFAULT_THEME).to_string(),
            mode: lock.unwrap_or(mode),
            lock,
            custom: BTreeMap::new(),
            docs: HashMap::new(),
            system: None,
            current: Theme::default(),
            errors: Vec::new(),
            epoch: 0,
        };
        m.reload();
        m
    }

    pub fn set_custom(&mut self, custom: BTreeMap<String, Value>) {
        self.custom = custom;
        self.docs.clear();
        self.reload();
    }

    pub fn set_system_palette(&mut self, colors: Option<TerminalColors>) {
        self.system = colors.filter(|c| c.usable());
        self.docs.remove("system");
        if self.active == "system" {
            self.reload();
        }
    }

    pub fn system_palette(&self) -> Option<&TerminalColors> {
        self.system.as_ref()
    }

    /// All registered names, sorted case-insensitively.
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = BUILTIN_V1.iter().map(|(n, _)| n.to_string()).collect();
        names.push(DEFAULT_THEME.into());
        for k in self.custom.keys() {
            if !names.contains(k) {
                names.push(k.clone());
            }
        }
        if !names.iter().any(|n| n == "system") {
            names.push("system".into());
        }
        names.sort_by_key(|a| a.to_lowercase());
        names
    }

    pub fn has(&self, name: &str) -> bool {
        self.names().iter().any(|n| n == name)
    }

    fn source_value(&self, name: &str) -> Option<Value> {
        if name == "system"
            && let Some(colors) = &self.system
        {
            let generated = generate_system(colors, self.mode);
            return v1::migrate_resolved(&generated, &generated).ok();
        }
        if let Some(v) = self.custom.get(name) {
            return Some(v.clone());
        }
        if name == DEFAULT_THEME || name == "system" {
            return serde_json::from_str(OPENCODE_V2).ok();
        }
        BUILTIN_V1.iter().find(|(n, _)| *n == name).and_then(|(_, s)| serde_json::from_str(s).ok())
    }

    pub fn document(&mut self, name: &str) -> anyhow::Result<Value> {
        if let Some(d) = self.docs.get(name) {
            return Ok(d.clone());
        }
        let Some(src) = self.source_value(name) else {
            anyhow::bail!("Theme not found: {name}");
        };
        let doc = if name == "system" && self.system.is_some() { src } else { to_document(name, &src)? };
        self.docs.insert(name.to_string(), doc.clone());
        Ok(doc)
    }

    pub fn modes_of(&mut self, name: &str) -> Vec<Mode> {
        self.document(name).map(|d| resolve::modes(&d)).unwrap_or_default()
    }

    pub fn supports(&mut self, mode: Mode) -> bool {
        let n = self.active.clone();
        self.modes_of(&n).contains(&mode)
    }

    /// Resolve the active theme; on failure record an error and fall back to `opencode`.
    pub fn reload(&mut self) {
        self.epoch += 1;
        let name = if self.has(&self.active) { self.active.clone() } else { DEFAULT_THEME.to_string() };
        match self.document(&name).and_then(|d| resolve::resolve_document(&d, self.mode)) {
            Ok(t) => self.current = t,
            Err(e) => {
                self.errors.push((name.clone(), e.to_string()));
                self.active = DEFAULT_THEME.into();
                let doc: Value = serde_json::from_str(OPENCODE_V2).expect("default theme");
                self.current = resolve::resolve_document(&doc, self.mode).expect("default theme resolves");
            }
        }
    }

    pub fn set(&mut self, name: &str) -> bool {
        if !self.has(name) {
            return false;
        }
        self.active = name.to_string();
        self.reload();
        true
    }

    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
        self.docs.remove("system");
        self.reload();
    }

    /// Effective mode of the active theme.
    pub fn effective_mode(&self) -> Mode {
        self.current.mode.unwrap_or(self.mode)
    }

    pub fn theme(&self) -> &Theme {
        &self.current
    }
}

/// One syntax style rule.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TokenStyle {
    pub fg: Option<Rgba>,
    pub bg: Option<Rgba>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

/// `generateSyntax` (`theme/src/tui/syntax.ts`).
pub struct SyntaxStyle {
    rules: HashMap<String, TokenStyle>,
    pub default: TokenStyle,
}

impl SyntaxStyle {
    pub fn from_theme(t: &Theme) -> SyntaxStyle {
        let mut rules: HashMap<String, TokenStyle> = HashMap::new();
        let mut rule = |scopes: &[&str], fg: Rgba, bold: bool, italic: bool, underline: bool, bg: Option<Rgba>| {
            for s in scopes {
                rules.insert(s.to_string(), TokenStyle { fg: Some(fg), bg, bold, italic, underline });
            }
        };
        let s = &t.syntax;
        let md = &t.markdown;
        let fb = &t.text.feedback;
        rule(&["default"], t.text.base, false, false, false, None);
        rule(&["prompt"], t.accent(200), false, false, false, None);
        rule(&["extmark.file"], fb.warning.base, true, false, false, None);
        rule(&["extmark.agent"], t.categorical(0, 200), true, false, false, None);
        let skill = if t.categorical.len() > 1 { t.categorical(1, 200) } else { t.categorical(0, 200) };
        rule(&["extmark.skill"], skill, true, false, false, None);
        rule(&["extmark.paste"], t.text.action.primary.focused, true, false, false, Some(fb.warning.base));
        rule(&["comment", "comment.documentation"], s.comment, false, true, false, None);
        rule(&["string", "symbol", "character.special", "character"], s.string, false, false, false, None);
        rule(&["number", "boolean", "constant", "float"], s.number, false, false, false, None);
        rule(&["keyword.return", "keyword.conditional", "keyword.repeat", "keyword.coroutine"], s.keyword, false, true, false, None);
        rule(&["keyword.type"], s.type_, true, true, false, None);
        rule(&["keyword.function", "function.method"], s.function, false, false, false, None);
        rule(&["keyword"], s.keyword, false, true, false, None);
        rule(&["keyword.import", "string.escape", "string.regexp", "tag.attribute", "keyword.export"], s.keyword, false, false, false, None);
        rule(&["operator", "keyword.operator", "punctuation.delimiter", "keyword.conditional.ternary"], s.operator, false, false, false, None);
        rule(
            &["variable", "variable.parameter", "function.method.call", "function.call", "property", "parameter", "field"],
            s.variable,
            false,
            false,
            false,
            None,
        );
        rule(&["variable.member", "function", "constructor"], s.function, false, false, false, None);
        rule(&["type", "module", "class", "namespace"], s.type_, false, false, false, None);
        rule(&["type.definition"], s.type_, true, false, false, None);
        rule(&["punctuation", "punctuation.bracket"], s.punctuation, false, false, false, None);
        rule(
            &["variable.builtin", "type.builtin", "function.builtin", "module.builtin", "constant.builtin", "variable.super"],
            fb.error.base,
            false,
            false,
            false,
            None,
        );
        rule(&["keyword.directive", "keyword.modifier", "keyword.exception"], s.keyword, false, true, false, None);
        rule(&["punctuation.special", "tag.delimiter"], s.operator, false, false, false, None);
        rule(
            &["markup.heading", "markup.heading.2", "markup.heading.3", "markup.heading.4", "markup.heading.5", "markup.heading.6"],
            md.heading,
            true,
            false,
            false,
            None,
        );
        rule(&["markup.heading.1"], md.heading, true, false, true, None);
        rule(&["markup.bold", "markup.strong"], md.strong, true, false, false, None);
        rule(&["markup.italic"], md.emphasis, false, true, false, None);
        rule(&["markup.list"], md.list_item, false, false, false, None);
        rule(&["markup.quote"], md.block_quote, false, true, false, None);
        rule(&["markup.raw", "markup.raw.block"], md.code, false, false, false, None);
        rule(&["markup.raw.inline"], md.code, false, false, false, Some(t.background.base));
        rule(&["markup.link", "markup.link.url", "string.special", "string.special.url"], md.link, false, false, true, None);
        rule(&["markup.link.label"], md.link_text, false, false, true, None);
        rule(&["label"], md.link_text, false, false, false, None);
        rule(&["spell", "nospell"], t.text.base, false, false, false, None);
        rule(&["markup.underline"], t.text.base, false, false, true, None);
        rule(&["comment.error"], fb.error.base, true, true, false, None);
        rule(&["comment.warning"], fb.warning.base, true, true, false, None);
        rule(&["comment.todo", "comment.note"], fb.info.base, true, true, false, None);
        rule(&["attribute", "annotation"], fb.warning.base, false, false, false, None);
        rule(&["tag"], fb.error.base, false, false, false, None);
        rule(&["markup.strikethrough", "markup.list.unchecked", "debug"], t.text.muted, false, false, false, None);
        rule(&["markup.list.checked"], fb.success.base, false, false, false, None);
        rule(&["diff.plus"], t.diff.text_added, false, false, false, Some(t.diff.bg_added));
        rule(&["diff.minus"], t.diff.text_removed, false, false, false, Some(t.diff.bg_removed));
        rule(&["diff.delta"], t.diff.text_context, false, false, false, Some(t.diff.bg_context));
        rule(&["error"], fb.error.base, true, false, false, None);
        rule(&["warning"], fb.warning.base, true, false, false, None);
        rule(&["info"], fb.info.base, false, false, false, None);
        let default = rules["default"];
        SyntaxStyle { rules, default }
    }

    /// Thinking blocks: every style with fg forced to `text.muted`.
    pub fn muted(t: &Theme) -> SyntaxStyle {
        let mut s = SyntaxStyle::from_theme(t);
        for r in s.rules.values_mut() {
            r.fg = Some(t.text.muted);
        }
        s.default.fg = Some(t.text.muted);
        s
    }

    /// Look up a scope with dotted-prefix fallback.
    pub fn get(&self, scope: &str) -> TokenStyle {
        let mut s = scope;
        loop {
            if let Some(r) = self.rules.get(s) {
                return *r;
            }
            match s.rfind('.') {
                Some(i) => s = &s[..i],
                None => return self.default,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opencode(mode: Mode) -> Theme {
        let doc: Value = serde_json::from_str(OPENCODE_V2).unwrap();
        resolve::resolve_document(&doc, mode).unwrap()
    }

    #[test]
    fn default_dark_values() {
        let t = opencode(Mode::Dark);
        assert_eq!(t.text.base.hex(), "#eeeeee");
        assert_eq!(t.text.muted.hex(), "#808080");
        assert_eq!(t.background.base.hex(), "#0a0a0a");
        assert_eq!(t.background.raised.base.hex(), "#141414");
        assert_eq!(t.background.raised.high.hex(), "#1e1e1e");
        assert_eq!(t.background.raised.max.hex(), "#4c4c4c");
        assert_eq!(t.text.action.primary.focused.hex(), "#0a0a0a");
        assert_eq!(t.text.action.primary.selected.hex(), "#fab283");
        assert_eq!(t.background.action.primary.focused.hex(), "#fab283");
        assert_eq!(t.background.action.primary.hovered.hex(), "#141414");
        assert_eq!(t.text.feedback.warning.base.hex(), "#f5a742");
        assert_eq!(t.border.hex(), "#484848");
        assert_eq!(t.diff.bg_context.hex(), "#141414");
        assert_eq!(t.syntax.keyword.hex(), "#9d7cd8");
        assert_eq!(t.dialog().background.base.hex(), "#141414");
        assert_eq!(t.dialog().background.action.primary.hovered.hex(), "#1e1e1e");
        assert_eq!(t.dialog().background.formfield.base.hex(), "#141414");
        // decrease(background.raised.base) = neutral.600
        assert_eq!(t.dec(t.background.raised.base).hex(), "#1e1e1e");
        assert_eq!(t.background.action.destructive.focused.hex(), "#e06c75");
    }

    #[test]
    fn default_light_values() {
        let t = opencode(Mode::Light);
        assert_eq!(t.text.base.hex(), "#1a1a1a");
        assert_eq!(t.background.base.hex(), "#ffffff");
        assert_eq!(t.text.action.primary.selected.hex(), "#3b7dd8");
        assert_eq!(t.dialog().background.base.hex(), "#fafafa");
        assert_eq!(t.syntax.type_.hex(), "#b0851f");
    }

    #[test]
    fn every_builtin_resolves() {
        let mut m = ThemeManager::new(None, Mode::Dark, None);
        for name in m.names() {
            let doc = m.document(&name).unwrap_or_else(|e| panic!("{name}: {e}"));
            for mode in resolve::modes(&doc) {
                resolve::resolve_document(&doc, mode).unwrap_or_else(|e| panic!("{name} {mode:?}: {e}"));
            }
        }
        assert_eq!(m.names().len(), 34);
    }

    #[test]
    fn single_mode_builtins() {
        let mut m = ThemeManager::new(None, Mode::Dark, None);
        for name in ["aura", "ayu", "catppuccin-frappe", "catppuccin-macchiato", "nightowl"] {
            assert_eq!(m.modes_of(name), vec![Mode::Dark], "{name}");
        }
        assert_eq!(m.modes_of("lucent-orng"), vec![Mode::Light, Mode::Dark]);
        assert_eq!(m.modes_of("tokyonight"), vec![Mode::Light, Mode::Dark]);
    }

    #[test]
    fn v1_migration_references_hues() {
        let src: Value = serde_json::from_str(BUILTIN_V1.iter().find(|(n, _)| *n == "tokyonight").unwrap().1).unwrap();
        let doc = v1::migrate_v1(&src).unwrap();
        // text references the neutral scale so decrease() works on it.
        assert_eq!(doc["dark"]["text"]["base"], "$hue.neutral.200");
        let t = resolve::resolve_document(&doc, Mode::Dark).unwrap();
        assert!(t.source(t.background.raised.base).is_some());
    }

    #[test]
    fn circular_v1_reference() {
        let src = serde_json::json!({"theme": {"primary": "a"}, "defs": {"a": "b", "b": "a"}});
        let err = v1::migrate_v1(&src).unwrap_err().to_string();
        assert!(err.starts_with("Circular color reference: a -> b -> a"), "{err}");
    }

    #[test]
    fn system_theme() {
        let colors = TerminalColors {
            palette: vec![
                Some("#000000".into()),
                Some("#cc0000".into()),
                Some("#00cc00".into()),
                Some("#cccc00".into()),
                Some("#0000cc".into()),
                Some("#cc00cc".into()),
                Some("#00cccc".into()),
                Some("#cccccc".into()),
            ],
            default_foreground: Some("#dddddd".into()),
            default_background: Some("#101010".into()),
        };
        assert_eq!(terminal_mode(&colors), Some(Mode::Dark));
        let mut m = ThemeManager::new(Some("system"), Mode::Dark, None);
        m.set_system_palette(Some(colors));
        assert_eq!(m.current.background.base.a, 0);
        assert_eq!(m.current.text.base.hex(), "#dddddd");
    }

    #[test]
    fn syntax_fallback() {
        let t = opencode(Mode::Dark);
        let s = SyntaxStyle::from_theme(&t);
        assert_eq!(s.get("keyword.return.foo").fg, Some(t.syntax.keyword));
        assert!(s.get("keyword.return").italic);
        assert_eq!(s.get("unknown.scope"), s.default);
    }
}
