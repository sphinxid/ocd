//! V2 theme document handling: mode selection, merge, expansion and resolution
//! (`theme/src/tui/{select,expand,resolve}.ts`).

use super::color::Rgba;
use super::v1::STEPS;
use serde_json::{Map, Value};
use std::collections::HashMap;

const ACTION_VARIANTS: [&str; 3] = ["primary", "secondary", "destructive"];
const ACTION_STATES: [&str; 5] = ["disabled", "pressed", "focused", "selected", "hovered"];
const FEEDBACK: [&str; 4] = ["error", "warning", "success", "info"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Light,
    Dark,
}

impl Mode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Mode::Light => "light",
            Mode::Dark => "dark",
        }
    }
    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "light" => Some(Mode::Light),
            "dark" => Some(Mode::Dark),
            _ => None,
        }
    }
    pub fn other(&self) -> Mode {
        match self {
            Mode::Light => Mode::Dark,
            Mode::Dark => Mode::Light,
        }
    }
}

pub fn modes(doc: &Value) -> Vec<Mode> {
    let mut out = Vec::new();
    if doc.get("light").is_some_and(|v| !v.is_null()) {
        out.push(Mode::Light);
    }
    if doc.get("dark").is_some_and(|v| !v.is_null()) {
        out.push(Mode::Dark);
    }
    out
}

/// `mergeTheme`: recursive deep merge of plain objects; arrays/strings replace.
pub fn merge(a: &Value, b: &Value) -> Value {
    let mut out = match a {
        Value::Object(o) => o.clone(),
        _ => Map::new(),
    };
    if let Value::Object(bo) = b {
        for (k, v) in bo {
            if v.is_null() {
                continue;
            }
            let merged = if v.is_object() { merge(out.get(k).unwrap_or(&Value::Null), v) } else { v.clone() };
            out.insert(k.clone(), merged);
        }
    }
    Value::Object(out)
}

pub fn select(doc: &Value, mode: Mode) -> anyhow::Result<(Value, Mode)> {
    let ms = modes(doc);
    let selected = if ms.contains(&mode) { mode } else { *ms.first().ok_or_else(|| anyhow::anyhow!("Theme must provide at least one mode"))? };
    let def = merge(doc.get("base").unwrap_or(&Value::Null), &doc[selected.as_str()]);
    Ok((def, selected))
}

fn expand_actions(def: &mut Value, path: &str) {
    let Some(obj) = def.as_object_mut() else { return };
    for (variant, value) in obj.iter_mut() {
        let Some(v) = value.as_object_mut() else { continue };
        if !v.get("base").is_some_and(|b| !b.is_null()) {
            continue;
        }
        for state in ACTION_STATES {
            let key = format!("${state}");
            if v.get(&key).is_none_or(|x| x.is_null()) {
                v.insert(key, Value::String(format!("${path}.{variant}.base")));
            }
        }
    }
}

fn expand_formfield(def: &mut Value, path: &str) {
    let Some(v) = def.as_object_mut() else { return };
    if !v.get("base").is_some_and(|b| !b.is_null()) {
        return;
    }
    for state in ACTION_STATES {
        let key = format!("${state}");
        if v.get(&key).is_none_or(|x| x.is_null()) {
            v.insert(key, Value::String(format!("${path}.base")));
        }
    }
}

pub fn expand_tokens(def: &mut Value) {
    if let Some(text) = def.get_mut("text").filter(|t| t.is_object()) {
        let has_base = text.get("base").is_some_and(|b| !b.is_null());
        if text.get("muted").is_none_or(|m| m.is_null()) && has_base {
            text["muted"] = Value::String("$text.base".into());
        }
        if let Some(a) = text.get_mut("action") {
            expand_actions(a, "text.action");
        }
        if let Some(f) = text.get_mut("formfield") {
            expand_formfield(f, "text.formfield");
        }
        if let Some(Value::Object(fb)) = text.get_mut("feedback") {
            for (kind, f) in fb.iter_mut() {
                if let Some(fo) = f.as_object_mut()
                    && fo.get("muted").is_none_or(|m| m.is_null())
                    && fo.get("base").is_some_and(|b| !b.is_null())
                {
                    fo.insert("muted".into(), Value::String(format!("$text.feedback.{kind}.base")));
                }
            }
        }
    }
    if let Some(bg) = def.get_mut("background").filter(|t| t.is_object()) {
        if let Some(a) = bg.get_mut("action") {
            expand_actions(a, "background.action");
        }
        if let Some(f) = bg.get_mut("formfield") {
            expand_formfield(f, "background.formfield");
        }
    }
}

pub fn expand(def: &mut Value) {
    expand_tokens(def);
    if let Some(d) = def.get_mut("@dialog").filter(|d| d.is_object()) {
        expand_tokens(d);
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Stateful {
    pub base: Rgba,
    pub disabled: Rgba,
    pub pressed: Rgba,
    pub focused: Rgba,
    pub selected: Rgba,
    pub hovered: Rgba,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Feedback {
    pub base: Rgba,
    pub muted: Rgba,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct FeedbackSet {
    pub error: Feedback,
    pub warning: Feedback,
    pub success: Feedback,
    pub info: Feedback,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Actions {
    pub primary: Stateful,
    pub secondary: Stateful,
    pub destructive: Stateful,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TextTokens {
    pub base: Rgba,
    pub muted: Rgba,
    pub action: Actions,
    pub formfield: Stateful,
    pub feedback: FeedbackSet,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Raised {
    pub base: Rgba,
    pub high: Rgba,
    pub max: Rgba,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct BgFeedback {
    pub error: Rgba,
    pub warning: Rgba,
    pub success: Rgba,
    pub info: Rgba,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct BackgroundTokens {
    pub base: Rgba,
    pub raised: Raised,
    pub action: Actions,
    pub formfield: Stateful,
    pub feedback: BgFeedback,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DiffTokens {
    pub text_added: Rgba,
    pub text_removed: Rgba,
    pub text_context: Rgba,
    pub text_hunk_header: Rgba,
    pub bg_added: Rgba,
    pub bg_removed: Rgba,
    pub bg_context: Rgba,
    pub highlight_added: Rgba,
    pub highlight_removed: Rgba,
    pub line_number_text: Rgba,
    pub line_number_bg_added: Rgba,
    pub line_number_bg_removed: Rgba,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SyntaxTokens {
    pub comment: Rgba,
    pub keyword: Rgba,
    pub function: Rgba,
    pub variable: Rgba,
    pub string: Rgba,
    pub number: Rgba,
    pub type_: Rgba,
    pub operator: Rgba,
    pub punctuation: Rgba,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct MarkdownTokens {
    pub text: Rgba,
    pub heading: Rgba,
    pub link: Rgba,
    pub link_text: Rgba,
    pub code: Rgba,
    pub block_quote: Rgba,
    pub emphasis: Rgba,
    pub strong: Rgba,
    pub horizontal_rule: Rgba,
    pub list_item: Rgba,
    pub list_enumeration: Rgba,
    pub image: Rgba,
    pub image_text: Rgba,
    pub code_block: Rgba,
}

/// Fully resolved theme for one mode (+ dialog surface).
#[derive(Clone, Debug, Default)]
pub struct Theme {
    pub hues: Vec<(String, [Rgba; 9])>,
    pub categorical: Vec<[Rgba; 9]>,
    pub text: TextTokens,
    pub background: BackgroundTokens,
    pub border: Rgba,
    pub scrollbar: Rgba,
    pub diff: DiffTokens,
    pub syntax: SyntaxTokens,
    pub markdown: MarkdownTokens,
    pub dialog: Option<Box<Theme>>,
    pub mode: Option<Mode>,
}

impl Theme {
    pub fn hue(&self, name: &str, step: u32) -> Rgba {
        let pos = STEPS.iter().position(|s| *s == step).unwrap_or(1);
        self.hues.iter().find(|(n, _)| n == name).map(|(_, s)| s[pos]).unwrap_or_default()
    }
    pub fn accent(&self, step: u32) -> Rgba {
        self.hue("accent", step)
    }
    pub fn interactive(&self, step: u32) -> Rgba {
        self.hue("interactive", step)
    }
    pub fn categorical(&self, i: usize, step: u32) -> Rgba {
        let pos = STEPS.iter().position(|s| *s == step).unwrap_or(1);
        if self.categorical.is_empty() {
            return self.text.base;
        }
        self.categorical[i % self.categorical.len()][pos]
    }

    fn shift(&self, c: Rgba, amount: i64) -> Rgba {
        let Some((hue, pos)) = c.src else { return c };
        let Some((_, scale)) = self.hues.get(hue as usize) else { return c };
        let p = (pos as i64 + amount).clamp(0, 8) as usize;
        scale[p]
    }
    pub fn increase(&self, c: Rgba, n: i64) -> Rgba {
        self.shift(c, n)
    }
    pub fn decrease(&self, c: Rgba, n: i64) -> Rgba {
        self.shift(c, -n)
    }
    pub fn dec(&self, c: Rgba) -> Rgba {
        self.decrease(c, 1)
    }
    pub fn source(&self, c: Rgba) -> Option<(&str, u32)> {
        let (hue, pos) = c.src?;
        Some((self.hues.get(hue as usize)?.0.as_str(), STEPS[pos as usize]))
    }

    /// `surface("dialog")`: absolute — the dialog view returns itself.
    pub fn dialog(&self) -> &Theme {
        match &self.dialog {
            Some(d) => d,
            None => self,
        }
    }
}

struct Resolver<'a> {
    source: &'a Value,
    hues: &'a [(String, [Rgba; 9])],
    cache: HashMap<String, Rgba>,
}

impl<'a> Resolver<'a> {
    fn color(&mut self, value: &str, path: &str, stack: &mut Vec<String>) -> anyhow::Result<Rgba> {
        if value == "transparent" {
            return Ok(Rgba::TRANSPARENT);
        }
        if Rgba::is_hex(value) {
            return Ok(Rgba::from_hex(value).unwrap());
        }
        let Some(target) = value.strip_prefix('$') else {
            anyhow::bail!("Invalid color \"{value}\" at \"{path}\"");
        };
        if let Some(hit) = self.cache.get(target) {
            return Ok(*hit);
        }
        if stack.iter().any(|s| s == target) {
            let mut c = stack.clone();
            c.push(target.to_string());
            anyhow::bail!("Circular theme reference: {}", c.join(" -> "));
        }
        // `$hue.<name>.<step>` reads the resolved hue instance.
        if let Some(rest) = target.strip_prefix("hue.") {
            if let Some((name, step)) = rest.rsplit_once('.')
                && let Ok(step) = step.parse::<u32>()
                && STEPS.contains(&step)
            {
                let pos = STEPS.iter().position(|s| *s == step).unwrap();
                let Some((_, scale)) = self.hues.iter().find(|(n, _)| n == name) else {
                    anyhow::bail!("Theme reference \"{value}\" was not found");
                };
                let c = scale[pos];
                self.cache.insert(target.to_string(), c);
                return Ok(c);
            }
            anyhow::bail!("Theme reference \"{value}\" was not found");
        }
        let mut cur = self.source;
        for key in target.split('.') {
            match cur.get(key) {
                Some(v) => cur = v,
                None => anyhow::bail!("Theme reference \"{value}\" was not found"),
            }
        }
        let result = match cur {
            Value::String(s) => {
                stack.push(target.to_string());
                let r = self.color(&s.clone(), target, stack);
                stack.pop();
                r?
            }
            Value::Null => anyhow::bail!("Theme reference \"{value}\" was not found"),
            _ => anyhow::bail!("Theme reference \"{value}\" at \"{path}\" is not a color"),
        };
        self.cache.insert(target.to_string(), result);
        Ok(result)
    }

    fn walk(&mut self, value: &Value, path: &str, out: &mut HashMap<String, Rgba>) -> anyhow::Result<()> {
        match value {
            Value::String(s) => {
                let c = self.color(s, path, &mut Vec::new())?;
                out.insert(path.to_string(), c);
            }
            Value::Object(o) => {
                for (k, v) in o {
                    let key = k.strip_prefix('$').filter(|s| ACTION_STATES.contains(s)).unwrap_or(k);
                    let p = if path.is_empty() { key.to_string() } else { format!("{path}.{key}") };
                    self.walk(v, &p, out)?;
                }
            }
            Value::Number(_) | Value::Null | Value::Bool(_) => {}
            Value::Array(_) => anyhow::bail!("Invalid theme value at \"{path}\""),
        }
        Ok(())
    }
}

fn resolve_hues(def: &Value) -> anyhow::Result<Vec<(String, [Rgba; 9])>> {
    let Some(src) = def.get("hue").and_then(|h| h.as_object()) else {
        anyhow::bail!("Missing required semantic hues: accent, interactive, neutral");
    };
    let missing: Vec<&str> =
        ["accent", "interactive", "neutral"].into_iter().filter(|n| !src.contains_key(*n)).collect();
    if !missing.is_empty() {
        anyhow::bail!("Missing required semantic hues: {}", missing.join(", "));
    }
    let names: Vec<String> = src.keys().cloned().collect();
    let mut cache: HashMap<String, [Rgba; 9]> = HashMap::new();
    fn resolve(
        name: &str,
        src: &Map<String, Value>,
        cache: &mut HashMap<String, [Rgba; 9]>,
        stack: &mut Vec<String>,
    ) -> anyhow::Result<[Rgba; 9]> {
        if let Some(hit) = cache.get(name) {
            return Ok(*hit);
        }
        if stack.iter().any(|s| s == name) {
            let mut c = stack.clone();
            c.push(name.into());
            anyhow::bail!("Circular hue reference: {}", c.join(" -> "));
        }
        let value = src.get(name);
        let result = match value {
            Some(Value::String(s)) => {
                let Some(target) = s.strip_prefix("$hue.").filter(|t| !t.is_empty()) else {
                    anyhow::bail!("Hue alias \"{s}\" must reference a hue scale");
                };
                if !src.contains_key(target) {
                    anyhow::bail!("Hue alias \"{s}\" references a missing hue");
                }
                stack.push(name.into());
                let r = resolve(target, src, cache, stack)?;
                stack.pop();
                r
            }
            Some(Value::Object(o)) => {
                let mut out = [Rgba::default(); 9];
                for (i, step) in STEPS.iter().enumerate() {
                    let c = o.get(&step.to_string()).and_then(|v| v.as_str());
                    match c.and_then(|c| if Rgba::is_hex(c) { Rgba::from_hex(c) } else { None }) {
                        Some(c) => out[i] = c,
                        None => anyhow::bail!("Invalid hue color at \"hue.{name}.{step}\""),
                    }
                }
                for k in o.keys() {
                    if !k.parse::<u32>().is_ok_and(|s| STEPS.contains(&s)) {
                        anyhow::bail!("Unknown hue step at \"hue.{name}.{k}\"");
                    }
                }
                out
            }
            _ => anyhow::bail!("Hue \"{name}\" was not found"),
        };
        cache.insert(name.into(), result);
        Ok(result)
    }
    let mut out = Vec::new();
    for (idx, name) in names.iter().enumerate() {
        let mut scale = resolve(name, src, &mut cache, &mut Vec::new())?;
        for (pos, c) in scale.iter_mut().enumerate() {
            c.src = Some((idx as u16, pos as u8));
        }
        out.push((name.clone(), scale));
    }
    Ok(out)
}

fn tokens_of(def: &Value) -> Value {
    let mut out = Map::new();
    for k in ["text", "background", "border", "scrollbar", "diff", "syntax", "markdown"] {
        if let Some(v) = def.get(k) {
            out.insert(k.into(), v.clone());
        }
    }
    Value::Object(out)
}

fn contextual_actions(base: Option<&Value>, surface: Option<&Value>) -> Value {
    let mut out = Map::new();
    for variant in ACTION_VARIANTS {
        let bv = base.and_then(|b| b.get(variant));
        let sv = surface.and_then(|s| s.get(variant));
        let mut v = Map::new();
        for state in std::iter::once("base").chain(ACTION_STATES) {
            let key = if state == "base" { None } else { Some(format!("${state}")) };
            let pick = key
                .as_ref()
                .and_then(|k| sv.and_then(|s| s.get(k)))
                .or_else(|| sv.and_then(|s| s.get("base")))
                .or_else(|| key.as_ref().and_then(|k| bv.and_then(|b| b.get(k))))
                .or_else(|| bv.and_then(|b| b.get("base")));
            if let Some(p) = pick.filter(|p| !p.is_null()) {
                v.insert(key.unwrap_or_else(|| "base".into()), p.clone());
            }
        }
        out.insert(variant.into(), Value::Object(v));
    }
    Value::Object(out)
}

fn contextualize(base: &Value, over: &Value) -> Value {
    let mut result = merge(base, over);
    let ta = contextual_actions(base.pointer("/text/action"), over.pointer("/text/action"));
    let ba = contextual_actions(base.pointer("/background/action"), over.pointer("/background/action"));
    if let Some(t) = result.get_mut("text").and_then(|t| t.as_object_mut()) {
        t.insert("action".into(), ta);
    }
    if let Some(b) = result.get_mut("background").and_then(|t| t.as_object_mut()) {
        b.insert("action".into(), ba);
    }
    result
}

fn build_view(tokens: &Value, hues: &[(String, [Rgba; 9])], categorical: &[[Rgba; 9]]) -> anyhow::Result<Theme> {
    let mut source = tokens.clone();
    // `$hue.*` references are answered from the resolved hue table; the source's `hue` key is a marker.
    if let Some(o) = source.as_object_mut() {
        o.insert("hue".into(), Value::Object(Map::new()));
    }
    let mut r = Resolver { source: &source, hues, cache: HashMap::new() };
    let mut flat = HashMap::new();
    let toks = tokens_of(tokens);
    r.walk(&toks, "", &mut flat)?;
    let g = |p: &str| -> anyhow::Result<Rgba> {
        flat.get(p).copied().ok_or_else(|| anyhow::anyhow!("Invalid theme: theme value is an invalid value ({p})"))
    };
    let stateful = |p: &str| -> anyhow::Result<Stateful> {
        let base = g(&format!("{p}.base"))?;
        let st = |s: &str| flat.get(&format!("{p}.{s}")).copied().unwrap_or(base);
        Ok(Stateful {
            base,
            disabled: st("disabled"),
            pressed: st("pressed"),
            focused: st("focused"),
            selected: st("selected"),
            hovered: st("hovered"),
        })
    };
    let fb = |k: &str| -> anyhow::Result<Feedback> {
        let base = g(&format!("text.feedback.{k}.base"))?;
        Ok(Feedback { base, muted: flat.get(&format!("text.feedback.{k}.muted")).copied().unwrap_or(base) })
    };
    let _ = FEEDBACK;
    Ok(Theme {
        hues: hues.to_vec(),
        categorical: categorical.to_vec(),
        text: TextTokens {
            base: g("text.base")?,
            muted: g("text.muted")?,
            action: Actions {
                primary: stateful("text.action.primary")?,
                secondary: stateful("text.action.secondary")?,
                destructive: stateful("text.action.destructive")?,
            },
            formfield: stateful("text.formfield")?,
            feedback: FeedbackSet { error: fb("error")?, warning: fb("warning")?, success: fb("success")?, info: fb("info")? },
        },
        background: BackgroundTokens {
            base: g("background.base")?,
            raised: Raised {
                base: g("background.raised.base")?,
                high: g("background.raised.high")?,
                max: g("background.raised.max")?,
            },
            action: Actions {
                primary: stateful("background.action.primary")?,
                secondary: stateful("background.action.secondary")?,
                destructive: stateful("background.action.destructive")?,
            },
            formfield: stateful("background.formfield")?,
            feedback: BgFeedback {
                error: g("background.feedback.error.base")?,
                warning: g("background.feedback.warning.base")?,
                success: g("background.feedback.success.base")?,
                info: g("background.feedback.info.base")?,
            },
        },
        border: g("border.base")?,
        scrollbar: g("scrollbar.base")?,
        diff: DiffTokens {
            text_added: g("diff.text.added")?,
            text_removed: g("diff.text.removed")?,
            text_context: g("diff.text.context")?,
            text_hunk_header: g("diff.text.hunkHeader")?,
            bg_added: g("diff.background.added")?,
            bg_removed: g("diff.background.removed")?,
            bg_context: g("diff.background.context")?,
            highlight_added: g("diff.highlight.added")?,
            highlight_removed: g("diff.highlight.removed")?,
            line_number_text: g("diff.lineNumber.text")?,
            line_number_bg_added: g("diff.lineNumber.background.added")?,
            line_number_bg_removed: g("diff.lineNumber.background.removed")?,
        },
        syntax: SyntaxTokens {
            comment: g("syntax.comment")?,
            keyword: g("syntax.keyword")?,
            function: g("syntax.function")?,
            variable: g("syntax.variable")?,
            string: g("syntax.string")?,
            number: g("syntax.number")?,
            type_: g("syntax.type")?,
            operator: g("syntax.operator")?,
            punctuation: g("syntax.punctuation")?,
        },
        markdown: MarkdownTokens {
            text: g("markdown.text")?,
            heading: g("markdown.heading")?,
            link: g("markdown.link")?,
            link_text: g("markdown.linkText")?,
            code: g("markdown.code")?,
            block_quote: g("markdown.blockQuote")?,
            emphasis: g("markdown.emphasis")?,
            strong: g("markdown.strong")?,
            horizontal_rule: g("markdown.horizontalRule")?,
            list_item: g("markdown.listItem")?,
            list_enumeration: g("markdown.listEnumeration")?,
            image: g("markdown.image")?,
            image_text: g("markdown.imageText")?,
            code_block: g("markdown.codeBlock")?,
        },
        dialog: None,
        mode: None,
    })
}

/// Resolve an (unexpanded) merged theme definition.
pub fn resolve_definition(def: &Value) -> anyhow::Result<Theme> {
    let mut def = def.clone();
    expand(&mut def);
    let hues = resolve_hues(&def)?;
    let mut categorical = Vec::new();
    if let Some(cat) = def.get("categorical").and_then(|c| c.as_array()) {
        for name in cat {
            let name = name.as_str().unwrap_or_default();
            match hues.iter().find(|(n, _)| n == name) {
                Some((_, s)) => categorical.push(*s),
                None => anyhow::bail!("Categorical hue \"{name}\" was not found"),
            }
        }
    }
    let base = tokens_of(&def);
    let mut theme = build_view(&base, &hues, &categorical)?;
    let dialog_tokens = match def.get("@dialog").filter(|d| d.is_object()) {
        Some(over) => contextualize(&base, over),
        None => base.clone(),
    };
    let dialog = build_view(&dialog_tokens, &hues, &categorical)?;
    theme.dialog = Some(Box::new(dialog));
    Ok(theme)
}

/// Resolve a V2 document for a requested mode. Returns the theme with its effective mode.
pub fn resolve_document(doc: &Value, mode: Mode) -> anyhow::Result<Theme> {
    let (def, effective) = select(doc, mode)?;
    let mut t = resolve_definition(&def)?;
    t.mode = Some(effective);
    if let Some(d) = t.dialog.as_mut() {
        d.mode = Some(effective);
    }
    Ok(t)
}

/// `parseThemeDocument`: validates every mode and wraps errors.
pub fn parse_document(input: &Value, name: &str) -> anyhow::Result<Value> {
    let wrap = |e: anyhow::Error| {
        let msg = e.to_string();
        let re = regex::Regex::new(r#"got ("[^"]*"|\S+)"#).unwrap();
        let value = re.captures(&msg).map(|c| c[1].to_string()).unwrap_or_else(|| "value".into());
        anyhow::anyhow!("Invalid theme: {name} {value} is an invalid value")
    };
    if !input.get("base").is_some_and(|b| b.is_object()) {
        return Err(wrap(anyhow::anyhow!("missing base")));
    }
    let ms = modes(input);
    if ms.is_empty() {
        return Err(wrap(anyhow::anyhow!("no modes")));
    }
    for m in ms {
        resolve_document(input, m).map_err(wrap)?;
    }
    Ok(input.clone())
}
