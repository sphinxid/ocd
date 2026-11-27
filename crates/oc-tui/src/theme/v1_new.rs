//! The new designs' V1 → V2 migration (`theme/src/tui/v1-migrate-new-designs.ts`): V1 colors laid out on the
//! `opencode` theme's steps (text at 100/200/300, the page at 900, raised surfaces at 700/600/500, each V1 color
//! anchored at 400 with quieter steps around it). Used for every V1 theme while `experimental["new-designs"]` is on.

use super::color::{Oklch, Rgba, oklch_to_hex, to_oklch};
use super::v1::{
    STEPS, V1Resolved, byte, detect_mode, dialog_background, palette_hues, palette_tokens, reference_hues, resolve_v1,
    selected_foreground,
};
use serde_json::{Map, Value, json};

/// `migrateV1NewDesigns` for a V1 JSON theme document.
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
            let def = migrate_mode(src, lm)?;
            if lm == "light" {
                let over = light_overrides(&def, light)?;
                return Ok(json!({ "base": base(def), "light": over }));
            }
            let hue = def["hue"].clone();
            return Ok(json!({ "base": base(def), "dark": { "hue": hue } }));
        }
    }
    let ld = migrate_mode(light, "light")?;
    let dd = migrate_mode(dark, "dark")?;
    let over = light_overrides(&ld, light)?;
    Ok(json!({ "base": base(ld), "light": over, "dark": dd }))
}

fn base(mut def: Value) -> Value {
    if let Some(o) = def.as_object_mut() {
        o.shift_remove("hue");
    }
    def
}

/// Light dialogs sit a step below the page like the opencode theme's, so rows can hover above them.
fn light_dialog() -> Value {
    json!({
        "background": {
            "base": "$hue.neutral.800",
            "raised": { "high": "$hue.neutral.700" },
            "action": {
                "primary": { "$hovered": "$hue.neutral.600" },
                "secondary": { "$hovered": "$hue.neutral.600" },
            },
        },
    })
}

fn light_overrides(def: &Value, t: &V1Resolved) -> anyhow::Result<Value> {
    let mut dialog = light_dialog();
    if t.get("backgroundPanel")?.a != 255 {
        dialog["background"]["base"] = Value::String(dialog_background(t)?);
    }
    Ok(json!({ "hue": def["hue"], "@dialog": dialog }))
}

fn migrate_mode(t: &V1Resolved, mode: &str) -> anyhow::Result<Value> {
    let color = |k: &str| -> anyhow::Result<Value> { Ok(Value::String(t.get(k)?.hex())) };
    let neutral = Neutral::new(t, mode)?;
    let primary = "$hue.interactive.400";
    let gray: Map<String, Value> =
        STEPS.iter().zip(neutral.scale.iter()).map(|(s, c)| (s.to_string(), Value::String(c.clone()))).collect();
    let (hue, categorical) = palette_hues(t, gray, |c| hue_scale(c, mode, &neutral))?;
    let destructive = selected_foreground(t, t.get("error")?)?.hex();
    let mut def = json!({
        "hue": hue,
        "categorical": categorical,
        "text": {
            "base": "$hue.neutral.100",
            "muted": "$hue.neutral.200",
            "faint": "$hue.neutral.300",
            "action": {
                "primary": { "base": "$text.base", "$disabled": "$text.faint", "$focused": "$text.base", "$selected": primary },
                "secondary": { "base": "$text.muted", "$hovered": "$text.base", "$focused": "$hue.interactive.200" },
                "destructive": { "base": destructive, "$disabled": "$text.faint" },
            },
            "formfield": {
                "base": "$text.base", "$hovered": primary, "$focused": primary, "$pressed": primary,
                "$disabled": "$text.faint", "$selected": primary,
            },
            "feedback": {
                "error": { "base": color("error")? },
                "warning": { "base": color("warning")? },
                "success": { "base": color("success")? },
                "info": { "base": color("info")? },
            },
        },
        "background": {
            "base": "$hue.neutral.900",
            "raised": { "base": "$hue.neutral.700", "high": "$hue.neutral.600", "max": "$hue.neutral.500" },
            "action": {
                "primary": {
                    "base": "transparent", "$hovered": "$hue.neutral.700",
                    "$focused": "$hue.interactive.600", "$selected": "transparent",
                },
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
    });
    if let (Some(o), Value::Object(p)) = (def.as_object_mut(), palette_tokens(t)?) {
        for (k, v) in p {
            o.insert(k, v);
        }
        o.insert(
            "@dialog".into(),
            json!({
                "background": {
                    "base": dialog_background(t)?,
                    "action": {
                        "primary": { "$hovered": "$background.raised.max" },
                        "secondary": { "$hovered": "$background.raised.max" },
                    },
                },
            }),
        );
    }
    Ok(reference_hues(def))
}

/// Step profiles measured from the opencode theme. Only lightness and chroma are taken from it; hues and anchors
/// always come from the V1 theme being migrated. Arrays are indexed by step (100 … 900).
struct Profile {
    /// Lightness of each surface step (400 … 900) above the page background.
    surface: [f64; 9],
    /// Where muted (200) and faint (300) text sit between the page (0) and the text color (1).
    text: [f64; 2],
    /// Lightness of each chromatic step relative to the neutral step of the same number (400 and 500 unused).
    hue_lightness: [f64; 9],
    /// Chroma of each chromatic step as a share of the anchor's chroma (400 unused).
    hue_chroma: [f64; 9],
    /// Step 500 sits this far from the anchor toward step 600.
    hue_mid: f64,
    /// Least share of the text-on-page contrast that text keeps on each background step (500 … 900).
    readable: [f64; 9],
    /// The same for chromatic steps 600 … 900.
    readable_hue: [f64; 9],
}

const DARK: Profile = Profile {
    surface: [0.0, 0.0, 0.0, 0.253, 0.175, 0.11, 0.057, 0.039, 0.0],
    text: [0.719, 0.538],
    hue_lightness: [-0.035, -0.023, -0.003, 0.0, 0.0, 0.095, 0.071, 0.024, 0.012],
    hue_chroma: [0.1, 0.24, 0.22, 0.0, 0.64, 0.2, 0.14, 0.09, 0.07],
    hue_mid: 0.55,
    readable: [0.0, 0.0, 0.0, 0.0, 0.66, 0.81, 0.92, 0.95, 0.99],
    readable_hue: [0.0, 0.0, 0.0, 0.0, 0.0, 0.7, 0.82, 0.92, 0.99],
};

const LIGHT: Profile = Profile {
    surface: [0.0, 0.0, 0.0, -0.227, -0.145, -0.102, -0.039, -0.015, 0.0],
    text: [0.688, 0.484],
    hue_lightness: [0.083, 0.13, 0.116, 0.0, 0.0, 0.021, -0.002, -0.004, -0.008],
    hue_chroma: [0.19, 0.12, 0.16, 0.0, 0.38, 0.17, 0.1, 0.05, 0.02],
    hue_mid: 0.65,
    readable: [0.0, 0.0, 0.0, 0.0, 0.64, 0.74, 0.89, 0.96, 0.97],
    readable_hue: [0.0, 0.0, 0.0, 0.0, 0.0, 0.78, 0.88, 0.93, 0.97],
};

fn profile(mode: &str) -> &'static Profile {
    if mode == "dark" { &DARK } else { &LIGHT }
}

fn idx(step: u32) -> usize {
    (step / 100 - 1) as usize
}

// Body text on surfaces and selections should stay comfortable to read even in low-contrast themes.
const MINIMUM_SURFACE_CONTRAST: f64 = 4.5;
const MAXIMUM_DIMMING: f64 = 0.6;

/// WCAG contrast ratio.
fn contrast(first: Rgba, second: Rgba) -> f64 {
    let luminance = |c: Rgba| {
        let channel = |v: u8| {
            let v = v as f64 / 255.0;
            if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * channel(c.r) + 0.7152 * channel(c.g) + 0.0722 * channel(c.b)
    };
    let (a, b) = (luminance(first), luminance(second));
    let (lighter, darker) = if a >= b { (a, b) } else { (b, a) };
    (lighter + 0.05) / (darker + 0.05)
}

fn mix(start: Oklch, end: Oklch, amount: f64) -> Oklch {
    let start_hue = if start.c > 0.002 { start.h } else { end.h };
    let end_hue = if end.c > 0.002 { end.h } else { start_hue };
    let hue = (((end_hue - start_hue) % 360.0 + 540.0) % 360.0) - 180.0;
    Oklch {
        l: start.l + (end.l - start.l) * amount,
        c: start.c + (end.c - start.c) * amount,
        h: start_hue + hue * amount,
    }
}

fn from_hex(s: &str) -> Rgba {
    Rgba::from_hex(s).unwrap_or_default()
}

/// The gray scale and the lightness of each step before readability dimming.
struct Neutral {
    scale: Vec<String>,
    lightness: [f64; 9],
    text: Rgba,
    page: Oklch,
    page_contrast: f64,
}

impl Neutral {
    fn new(t: &V1Resolved, mode: &str) -> anyhow::Result<Neutral> {
        let p = profile(mode);
        let opaque = |c: &Rgba| c.a > 0;
        let background = t.get("background")?;
        let panel = t.get("backgroundPanel")?;
        let element_raw = t.get("backgroundElement")?;
        let element = [Some(element_raw), t.colors.get("backgroundMenu").copied()].into_iter().flatten().find(opaque);
        let sources = [(900u32, Some(background)), (700, Some(panel)), (600, element)];
        let reference = sources.iter().find_map(|(step, c)| c.filter(opaque).map(|c| (*step, c)));
        let fallback = if mode == "dark" { Rgba::rgb(0, 0, 0) } else { Rgba::rgb(255, 255, 255) };
        let page = to_oklch(&reference.map_or(fallback, |(_, c)| c));
        let page_lightness = page.l - reference.map_or(0.0, |(step, _)| p.surface[idx(step)]);
        let tint = |colors: [Option<Rgba>; 3]| colors.into_iter().flatten().find(opaque).map_or(page, |c| to_oklch(&c));
        let surface_tint = |step: u32| match step {
            900 => tint([Some(background), Some(panel), element]),
            800 | 700 => tint([Some(panel), Some(background), element]),
            _ => tint([element, Some(panel), Some(background)]),
        };
        let text = t.get("text")?;
        let page_color = Oklch { l: page_lightness, ..surface_tint(900) };
        let page_contrast = contrast(text, from_hex(&oklch_to_hex(page_color)));
        let mut n = Neutral { scale: vec![String::new(); 9], lightness: [0.0; 9], text, page: page_color, page_contrast };
        let mut planned = [None; 9];
        for step in [800, 700, 600, 500, 400] {
            let generated = Oklch { l: page_lightness + p.surface[idx(step)], ..surface_tint(step) };
            let color = if step == 400 { generated } else { n.readable(generated, p.readable[idx(step)]) };
            planned[idx(step)] = Some(generated.l);
            n.lightness[idx(step)] = color.l;
            n.scale[idx(step)] = oklch_to_hex(color);
        }
        n.lightness[idx(900)] = page_lightness;
        n.scale[idx(900)] = background.hex();
        // Transparent V1 surfaces stay transparent so the terminal shows through, as they did before.
        for (step, c) in [(800, panel), (700, panel), (600, element_raw)] {
            if !opaque(&c) {
                n.scale[idx(step)] = c.hex();
            }
        }
        let text_l = to_oklch(&text);
        n.lightness[idx(100)] = text_l.l;
        n.scale[idx(100)] = text.hex();
        for (step, share) in [(200, p.text[0]), (300, p.text[1])] {
            let generated = mix(page_color, text_l, share);
            n.lightness[idx(step)] = generated.l;
            n.scale[idx(step)] = oklch_to_hex(generated);
        }
        // Keep the theme's own muted text on whichever text step it is closest to.
        let muted_color = t.get("textMuted")?;
        let muted = to_oklch(&muted_color);
        let (l200, l300) = (n.lightness[idx(200)], n.lightness[idx(300)]);
        let muted_step = if (muted.l - l200).abs() <= (muted.l - l300).abs() { 200 } else { 300 };
        n.lightness[idx(muted_step)] = muted.l;
        n.scale[idx(muted_step)] = muted_color.hex();
        for (slot, planned) in n.lightness.iter_mut().zip(planned) {
            if let Some(planned) = planned {
                *slot = planned;
            }
        }
        Ok(n)
    }

    /// Pulls a generated background back toward the page until text on it keeps enough contrast.
    fn readable(&self, color: Oklch, share: f64) -> Oklch {
        let target = (self.page_contrast * share).max(MINIMUM_SURFACE_CONTRAST.min(self.page_contrast * 0.95));
        let passes = |amount: f64| contrast(self.text, from_hex(&oklch_to_hex(mix(color, self.page, amount)))) >= target;
        if passes(0.0) {
            return color;
        }
        // Stop short of the page so surfaces and selections stay visible in low-contrast themes.
        if !passes(MAXIMUM_DIMMING) {
            return mix(color, self.page, MAXIMUM_DIMMING);
        }
        let (mut low, mut high) = (0.0, MAXIMUM_DIMMING);
        for _ in 0..12 {
            let amount = (low + high) / 2.0;
            if passes(amount) {
                high = amount;
            } else {
                low = amount;
            }
        }
        mix(color, self.page, high)
    }
}

fn hue_scale(color: &Rgba, mode: &str, neutral: &Neutral) -> Map<String, Value> {
    let p = profile(mode);
    let anchor = to_oklch(color);
    let lightness = |step: u32| neutral.lightness[idx(step)] + p.hue_lightness[idx(step)];
    let mut out = Map::new();
    for step in STEPS {
        let value = if step == 400 {
            color.hex()
        } else {
            let l = if step == 500 { anchor.l + (lightness(600) - anchor.l) * p.hue_mid } else { lightness(step) };
            let generated = Oklch { l, c: anchor.c * p.hue_chroma[idx(step)], h: anchor.h };
            let generated = if step >= 600 { neutral.readable(generated, p.readable_hue[idx(step)]) } else { generated };
            let hex = oklch_to_hex(generated);
            if color.a == 255 { hex } else { format!("{hex}{}", byte(color.a)) }
        };
        out.insert(step.to_string(), Value::String(value));
    }
    out
}
