use super::*;
use serde_json::{Value, json};

/// Recorded with `tests/golden/clack_prompts.mjs`: @clack/prompts 1.0.0-alpha.1 under Bun 1.4.4 with
/// `FORCE_COLOR=1`, piped output (no TTY rows, so `limitOptions` assumes 10), the bytes written
/// before the first key and after each one.
const GOLDEN: &str = include_str!("../../tests/golden/clack_prompts.json");

fn options(opts: &Value) -> Vec<Opt> {
    opts["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| Opt { value: o["value"].as_str().unwrap().into(), label: o["label"].as_str().unwrap_or(o["value"].as_str().unwrap()).into(), hint: o["hint"].as_str().map(String::from) })
        .collect()
}

fn required(v: Option<&str>) -> Option<String> {
    v.filter(|v| !v.is_empty()).is_none().then(|| "Required".to_string())
}

fn replay<K: Kind>(case: &Value, mut prompt: Prompt<K>, initial: Option<&str>) -> (Prompt<K>, Vec<String>) {
    let mut frames = vec![prompt.start(initial)];
    for k in case["keys"].as_array().unwrap() {
        let mut buf = k.as_str().unwrap().as_bytes().to_vec();
        let mut out = String::new();
        for key in parse_keys(&mut buf, true) {
            if !prompt.done() {
                out.push_str(&prompt.press(&key));
            }
        }
        frames.push(out);
    }
    frames.push(String::new());
    (prompt, frames)
}

fn check(case: &Value, frames: &[String], result: Value) {
    let expected: Vec<String> = case["frames"].as_array().unwrap().iter().map(|f| f.as_str().unwrap().to_string()).collect();
    for (i, (want, got)) in expected.iter().zip(frames).enumerate() {
        assert_eq!(got, want, "{} {:?}: frame {i} (after key {:?})", case["kind"], case["opts"]["message"], i.checked_sub(1).map(|k| &case["keys"][k]));
    }
    assert_eq!(frames.len(), expected.len());
    if let Some(r) = case.get("result") {
        assert_eq!(&result, r, "{} {:?}", case["kind"], case["opts"]["message"]);
    }
}

#[test]
fn prompts_match_recorded_clack_frames() {
    let cases: Value = serde_json::from_str(GOLDEN).unwrap();
    let pc = Pc { on: true };
    for case in cases.as_array().unwrap() {
        let opts = &case["opts"];
        let columns = case["columns"].as_u64().map(|c| c as usize);
        let validate: Option<Validate> = opts.get("_validate").map(|_| &required as Validate);
        match case["kind"].as_str().unwrap() {
            "multiselect" => {
                let initial: Vec<String> = opts["initialValues"].as_array().map(|a| a.iter().map(|v| v.as_str().unwrap().to_string()).collect()).unwrap_or_default();
                let kind = MultiSelect::new(opts["message"].as_str().unwrap(), options(opts), &initial, opts["required"].as_bool().unwrap_or(true), pc);
                let (p, frames) = replay(case, Prompt::new(kind, pc, columns, None), None);
                let result = if p.ctx.state == State::Cancel { json!("<cancel>") } else { json!(p.kind.value) };
                check(case, &frames, result);
            }
            "select" => {
                let kind = Select::new(opts["message"].as_str().unwrap(), options(opts), opts["initialValue"].as_str());
                let (p, frames) = replay(case, Prompt::new(kind, pc, columns, None), None);
                let result = if p.ctx.state == State::Cancel { json!("<cancel>") } else { json!(p.kind.value()) };
                check(case, &frames, result);
            }
            "text" => {
                let mut kind = Text::new(opts["message"].as_str().unwrap(), validate);
                kind.placeholder = opts["placeholder"].as_str().map(String::from);
                let (p, frames) = replay(case, Prompt::new(kind, pc, columns, None), opts["initialValue"].as_str());
                let result = if p.ctx.state == State::Cancel { json!("<cancel>") } else { json!(p.kind.value) };
                check(case, &frames, result);
            }
            "password" => {
                let kind = Password::new(opts["message"].as_str().unwrap(), validate);
                let (p, frames) = replay(case, Prompt::new(kind, pc, columns, None), None);
                let result = if p.ctx.state == State::Cancel { json!("<cancel>") } else { json!(p.kind.value) };
                check(case, &frames, result);
            }
            "confirm" => {
                let kind = Confirm::new(opts["message"].as_str().unwrap(), opts["initialValue"].as_bool().unwrap_or(true));
                let (p, frames) = replay(case, Prompt::new(kind, pc, columns, None), None);
                let result = if p.ctx.state == State::Cancel { json!("<cancel>") } else { json!(p.kind.value) };
                check(case, &frames, result);
            }
            k => panic!("unknown kind {k}"),
        }
    }
}

/// Recorded with `tests/golden/clack_autocomplete.mjs`: clack's `autocomplete` and upstream's integration
/// picker under Bun 1.4.4, `FORCE_COLOR=1`, with the picker's `process.stdout.rows`/`columns` set per case.
const AUTOCOMPLETE: &str = include_str!("../../tests/golden/clack_autocomplete.json");

#[test]
fn autocomplete_and_integration_picker_match_recorded_frames() {
    let cases: Value = serde_json::from_str(AUTOCOMPLETE).unwrap();
    let pc = Pc { on: true };
    let shown = |state: State, value: Option<String>| match (state, value) {
        (State::Cancel, _) => json!("<cancel>"),
        (_, Some(v)) => json!(v),
        (_, None) => json!("<undefined>"),
    };
    for case in cases.as_array().unwrap() {
        let opts = &case["opts"];
        let columns = case["columns"].as_u64().map(|c| c as usize);
        let rows = case["rows"].as_u64().map(|c| c as usize);
        match case["kind"].as_str().unwrap() {
            "autocomplete" => {
                let kind = Autocomplete::new(opts["message"].as_str().unwrap(), options(opts), opts["maxItems"].as_u64().map(|m| m as usize));
                // limitOptions reads the (piped) output's rows: 10.
                let (p, frames) = replay(case, Prompt::new(kind, pc, columns, None), None);
                let result = shown(p.ctx.state, p.kind.state.value.clone());
                check(case, &frames, result);
            }
            "picker" => {
                let choices = opts["choices"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|c| IntegrationChoice {
                        value: c["value"].as_str().unwrap().into(),
                        label: c["label"].as_str().unwrap().into(),
                        category: c["category"].as_str().unwrap().into(),
                        connected: c["connected"].as_bool().unwrap(),
                        hint: c["hint"].as_str().map(String::from),
                    })
                    .collect();
                let kind = IntegrationPicker::new(choices, opts["kind"].as_str().unwrap());
                let (p, frames) = replay(case, Prompt::new(kind, pc, columns, rows), None);
                let result = shown(p.ctx.state, p.kind.state.value.clone());
                check(case, &frames, result);
            }
            k => panic!("unknown kind {k}"),
        }
    }
}

#[test]
fn resize_rerenders_at_the_new_width() {
    let pc = Pc { on: false };
    let opts = vec![Opt { value: "a".into(), label: "Alpha beta gamma".into(), hint: None }];
    let mut p = Prompt::new(Select::new("Pick", opts, None), pc, Some(40), None);
    p.start(None);
    // The old frame is measured at the new width (`restoreCursor`), then the changed lines redrawn.
    assert_eq!(p.resize(Some(10), None), "\x1b[999D\x1b[5A\x1b[2B\x1b[J│  ● Alpha\nbeta gamma\n└\n");
    assert_eq!(p.resize(Some(10), None), "");
}

#[test]
fn ctrl_d_on_an_empty_line_closes_readline() {
    let pc = Pc { on: false };
    let ctrl_d = Key { ctrl: true, ..Key::named("d", Some("\x04"), "\x04") };
    let mut p = Prompt::new(Text::new("Name", None), pc, None, None);
    p.start(Some("ab"));
    // With text in the line, ctrl+d deletes forward (nothing at the end).
    assert_eq!(p.press(&ctrl_d), "");
    assert!(!p.closed());
    let mut p = Prompt::new(Text::new("Name", None), pc, None, None);
    p.start(None);
    assert_eq!(p.press(&ctrl_d), "");
    assert!(p.closed() && !p.done());
    // A select's readline line collects typed characters too, so ctrl+d after a space doesn't close it.
    let opts = vec![Opt { value: "a".into(), label: "A".into(), hint: None }];
    let mut p = Prompt::new(MultiSelect::new("Pick", opts, &[], false, pc), pc, None, None);
    p.start(None);
    p.press(&Key::named("space", Some(" "), " "));
    p.press(&ctrl_d);
    assert!(!p.closed());
}

#[test]
fn wrap_normalises_to_nfc() {
    assert_eq!(wrap("e\u{301}", None), "\u{e9}");
}

#[test]
fn limit_options_follows_terminal_rows() {
    let pc = Pc { on: false };
    let opts: Vec<Opt> = (0..30).map(|i| Opt { value: format!("v{i}"), label: format!("Option {i}"), hint: None }).collect();
    let mut p = Prompt::new(Select::new("Pick", opts, Some("v20")), pc, None, Some(12));
    let first = p.start(None);
    // 12 rows leave 8 options, the window ending three below the cursor, ellipses at both ends.
    assert_eq!(
        first,
        "\x1b[?25l│\n◆  Pick\n│  ...\n│  ○ Option 16\n│  ○ Option 17\n│  ○ Option 18\n│  ○ Option 19\n│  ● Option 20\n│  ○ Option 21\n│  ...\n└\n"
    );
}

#[test]
fn colours_off_and_wrapping() {
    let pc = Pc { on: false };
    let opts = vec![Opt { value: "a".into(), label: "Alpha".into(), hint: Some("first".into()) }, Opt { value: "b".into(), label: "Beta".into(), hint: None }];
    let mut p = Prompt::new(MultiSelect::new("Pick", opts, &[], true, pc), pc, None, None);
    assert_eq!(p.start(None), "\x1b[?25l│\n◆  Pick\n│  ◻ Alpha (first)\n│  ◻ Beta\n└\n");
    let enter = Key::named("return", Some("\r"), "\r");
    assert_eq!(
        p.press(&enter),
        "\x1b[999D\x1b[5A\x1b[1B\x1b[J▲  Pick\n│  ◻ Alpha (first)\n│  ◻ Beta\n└  Please select at least one option.\nPress  space  to select,  enter  to submit\n"
    );
    assert_eq!(wrap("one two three four", Some(8)), "one two\nthree\nfour");
    assert_eq!(wrap("  lead and trail  \n   ", None), "lead and trail\n");
    assert_eq!(wrap("\x1b[36mabcdefghij\x1b[39m", Some(4)), "\x1b[36mabcd\x1b[39m\n\x1b[36mefgh\x1b[39m\n\x1b[36mij\x1b[39m");
}

#[test]
fn keypress_parsing() {
    let mut buf = b"\x1b[A\x1b[1;5C\x1bxa A\x03\r\x7f\x1b".to_vec();
    let keys = parse_keys(&mut buf, false);
    let names: Vec<_> = keys.iter().map(|k| (k.name.clone(), k.ch.clone(), k.ctrl, k.meta, k.shift)).collect();
    let n = |s: &str| Some(s.to_string());
    assert_eq!(
        names,
        vec![
            (n("up"), None, false, false, false),
            (n("right"), None, true, false, false),
            (n("x"), None, false, true, false),
            (n("a"), n("a"), false, false, false),
            (n("space"), n(" "), false, false, false),
            (n("a"), n("A"), false, false, true),
            (n("c"), n("\x03"), true, false, false),
            (n("return"), n("\r"), false, false, false),
            (n("backspace"), n("\x7f"), false, false, false),
        ]
    );
    // A lone ESC waits for the escape-code timeout before it counts as the escape key.
    assert_eq!(buf, b"\x1b");
    assert_eq!(parse_keys(&mut buf, true)[0].name.as_deref(), Some("escape"));
    let mut utf = "é".as_bytes().to_vec();
    assert_eq!(parse_keys(&mut utf, true)[0].ch.as_deref(), Some("é"));
}
