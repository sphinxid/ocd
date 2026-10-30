//! V1 theme resolution and V1 → V2 migration (`theme/src/tui/v1-migrate.ts`).

use super::color::{Oklch, Rgba, ansi, oklch_to_hex, rgb_to_oklch, to_oklch};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

pub const STEPS: [u32; 9] = [100, 200, 300, 400, 500, 600, 700, 800, 900];
const CHROMATIC: [&str; 7] = ["red", "orange", "yellow", "green", "cyan", "blue", "purple"];
const CATEGORICAL_TOKENS: [&str; 7] = ["secondary", "accent", "success", "warning", "primary", "error", "info"];
const MIN_CHROMA: f64 = 0.03;
const LIGHT_THRESHOLD: f64 = 0.6;
const LIGHT_REFS: [&str; 7] = ["#fca5a5", "#fdba74", "#fde047", "#86efac", "#67e8f9", "#93c5fd", "#d8b4fe"];
const DARK_REFS: [&str; 7] = ["#b91c1c", "#c2410c", "#a16207", "#15803d", "#0e7490", "#1d4ed8", "#7e22ce"];

/// Every color key of a V1 theme.
pub const V1_KEYS: [&str; 52] = [
    "primary",
    "secondary",
    "accent",
    "error",
    "warning",
    "success",
    "info",
    "text",
    "textMuted",
    "selectedListItemText",
    "background",
    "backgroundPanel",
    "backgroundElement",
    "backgroundMenu",
    "border",
    "borderActive",
    "borderSubtle",
    "diffAdded",
    "diffRemoved",
    "diffContext",
    "diffHunkHeader",
    "diffHighlightAdded",
    "diffHighlightRemoved",
    "diffAddedBg",
    "diffRemovedBg",
    "diffContextBg",
    "diffLineNumber",
    "diffAddedLineNumberBg",
    "diffRemovedLineNumberBg",
    "markdownText",
    "markdownHeading",
    "markdownLink",
    "markdownLinkText",
    "markdownCode",
    "markdownBlockQuote",
    "markdownEmph",
    "markdownStrong",
    "markdownHorizontalRule",
    "markdownListItem",
    "markdownListEnumeration",
    "markdownImage",
    "markdownImageText",
    "markdownCodeBlock",
    "syntaxComment",
    "syntaxKeyword",
    "syntaxFunction",
    "syntaxVariable",
    "syntaxString",
    "syntaxNumber",
    "syntaxType",
    "syntaxOperator",
    "syntaxPunctuation",
];

/// A V1 theme with every color resolved for one mode.
#[derive(Clone, Debug, Default)]
pub struct V1Resolved {
    pub colors: BTreeMap<String, Rgba>,
    pub has_selected_list_item_text: bool,
}

impl V1Resolved {
    pub fn get(&self, key: &str) -> anyhow::Result<Rgba> {
        self.colors
            .get(key)
            .copied()
            .ok_or_else(|| anyhow::anyhow!("Cannot read properties of undefined (reading '{key}')"))
    }
}

pub fn resolve_v1(theme: &Value, mode: &str) -> anyhow::Result<V1Resolved> {
    let empty = Map::new();
    let defs = theme.get("defs").and_then(|d| d.as_object()).unwrap_or(&empty);
    let tokens = theme.get("theme").and_then(|t| t.as_object()).unwrap_or(&empty);

    fn resolve_color(
        value: &Value,
        chain: &mut Vec<String>,
        defs: &Map<String, Value>,
        tokens: &Map<String, Value>,
        mode: &str,
    ) -> anyhow::Result<Rgba> {
        match value {
            Value::String(s) => {
                if s == "transparent" || s == "none" {
                    return Ok(Rgba::TRANSPARENT);
                }
                if s.starts_with('#') {
                    return Ok(Rgba::from_hex(s).unwrap_or(Rgba::rgb(0, 0, 0)));
                }
                if chain.contains(s) {
                    let mut c = chain.clone();
                    c.push(s.clone());
                    anyhow::bail!("Circular color reference: {}", c.join(" -> "));
                }
                let next = defs.get(s).or_else(|| tokens.get(s));
                let Some(next) = next else {
                    anyhow::bail!("Color reference \"{s}\" not found in defs or theme");
                };
                chain.push(s.clone());
                let r = resolve_color(next, chain, defs, tokens, mode);
                chain.pop();
                r
            }
            Value::Number(n) => Ok(ansi(n.as_f64().unwrap_or(0.0) as i64)),
            Value::Object(o) => match o.get(mode) {
                Some(v) => resolve_color(v, chain, defs, tokens, mode),
                None => anyhow::bail!("Invalid V1 theme color"),
            },
            _ => anyhow::bail!("Invalid V1 theme color"),
        }
    }

    let mut out = V1Resolved::default();
    for (key, value) in tokens {
        if key == "selectedListItemText" || key == "backgroundMenu" || key == "thinkingOpacity" {
            continue;
        }
        let c = resolve_color(value, &mut Vec::new(), defs, tokens, mode)?;
        out.colors.insert(key.clone(), c);
    }
    out.has_selected_list_item_text = tokens.get("selectedListItemText").is_some();
    let selected = match tokens.get("selectedListItemText") {
        Some(v) => Some(resolve_color(v, &mut Vec::new(), defs, tokens, mode)?),
        None => out.colors.get("background").copied(),
    };
    if let Some(s) = selected {
        out.colors.insert("selectedListItemText".into(), s);
    }
    let menu = match tokens.get("backgroundMenu") {
        Some(v) if !v.is_null() && v != &Value::String(String::new()) => {
            Some(resolve_color(v, &mut Vec::new(), defs, tokens, mode)?)
        }
        _ => out.colors.get("backgroundElement").copied(),
    };
    if let Some(m) = menu {
        out.colors.insert("backgroundMenu".into(), m);
    }
    Ok(out)
}

fn detect_mode(t: &V1Resolved) -> anyhow::Result<&'static str> {
    Ok(if t.get("text")?.luminance() > t.get("background")?.luminance() { "dark" } else { "light" })
}

/// Migrate a V1 JSON theme document into a V2 `{base, light?, dark?}` document.
pub fn migrate_v1(theme: &Value) -> anyhow::Result<Value> {
    let light = resolve_v1(theme, "light")?;
    let dark = resolve_v1(theme, "dark")?;
    migrate_resolved(&light, &dark)
}

pub fn migrate_resolved(light: &V1Resolved, dark: &V1Resolved) -> anyhow::Result<Value> {
    let lb = light.get("background")?;
    let db = dark.get("background")?;
    if lb.a > 0 && db.a > 0 && lb.same_value(&db) {
        let lm = detect_mode(light)?;
        let dm = detect_mode(dark)?;
        if lm == dm {
            let src = if lm == "light" { light } else { dark };
            let mut def = migrate_mode(src, lm)?;
            let hue = def.as_object_mut().unwrap().remove("hue").unwrap();
            let mut doc = Map::new();
            doc.insert("base".into(), def);
            doc.insert(lm.into(), json!({ "hue": hue }));
            return Ok(Value::Object(doc));
        }
    }
    let mut ld = migrate_mode(light, "light")?;
    let dd = migrate_mode(dark, "dark")?;
    let lhue = ld.as_object_mut().unwrap().remove("hue").unwrap();
    Ok(json!({ "base": ld, "light": { "hue": lhue }, "dark": dd }))
}

fn selected_foreground(t: &V1Resolved, bg: Rgba) -> anyhow::Result<Rgba> {
    if t.has_selected_list_item_text {
        return t.get("selectedListItemText");
    }
    let background = t.get("background")?;
    if background.a != 0 {
        return Ok(background);
    }
    let (r, g, b, _) = bg.floats();
    Ok(if 0.299 * r + 0.587 * g + 0.114 * b > 0.5 { Rgba::rgb(0, 0, 0) } else { Rgba::rgb(255, 255, 255) })
}

struct Inferred {
    by_hue: BTreeMap<&'static str, (Rgba, f64)>,
    by_token: BTreeMap<&'static str, &'static str>,
}

fn hue_distance(a: f64, b: f64) -> f64 {
    let d = (a - b).abs();
    d.min(360.0 - d)
}

fn ambiguous(c: &Rgba, chroma: f64) -> bool {
    c.a == 0 || chroma < MIN_CHROMA
}

fn hue_angles(light: bool) -> [f64; 7] {
    let refs = if light { LIGHT_REFS } else { DARK_REFS };
    let mut out = [0.0; 7];
    for (i, r) in refs.iter().enumerate() {
        out[i] = to_oklch(&Rgba::from_hex(r).unwrap()).h;
    }
    out
}

fn infer_hue(c: &Rgba) -> Option<(&'static str, f64)> {
    let v = to_oklch(c);
    if ambiguous(c, v.c) {
        return None;
    }
    let angles = hue_angles(v.l >= LIGHT_THRESHOLD);
    let mut best: Option<(&'static str, f64)> = None;
    for (i, name) in CHROMATIC.iter().enumerate() {
        let d = hue_distance(v.h, angles[i]);
        // stable sort semantic: first minimum wins.
        if best.map(|b| d < b.1).unwrap_or(true) {
            best = Some((name, d));
        }
    }
    best
}

fn infer_hues(t: &V1Resolved) -> anyhow::Result<Inferred> {
    let mut res = Inferred { by_hue: BTreeMap::new(), by_token: BTreeMap::new() };
    for token in ["accent", "success", "warning", "primary", "error", "info", "secondary"] {
        let color = t.get(token)?;
        let Some((name, distance)) = infer_hue(&color) else { continue };
        let keep = matches!(res.by_hue.get(name), Some((_, d)) if *d <= distance);
        if !keep {
            res.by_hue.insert(name, (color, distance));
        }
        res.by_token.insert(token, name);
    }
    for token in ["accent", "primary"] {
        let color = t.get(token)?;
        let Some((name, distance)) = infer_hue(&color) else { continue };
        res.by_hue.insert(name, (color, distance));
        res.by_token.insert(token, name);
    }
    Ok(res)
}

fn byte(v: u8) -> String {
    format!("{v:02x}")
}

fn hue_scale(color: &Rgba, mode: &str) -> Map<String, Value> {
    let v = to_oklch(color);
    let endpoint = if mode == "light" { v.l.max(0.97) } else { v.l.min(0.18) };
    let alpha = color.a;
    let mut out = Map::new();
    for step in STEPS {
        if step == 200 {
            out.insert(step.to_string(), Value::String(color.hex()));
            continue;
        }
        let progress = (step as f64 - 200.0) / 700.0;
        let generated = oklch_to_hex(Oklch {
            l: v.l + (endpoint - v.l) * progress,
            c: v.c * (1.0 - progress * 0.5),
            h: v.h,
        });
        let s = if alpha == 255 { generated } else { format!("{generated}{}", byte(alpha)) };
        out.insert(step.to_string(), Value::String(s));
    }
    out
}

fn interpolate(first: &Rgba, second: &Rgba, amount: f64) -> String {
    let start = to_oklch(first);
    let end = to_oklch(second);
    let start_h = if start.h.is_finite() { start.h } else if end.h.is_finite() { end.h } else { 0.0 };
    let end_h = if end.h.is_finite() { end.h } else { start_h };
    let hue = ((((end_h - start_h) % 360.0) + 540.0) % 360.0) - 180.0;
    let generated = oklch_to_hex(Oklch {
        l: start.l + (end.l - start.l) * amount,
        c: start.c + (end.c - start.c) * amount,
        h: start_h + hue * amount,
    });
    let alpha = (super::color::js_round(first.a as f64 + (second.a as f64 - first.a as f64) * amount)).clamp(0.0, 255.0)
        as u8;
    if alpha == 255 { generated } else { format!("{generated}{}", byte(alpha)) }
}

fn neutral_scale(t: &V1Resolved) -> anyhow::Result<Map<String, Value>> {
    // Anchors after the 1000-step reversal: 200=text 400=textMuted 600=element 700=panel 800=background.
    let element = t.get("backgroundElement").or_else(|_| t.get("backgroundMenu"))?;
    let anchors: Vec<(u32, Rgba)> = vec![
        (200, t.get("text")?),
        (400, t.get("textMuted")?),
        (600, element),
        (700, t.get("backgroundPanel")?),
        (800, t.get("background")?),
    ];
    let mut out = Map::new();
    for step in STEPS {
        if let Some((_, c)) = anchors.iter().find(|(s, _)| *s == step) {
            out.insert(step.to_string(), Value::String(c.hex()));
            continue;
        }
        let first = anchors[0];
        let last = anchors[anchors.len() - 1];
        let (lower, upper) = if step < first.0 {
            (first, anchors[1])
        } else if step > last.0 {
            (anchors[anchors.len() - 2], last)
        } else {
            let lower = *anchors.iter().filter(|(s, _)| *s < step).last().unwrap();
            let upper = *anchors.iter().find(|(s, _)| *s > step).unwrap();
            (lower, upper)
        };
        let amount = (step as f64 - lower.0 as f64) / (upper.0 as f64 - lower.0 as f64);
        out.insert(step.to_string(), Value::String(interpolate(&lower.1, &upper.1, amount)));
    }
    Ok(out)
}

/// `dialogBackground`: dialogs float over the session, so a see-through panel would let the content
/// underneath show through them. Fall back to the first visible surface color, made opaque.
pub fn dialog_background(t: &V1Resolved) -> anyhow::Result<String> {
    if t.get("backgroundPanel")?.a == 255 {
        return Ok("$background.raised.base".into());
    }
    let mut visible = None;
    for k in ["backgroundPanel", "backgroundMenu", "backgroundElement", "background"] {
        let c = t.get(k)?;
        if c.a > 0 {
            visible = Some(c);
            break;
        }
    }
    Ok(match visible {
        Some(c) => format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b),
        None => (if t.get("text")?.luminance() > 0.5 { "#000000" } else { "#ffffff" }).to_string(),
    })
}

fn migrate_mode(t: &V1Resolved, mode: &str) -> anyhow::Result<Value> {
    let color = |k: &str| -> anyhow::Result<Value> { Ok(Value::String(t.get(k)?.hex())) };
    let selected = selected_foreground(t, t.get("primary")?)?.hex();
    let destructive = selected_foreground(t, t.get("error")?)?.hex();
    let hues = infer_hues(t)?;
    let mut categorical: Vec<&str> = Vec::new();
    for token in CATEGORICAL_TOKENS {
        if let Some(h) = hues.by_token.get(token)
            && !categorical.contains(h)
        {
            categorical.push(h);
        }
    }
    let text = "$hue.neutral.200";
    let text_muted = "$hue.neutral.400";
    let primary = "$hue.interactive.200";
    let background = "$hue.neutral.800";
    let panel = "$hue.neutral.700";
    let menu = "$hue.neutral.600";
    let raised_max = "$hue.neutral.500";

    let mut hue = Map::new();
    hue.insert("gray".into(), Value::Object(neutral_scale(t)?));
    for name in CHROMATIC {
        if let Some((c, _)) = hues.by_hue.get(name) {
            hue.insert(name.into(), Value::Object(hue_scale(c, mode)));
        }
    }
    hue.insert(
        "accent".into(),
        Value::String(hues.by_token.get("accent").map(|h| format!("$hue.{h}")).unwrap_or("$hue.gray".into())),
    );
    hue.insert(
        "interactive".into(),
        Value::String(hues.by_token.get("primary").map(|h| format!("$hue.{h}")).unwrap_or("$hue.gray".into())),
    );
    hue.insert("neutral".into(), Value::String("$hue.gray".into()));
    let categorical: Vec<Value> = if categorical.is_empty() {
        vec![Value::String("neutral".into())]
    } else {
        categorical.into_iter().map(|s| Value::String(s.into())).collect()
    };

    let def = json!({
        "hue": hue,
        "categorical": categorical,
        "text": {
            "base": text,
            "muted": text_muted,
            "action": {
                "primary": { "base": "$text.base", "$disabled": text_muted, "$focused": selected, "$selected": primary },
                "secondary": { "base": "$text.muted", "$hovered": "$text.base" },
                "destructive": { "base": destructive, "$disabled": text_muted },
            },
            "formfield": {
                "base": text, "$hovered": primary, "$focused": primary, "$pressed": primary,
                "$disabled": text_muted, "$selected": primary,
            },
            "feedback": {
                "error": { "base": color("error")? },
                "warning": { "base": color("warning")? },
                "success": { "base": color("success")? },
                "info": { "base": color("info")? },
            },
        },
        "background": {
            "base": background,
            "raised": { "base": panel, "high": menu, "max": raised_max },
            "action": {
                "primary": { "base": "transparent", "$hovered": panel, "$focused": primary, "$selected": "transparent" },
                "secondary": { "base": "transparent" },
                "destructive": { "base": color("error")? },
            },
            "formfield": { "base": "$background.base" },
            "feedback": {
                "error": { "base": "$background.base" },
                "warning": { "base": "$background.base" },
                "success": { "base": "$background.base" },
                "info": { "base": "$background.base" },
            },
        },
        "border": { "base": color("border")? },
        "scrollbar": { "base": color("borderActive")? },
        "diff": {
            "text": {
                "added": color("diffAdded")?, "removed": color("diffRemoved")?,
                "context": color("diffContext")?, "hunkHeader": color("diffHunkHeader")?,
            },
            "background": {
                "added": color("diffAddedBg")?, "removed": color("diffRemovedBg")?, "context": color("diffContextBg")?,
            },
            "highlight": { "added": color("diffHighlightAdded")?, "removed": color("diffHighlightRemoved")? },
            "lineNumber": {
                "text": color("diffLineNumber")?,
                "background": { "added": color("diffAddedLineNumberBg")?, "removed": color("diffRemovedLineNumberBg")? },
            },
        },
        "syntax": {
            "comment": color("syntaxComment")?, "keyword": color("syntaxKeyword")?, "function": color("syntaxFunction")?,
            "variable": color("syntaxVariable")?, "string": color("syntaxString")?, "number": color("syntaxNumber")?,
            "type": color("syntaxType")?, "operator": color("syntaxOperator")?, "punctuation": color("syntaxPunctuation")?,
        },
        "markdown": {
            "text": color("markdownText")?, "heading": color("markdownHeading")?, "link": color("markdownLink")?,
            "linkText": color("markdownLinkText")?, "code": color("markdownCode")?, "blockQuote": color("markdownBlockQuote")?,
            "emphasis": color("markdownEmph")?, "strong": color("markdownStrong")?,
            "horizontalRule": color("markdownHorizontalRule")?, "listItem": color("markdownListItem")?,
            "listEnumeration": color("markdownListEnumeration")?, "image": color("markdownImage")?,
            "imageText": color("markdownImageText")?, "codeBlock": color("markdownCodeBlock")?,
        },
        "@dialog": {
            "background": {
                "base": dialog_background(t)?,
                "action": { "primary": { "$hovered": "$background.raised.high" } },
            },
        },
    });
    Ok(reference_hues(def))
}

fn reference_hues(mut theme: Value) -> Value {
    let Some(hue) = theme.get("hue").and_then(|h| h.as_object()).cloned() else { return theme };
    fn resolve<'a>(name: &str, hue: &'a Map<String, Value>, chain: &mut Vec<String>) -> Option<&'a Map<String, Value>> {
        if chain.iter().any(|c| c == name) {
            return None;
        }
        let v = hue.get(name)?;
        match v {
            Value::Object(o) => Some(o),
            Value::String(s) => {
                let target = s.strip_prefix("$hue.")?;
                if target.contains('.') {
                    return None;
                }
                chain.push(name.to_string());
                resolve(target, hue, chain)
            }
            _ => None,
        }
    }
    let mut refs: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut index = |name: &str, overwrite: bool| {
        let Some(scale) = resolve(name, &hue, &mut Vec::new()) else { return };
        for step in STEPS {
            let Some(Value::String(color)) = scale.get(&step.to_string()) else { continue };
            let key = color.to_lowercase();
            if !overwrite && refs.contains_key(&key) {
                continue;
            }
            refs.insert(key, format!("$hue.{name}.{step}"));
        }
    };
    for name in CHROMATIC {
        index(name, false);
    }
    index("gray", false);
    index("accent", true);
    index("interactive", true);
    index("neutral", true);

    fn replace(v: &mut Value, refs: &std::collections::HashMap<String, String>) {
        match v {
            Value::String(s) => {
                if let Some(r) = refs.get(&s.to_lowercase()) {
                    *s = r.clone();
                }
            }
            Value::Object(o) => {
                for (_, item) in o.iter_mut() {
                    replace(item, refs);
                }
            }
            _ => {}
        }
    }
    if let Value::Object(o) = &mut theme {
        for (k, v) in o.iter_mut() {
            if k == "hue" || k == "categorical" {
                continue;
            }
            replace(v, &refs);
        }
    }
    theme
}

#[allow(dead_code)]
pub fn oklch_of_hex(hex: &str) -> Oklch {
    let c = Rgba::from_hex(hex).unwrap_or_default();
    rgb_to_oklch(c.r as f64 / 255.0, c.g as f64 / 255.0, c.b as f64 / 255.0)
}
