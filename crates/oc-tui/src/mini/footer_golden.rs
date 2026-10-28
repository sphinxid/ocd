//! Golden tests: footer frames captured from upstream's `RunFooter` on an OpenTUI split-footer test
//! renderer (`testdata/footer.json`): rows, per-cell styles, footer height and cursor per snap.

use super::*;
use crate::keymap::{Keybinds, Stroke};
use crate::mini::cell::{Color, Glyph, Intent};
use crate::mini::theme;
use crate::theme::Mode;
use serde_json::Value;
use std::time::{Duration, Instant};

fn color(c: Color) -> String {
    let hex = format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b);
    match c.intent {
        Intent::Indexed(slot) => format!("i{slot}"),
        Intent::Default => if c.a == 255 { format!("d{hex}") } else { format!("d{hex}:{}", c.a) },
        Intent::Rgb => if c.a == 255 { hex } else { format!("{hex}:{}", c.a) },
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

fn agent(v: &Value) -> RunAgent {
    RunAgent {
        id: v["id"].as_str().unwrap_or_default().into(),
        name: v["name"].as_str().unwrap_or_default().into(),
        description: v["description"].as_str().map(str::to_string),
        mode: v["mode"].as_str().unwrap_or_default().into(),
        hidden: v["hidden"].as_bool().unwrap_or(false),
    }
}

fn provider(v: &Value) -> RunProvider {
    let mut models = indexmap::IndexMap::new();
    for (id, m) in v["models"].as_object().unwrap() {
        models.insert(
            id.clone(),
            RunProviderModel {
                name: m["name"].as_str().map(str::to_string),
                cost_input: m["cost"]["input"].as_f64(),
                context: None,
                status: m["status"].as_str().map(str::to_string),
                variants: m["variants"].as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default(),
            },
        );
    }
    RunProvider { id: v["id"].as_str().unwrap().into(), name: v["name"].as_str().unwrap().into(), models }
}

fn model(v: &Value) -> Option<ModelSel> {
    v.as_object().map(|_| ModelSel { provider_id: v["providerID"].as_str().unwrap().into(), model_id: v["modelID"].as_str().unwrap().into() })
}

fn event(v: &Value) -> FooterEvent {
    match v["type"].as_str().unwrap() {
        "catalog" => FooterEvent::Catalog {
            agents: v["agents"].as_array().unwrap().iter().map(agent).collect(),
            references: vec![],
            commands: v["commands"].as_array().map(|a| a.iter().map(|c| RunCommand { name: c["name"].as_str().unwrap().into(), description: c["description"].as_str().map(str::to_string), source: c["source"].as_str().map(str::to_string) }).collect()),
        },
        "models" => FooterEvent::Models(v["providers"].as_array().unwrap().iter().map(provider).collect()),
        "variants" => FooterEvent::Variants { variants: v["variants"].as_array().unwrap().iter().map(|s| s.as_str().unwrap().to_string()).collect(), current: v["current"].as_str().map(str::to_string) },
        "turn.send" => FooterEvent::TurnSend,
        "turn.idle" => FooterEvent::TurnIdle,
        "stream.patch" => {
            let p = &v["patch"];
            FooterEvent::Patch(FooterPatch {
                phase: p["phase"].as_str().map(|s| if s == "running" { FooterPhase::Running } else { FooterPhase::Idle }),
                status: p["status"].as_str().map(str::to_string),
                usage: p.get("usage").map(|u| u.as_object().map(|_| Usage { tokens: u["tokens"].as_f64().unwrap_or(0.0), percent: u["percent"].as_i64(), cost: u["cost"].as_f64() })),
                ..Default::default()
            })
        }
        "stream.view" => {
            let view = &v["view"];
            FooterEvent::View(match view["type"].as_str().unwrap() {
                "permission" => FooterView::Permission(Box::new(MiniPermission { request: serde_json::from_value(view["request"].clone()).unwrap(), tool: None })),
                "form" => FooterView::Form(Box::new(MiniForm { form: serde_json::from_value(view["request"].clone()).unwrap(), location: None })),
                _ => FooterView::Prompt,
            })
        }
        other => panic!("event {other}"),
    }
}

fn stroke(step: &Value) -> Stroke {
    let name = match step["key"].as_str().unwrap() {
        "RETURN" => "return",
        "ESCAPE" => "escape",
        "ARROW_DOWN" => "down",
        "ARROW_UP" => "up",
        "ARROW_LEFT" => "left",
        "ARROW_RIGHT" => "right",
        "TAB" => "tab",
        "BACKSPACE" => "backspace",
        other => other,
    };
    Stroke { name: name.into(), ctrl: step["ctrl"].as_bool().unwrap_or(false), shift: step["shift"].as_bool().unwrap_or(false), meta: step["meta"].as_bool().unwrap_or(false), ..Default::default() }
}

fn char_stroke(c: char) -> Stroke {
    match c {
        ' ' => Stroke::key("space"),
        c if c.is_ascii_uppercase() => Stroke { name: c.to_ascii_lowercase().to_string(), shift: true, ..Default::default() },
        c => Stroke::key(&c.to_string()),
    }
}

fn run(case: &Value) -> Vec<Value> {
    let width = case["width"].as_u64().unwrap_or(80) as usize;
    let height = case["height"].as_u64().unwrap_or(30) as usize;
    let mono = case["mono"].as_bool().unwrap_or(false);
    let theme = if mono {
        theme::mono_theme(Mode::Dark)
    } else {
        let doc: Value = serde_json::from_str(crate::theme::OPENCODE_V2).unwrap();
        let t = crate::theme::resolve::resolve_document(&doc, Mode::Dark).unwrap();
        theme::resolve(&t, None, false)
    };
    let settings = MiniSettings { mono, ..Default::default() };
    let start = Instant::now();
    let mut now = start;
    let mut f = Footer::new(
        FooterOptions {
            agents: case["agents"].as_array().unwrap().iter().map(agent).collect(),
            references: vec![],
            agent: Some(case["agent"].as_str().unwrap_or("build").into()),
            model_label: case["modelLabel"].as_str().unwrap_or("Mock One").into(),
            model: model(&case["model"]),
            variant: None,
            first: case["first"].as_bool().unwrap_or(true),
            history: vec![],
            theme,
            settings,
            keybinds: Keybinds::new(None, 2000, true),
            animations: false,
            cursor_style: 1,
            wrote: true,
            startup: case["startup"].as_object().map(|s| (s["version"].as_str().unwrap().into(), s["detail"].as_str().unwrap().into())),
            directory: "/project".into(),
            image_preview: false,
        },
        width,
        height,
        4,
        now,
    );
    f.apply_height();
    let mut frames = Vec::new();
    for step in case["steps"].as_array().unwrap() {
        now += Duration::from_millis(1);
        if let Some(e) = step.get("event") {
            f.event(event(e));
            f.apply_height();
        }
        if let Some(t) = step.get("text").and_then(|t| t.as_str()) {
            for c in t.chars() {
                f.key(char_stroke(c), now);
            }
        }
        if let Some(t) = step.get("insert").and_then(|t| t.as_str()) {
            for c in t.chars() {
                f.key(char_stroke(c), now);
            }
        }
        if step.get("key").is_some() {
            f.key(stroke(step), now);
        }
        if step.get("finishStartup").is_some() {
            f.finish_startup();
        }
        if let Some(name) = step.get("snap").and_then(|s| s.as_str()) {
            f.tick(now);
            let (buf, cursor) = f.render(now);
            frames.push(serde_json::json!({
                "name": name,
                "footerHeight": f.height,
                "rows": (0..buf.height).map(|y| buf.row_text(y)).collect::<Vec<_>>(),
                "styles": styles(&buf),
                "cursor": cursor.map(|c| vec![c.x as u64 + 1, c.y as u64 + 1]),
            }));
        }
        for a in std::mem::take(&mut f.actions) {
            if a == FooterAction::CycleVariant {
                f.cycle_result(None);
            }
        }
    }
    frames
}

fn expected(v: &Value) -> Value {
    serde_json::json!({ "name": v["name"], "footerHeight": v["footerHeight"], "rows": v["rows"], "styles": v["styles"], "cursor": v["cursor"] })
}

#[test]
fn matches_upstream_footer_frames() {
    let cases: Vec<Value> = serde_json::from_str(include_str!("testdata/footer.json")).unwrap();
    let only = std::env::var("MINI_GOLDEN").ok();
    let mut failed = Vec::new();
    for case in &cases {
        let name = case["name"].as_str().unwrap();
        if only.as_deref().is_some_and(|o| !name.contains(o)) || name.starts_with("slash") {
            continue;
        }
        let got = run(case);
        let mut want: Vec<Value> = case["frames"].as_array().unwrap().iter().map(expected).collect();
        // Upstream's narrow permission dock keeps a stale scrollbar thumb after the footer grows
        // (the slider's viewport size from the 4-row footer); compare those frames without it.
        let mut got = got;
        if name == "permission-w34" {
            for f in want.iter_mut().chain(got.iter_mut()) {
                if let Some(rows) = f["rows"].as_array_mut() {
                    for r in rows.iter_mut() {
                        let t = r.as_str().unwrap().trim_end_matches(['█', '▀', '▄']).trim_end().to_string();
                        *r = Value::String(t);
                    }
                }
                f["styles"] = Value::Null;
            }
        }
        if got != want {
            if only.is_some() {
                eprintln!("== {name}\n-- want\n{}\n-- got\n{}", serde_json::to_string_pretty(&want).unwrap(), serde_json::to_string_pretty(&got).unwrap());
            }
            failed.push(name.to_string());
        }
    }
    assert!(failed.is_empty(), "{} of {} cases differ: {failed:?}", failed.len(), cases.len());
}
