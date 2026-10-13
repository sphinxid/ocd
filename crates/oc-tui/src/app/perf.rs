//! Performance regression tests: a synthetic long session (1,500+ messages with long markdown,
//! code blocks, tool output and diffs) and a flood of streaming deltas.
//!
//! The fast tests run with `cargo test -p oc-tui`; the heavier benchmark is ignored:
//! `cargo test -p oc-tui --release -- --ignored --nocapture perf_bench`.

use super::*;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::json;
use std::time::Instant;

pub(crate) fn perf_app() -> App {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    perf_app_with(tx)
}

pub(crate) fn perf_app_with(tx: tokio::sync::mpsc::UnboundedSender<AppMsg>) -> App {
    let opts = TuiOptions { url: "http://127.0.0.1:1".into(), directory: "/work/proj".into(), version: "2.0.26".into(), channel: "test".into(), ..Default::default() };
    let cfg = crate::config::Config::from_value(json!({ "tabs": { "mode": "off" } }));
    let mut app = App::new(opts, cfg, tx);
    app.history = crate::state::History::default();
    app.stash = crate::state::Stash::default();
    app.models = crate::state::ModelPrefs::default();
    app.pins = crate::state::SessionPins::default();
    app.keyed = crate::state::KeyedStore { dir: std::env::temp_dir().join(format!("oc-tui-keyed-{}", crate::util::id_body(false))) };
    app.placeholder = 1;
    app.connection = crate::api::sse::ConnStatus::Connected;
    let loc = app.store.location_mut("/work/proj");
    loc.agents = Some(vec![
        AgentInfo { id: "build".into(), name: "build".into(), mode: "primary".into(), ..Default::default() },
        AgentInfo { id: "plan".into(), name: "plan".into(), mode: "primary".into(), ..Default::default() },
    ]);
    loc.models = Some(vec![ModelInfo {
        id: "claude-sonnet".into(),
        provider_id: "anthropic".into(),
        name: "Claude Sonnet 4.5".into(),
        limit: ModelLimit { context: 200000.0, output: 8000.0 },
        status: "active".into(),
        enabled: true,
        ..Default::default()
    }]);
    app.default_model = Some(ModelRef { id: "claude-sonnet".into(), provider_id: "anthropic".into(), variant: None });
    app
}

fn markdown(i: usize) -> String {
    format!(
        "## Step {i}: parser changes\n\nI looked at **`src/parser{i}.ts`** and found that the tokenizer drops the trailing \
         delimiter when the input ends with a comment. This is a fairly long paragraph so that wrapping has some work to do, \
         with `inline code`, *emphasis* and a [link](https://example.com/{i}).\n\n\
         - first point about the change number {i}\n- second point, a bit longer than the first one to force a wrap at narrow widths\n\
         - third point\n\n\
         ```rust\nfn parse_{i}(input: &str) -> Result<Vec<Token>, Error> {{\n    let mut out = Vec::new();\n    for (n, line) in input.lines().enumerate() {{\n        if line.trim().is_empty() {{ continue; }}\n        out.push(Token::new(n, line)?);\n    }}\n    Ok(out)\n}}\n```\n\n\
         | file | change |\n|---|---|\n| parser.ts | fix |\n| lexer.ts | test |\n\nDone with step {i}."
    )
}

fn patch(i: usize) -> String {
    let mut p = String::from("@@ -1,20 +1,22 @@\n");
    for k in 0..20 {
        if k % 5 == 2 {
            p.push_str(&format!("-    let old_{k} = compute({i}, {k});\n+    let new_{k} = compute_fast({i}, {k});\n"));
        } else {
            p.push_str(&format!("     line {k} of file {i} stays the same\n"));
        }
    }
    p
}

/// One turn: user prompt, an assistant step with tools, a final step with long markdown, idle.
fn turn(i: usize, base: i64) -> Vec<serde_json::Value> {
    let t = base + i as i64 * 10_000;
    let shell_out: String = (0..40).map(|k| format!("test {i}.{k} ... ok")).collect::<Vec<_>>().join("\n");
    let code: String = (0..30).map(|k| format!("export const value{k} = {k} * {i};")).collect::<Vec<_>>().join("\n");
    vec![
        json!({"id": format!("msg_{i:05}_a"), "type":"user", "text": format!("Please fix bug number {i} in the parser and add tests"), "time":{"created": t}}),
        json!({"id": format!("msg_{i:05}_b"), "type":"assistant","agent":"build","model":{"id":"claude-sonnet","providerID":"anthropic"},
            "time":{"created": t + 10, "streamed": t + 20, "completed": t + 3000},"finish":"tool-calls",
            "tokens":{"input":12000,"output":300,"reasoning":40,"cache":{"read":0,"write":0}},
            "content":[
                {"type":"reasoning","text": format!("**Inspecting parser {i}**\n\nLooking at the tokenizer and the tests."),"time":{"created": t + 10,"completed": t + 500}},
                {"type":"tool","id": format!("t{i}_1"),"name":"read","state":{"status":"completed","input":{"path": format!("/work/proj/src/parser{i}.ts")},"content":[{"type":"text","text":"x"}]},"time":{"created": t + 600,"completed": t + 700}},
                {"type":"tool","id": format!("t{i}_2"),"name":"grep","state":{"status":"completed","input":{"pattern":"tokenize"},"metadata":{"matches":3},"content":[{"type":"text","text":"x"}]},"time":{"created": t + 700,"completed": t + 800}},
                {"type":"tool","id": format!("t{i}_3"),"name":"bash","state":{"status":"completed","input":{"command":"bun test --filter parser"},"content":[{"type":"text","text": shell_out}]},"time":{"created": t + 800,"completed": t + 900}},
                {"type":"tool","id": format!("t{i}_4"),"name":"edit","state":{"status":"completed","input":{"path": format!("src/parser{i}.ts")},
                  "metadata":{"files":[{"file": format!("src/parser{i}.ts"),"status":"modified","additions":4,"deletions":4,"patch": patch(i)}]},
                  "content":[{"type":"text","text":"ok"}]},"time":{"created": t + 900,"completed": t + 1000}},
                {"type":"tool","id": format!("t{i}_5"),"name":"write","state":{"status":"completed","input":{"path": format!("src/gen{i}.ts"), "content": code},"content":[{"type":"text","text":"ok"}]},"time":{"created": t + 1000,"completed": t + 1100}}
            ]}),
        json!({"id": format!("msg_{i:05}_c"), "type":"assistant","agent":"build","model":{"id":"claude-sonnet","providerID":"anthropic"},
            "time":{"created": t + 3100, "streamed": t + 3200, "completed": t + 6000},"finish":"stop",
            "tokens":{"input":13000,"output":800,"reasoning":0,"cache":{"read":0,"write":0}},
            "content":[{"type":"text","text": markdown(i)}]}),
        json!({"id": format!("msg_{i:05}_d"), "type":"idle","outcome":"succeeded","time":{"created": t + 6100}}),
    ]
}

/// A session with `messages` messages (rounded up to whole turns of 4).
pub(crate) fn long_session(app: &mut App, messages: usize) -> String {
    let id = "ses_perf".to_string();
    app.store.remember(SessionInfo {
        id: id.clone(),
        title: Some("Long session".into()),
        agent: Some("build".into()),
        model: Some(ModelRef { id: "claude-sonnet".into(), provider_id: "anthropic".into(), variant: None }),
        location: LocationRef { directory: "/work/proj".into(), workspace_id: None },
        ..Default::default()
    });
    let turns = messages.div_ceil(4);
    let raw: Vec<serde_json::Value> = (0..turns).flat_map(|i| turn(i, 1_000_000)).collect();
    let mut msgs: Vec<Message> = serde_json::from_value(serde_json::Value::Array(raw)).unwrap();
    // `apply_messages` takes a newest-first page.
    msgs.reverse();
    app.store.apply_messages(&id, msgs, None);
    app.store.loaded.insert(format!("session.message:{id}"));
    app.store.loaded.insert(format!("session.pending:{id}"));
    app.route = Route::Session(id.clone());
    id
}

pub(crate) fn ev(kind: &str, data: serde_json::Value) -> Event {
    Event { id: format!("evt_{}", crate::util::id_body(false)), created: crate::util::locale::now_ms(), kind: kind.into(), data, ..Default::default() }
}

/// Start a streaming step on `sid`: execution started, step started, text started.
pub(crate) fn start_stream(app: &mut App, sid: &str) {
    app.on_event(ev("session.execution.started", json!({"sessionID": sid})));
    app.on_event(ev("session.step.started", json!({"sessionID": sid, "assistantMessageID": "msg_zz_live", "agent": "build", "model": {"id":"claude-sonnet","providerID":"anthropic"}})));
    app.on_event(ev("session.text.started", json!({"sessionID": sid, "assistantMessageID": "msg_zz_live", "ordinal": 0})));
}

pub(crate) fn delta(sid: &str, k: usize) -> Event {
    let text = if k % 25 == 24 { format!("word{k}.\n\n") } else { format!("word{k} ") };
    ev("session.text.delta", json!({"sessionID": sid, "assistantMessageID": "msg_zz_live", "ordinal": 0, "delta": text}))
}

pub(crate) fn draw(app: &mut App, term: &mut Terminal<TestBackend>) {
    term.draw(|f| app.draw(f)).unwrap();
}

/// Mean time of one frame drawn right after a single text delta, over `n` deltas. Rows above the
/// window are not rendered in the background here (budget 0), so this is the steady-state cost.
pub(crate) fn frame_after_delta_ms(messages: usize, n: usize) -> f64 {
    let mut app = perf_app();
    let sid = long_session(&mut app, messages);
    start_stream(&mut app, &sid);
    app.tcache.budget = Some(std::time::Duration::ZERO);
    let mut term = Terminal::new(TestBackend::new(140, 50)).unwrap();
    for k in 0..3 {
        app.on_event(delta(&sid, k));
        draw(&mut app, &mut term);
    }
    let start = Instant::now();
    for k in 3..3 + n {
        app.on_event(delta(&sid, k));
        draw(&mut app, &mut term);
    }
    start.elapsed().as_secs_f64() * 1000.0 / n as f64
}

/// Single-delta frame cost must not grow with the transcript: a 1,500-message session costs about
/// the same per frame as a 16-message one, and stays within a few milliseconds (release build;
/// debug builds get a proportionally larger budget). The first frame of a long session renders
/// only the rows the window shows.
#[test]
fn perf_frame_cost_independent_of_transcript_length() {
    let mut app = perf_app();
    let sid = long_session(&mut app, 1500);
    app.tcache.budget = Some(std::time::Duration::ZERO);
    let mut term = Terminal::new(TestBackend::new(140, 50)).unwrap();
    draw(&mut app, &mut term);
    assert!(app.tcache.stats.rendered <= 8, "first frame rendered {} rows", app.tcache.stats.rendered);
    assert!(!app.tcache.is_complete());
    let _ = sid;
    let small = frame_after_delta_ms(16, 20);
    let large = frame_after_delta_ms(1500, 20);
    let budget = if cfg!(debug_assertions) { 40.0 } else { 4.0 };
    eprintln!("frame after one delta: 16 msgs {small:.2} ms, 1500 msgs {large:.2} ms (budget {budget} ms)");
    assert!(large < budget, "frame after one delta with 1500 messages took {large:.2} ms (budget {budget} ms)");
    assert!(large < small * 3.0 + 1.0, "frame cost grows with transcript length: {small:.2} ms -> {large:.2} ms");
}

/// Heavier benchmark: full transcript render vs. size, per-frame cost after a delta, and event
/// throughput. Run with `--release -- --ignored --nocapture perf_bench`.
#[test]
#[ignore]
fn perf_bench() {
    for n in [100usize, 500, 1500, 3000] {
        // Cold highlight cache for each measurement.
        let mut app = perf_app();
        let sid = long_session(&mut app, n);
        crate::ui::highlight::clear_cache();
        let start = Instant::now();
        let (lines, _) = app.render_transcript_lines(&sid, 136);
        let full = start.elapsed().as_secs_f64() * 1000.0;
        // First frame of the session (lazy: only what the window shows).
        let mut app = perf_app();
        let sid = long_session(&mut app, n);
        app.tcache.budget = Some(std::time::Duration::ZERO);
        let mut term = Terminal::new(TestBackend::new(140, 50)).unwrap();
        crate::ui::highlight::clear_cache();
        let start = Instant::now();
        draw(&mut app, &mut term);
        let first = start.elapsed().as_secs_f64() * 1000.0;
        let start = Instant::now();
        app.transcript_complete();
        let rest = start.elapsed().as_secs_f64() * 1000.0;
        let _ = sid;
        let frame = frame_after_delta_ms(n, 50);
        eprintln!(
            "messages {n:5}: transcript {:6} lines | reference full render {full:8.1} ms | first frame {first:6.1} ms, rest in background {rest:8.1} ms | frame after delta {frame:6.3} ms",
            lines.len()
        );
    }
    // Event projection throughput (no drawing).
    let mut app = perf_app();
    let sid = long_session(&mut app, 1500);
    start_stream(&mut app, &sid);
    let events: Vec<Event> = (0..20_000).map(|k| delta(&sid, k)).collect();
    let start = Instant::now();
    for e in events {
        app.on_event(e);
    }
    let secs = start.elapsed().as_secs_f64();
    eprintln!("on_event throughput (1500 msgs): {:.0} deltas/s", 20_000.0 / secs);
}

/// Where a full render's time goes (one turn's pieces). `--ignored --nocapture perf_pieces`.
#[test]
#[ignore]
fn perf_pieces() {
    let app = perf_app();
    let t = app.theme().clone();
    let time = |label: &str, f: &mut dyn FnMut()| {
        f();
        let start = Instant::now();
        for _ in 0..20 {
            f();
        }
        eprintln!("{label:28} {:8.3} ms", start.elapsed().as_secs_f64() * 1000.0 / 20.0);
    };
    let md = markdown(7);
    let o = crate::ui::markdown::MdOpts { theme: &t, syntax: &app.syntax, conceal: true, fg: t.markdown.text, mono: false };
    time("markdown (long answer)", &mut || {
        std::hint::black_box(crate::ui::markdown::render(&md, 130, &o));
    });
    let code: String = (0..30).map(|k| format!("export const value{k} = {k} * 7;")).collect::<Vec<_>>().join("\n");
    time("highlight 30 ts lines", &mut || {
        std::hint::black_box(crate::ui::highlight::highlight(&code, "typescript", &app.syntax));
    });
    let rs = "fn parse(input: &str) -> Result<Vec<Token>, Error> {\n    let mut out = Vec::new();\n    Ok(out)\n}";
    time("highlight 4 rust lines", &mut || {
        std::hint::black_box(crate::ui::highlight::highlight(rs, "rust", &app.syntax));
    });
    let p = patch(7);
    let d = crate::ui::diff::DiffOpts { theme: &t, syntax: &app.syntax, filetype: "typescript", split: true, wrap: true };
    time("diff 20 lines (split)", &mut || {
        std::hint::black_box(crate::ui::diff::render(&crate::ui::diff::parse(&p), 130, &d));
    });
    let mut app = perf_app();
    let sid = long_session(&mut app, 4);
    time("whole turn (reference)", &mut || {
        std::hint::black_box(app.render_transcript_lines(&sid, 136));
    });
}

// ---- loop-level benchmark: key latency under an SSE flood --------------------------------------

/// A question dialog (form) for `sid`: ↓ moves the selected row (3 options + custom answer).
fn question(app: &mut App, sid: &str) {
    app.store.forms.insert(
        sid.to_string(),
        vec![(
            FormInfo {
                id: "frm_perf".into(),
                session_id: sid.to_string(),
                title: "Question".into(),
                fields: vec![json!({"key":"pick","type":"string","title":"Pick","description":"Which one?","options":[{"value":"a","label":"Alpha"},{"value":"b","label":"Beta"},{"value":"c","label":"Gamma"}],"custom":true})],
                ..Default::default()
            },
            None,
        )],
    );
}

/// The loop as it was before batching: one event per iteration, a frame after every event while
/// anything animates, and (emulating the old renderer) a full transcript render per frame.
async fn legacy_loop<S>(app: &mut App, term: &mut Terminal<TestBackend>, events: &mut S, rx: &mut tokio::sync::mpsc::UnboundedReceiver<AppMsg>, after: &mut dyn FnMut(&mut App), deadline: Instant)
where
    S: futures::Stream<Item = std::io::Result<crossterm::event::Event>> + Unpin,
{
    use futures::StreamExt;
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(40));
    loop {
        if app.dirty || app.animating() {
            if let Some(sid) = app.session_id().map(str::to_string) {
                let w = app.transcript_rect.width.max(1) as usize;
                std::hint::black_box(app.render_transcript_lines(&sid, w));
            }
            term.draw(|f| app.draw(f)).unwrap();
            app.dirty = false;
            after(app);
        }
        // The backlog may take hours to drain: stop at the deadline.
        if app.quit || Instant::now() >= deadline {
            break;
        }
        tokio::select! {
            ev = events.next() => match ev {
                Some(Ok(ev)) => super::event_loop::on_term(app, ev),
                _ => break,
            },
            msg = rx.recv() => match msg {
                Some(m) => super::event_loop::on_msg(app, m),
                None => break,
            },
            _ = tick.tick() => app.tick(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct FloodReport {
    pub sent: usize,
    pub applied: usize,
    pub frames: usize,
    pub keys: usize,
    /// Key-to-frame latencies of keys whose effect was drawn, in ms.
    pub latencies: Vec<f64>,
}

impl FloodReport {
    fn pct(&self, p: f64) -> f64 {
        let mut v = self.latencies.clone();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        if v.is_empty() { f64::NAN } else { v[((v.len() - 1) as f64 * p).round() as usize] }
    }
    fn max(&self) -> f64 {
        self.latencies.iter().cloned().fold(0.0, f64::max)
    }
}

/// Stream text deltas at `rate`/s into a `messages`-message session for `secs` while pressing ↓ in
/// a question dialog every 250 ms; report how long each key took to show up on screen.
pub(crate) async fn flood(messages: usize, rate: u64, secs: u64, legacy: bool) -> FloodReport {
    use crossterm::event::{Event as CEvent, KeyCode, KeyEvent, KeyModifiers};
    use std::sync::{Arc, Mutex};
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = perf_app_with(tx.clone());
    let sid = long_session(&mut app, messages);
    start_stream(&mut app, &sid);
    question(&mut app, &sid);
    let mut term = Terminal::new(TestBackend::new(140, 50)).unwrap();
    // Late in a long session every row has been rendered once already; keys are measured after.
    draw(&mut app, &mut term);
    if !legacy {
        app.transcript_complete();
    }
    let (key_tx, mut key_rx) = futures::channel::mpsc::unbounded::<std::io::Result<CEvent>>();
    let sent_keys: Arc<Mutex<Vec<Instant>>> = Arc::default();
    let sent = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (sk, sn, sid2) = (sent_keys.clone(), sent.clone(), sid.clone());
    let producer = tokio::spawn(async move {
        let start = Instant::now();
        let mut k = 0usize;
        let mut next_key = start + std::time::Duration::from_millis(250);
        let step = std::time::Duration::from_micros(1_000_000 / rate);
        let mut next = start;
        while start.elapsed().as_secs() < secs {
            next += step;
            tokio::time::sleep_until(next.into()).await;
            let _ = tx.send(AppMsg::Sse(crate::api::sse::SseMsg::Event(Box::new(delta(&sid2, k)))));
            k += 1;
            sn.store(k, std::sync::atomic::Ordering::Relaxed);
            if Instant::now() >= next_key {
                next_key += std::time::Duration::from_millis(250);
                sk.lock().unwrap().push(Instant::now());
                let _ = key_tx.unbounded_send(Ok(CEvent::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))));
            }
        }
        // Let the last key reach the screen, then stop.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let _ = tx.send(AppMsg::Apply(Box::new(|app: &mut App| app.quit = true)));
        key_tx
    });
    // (draw time, selected row) per frame.
    let frames: Arc<Mutex<Vec<(Instant, usize)>>> = Arc::default();
    let f2 = frames.clone();
    let mut after = move |app: &mut App| {
        let row = app.form_ui.as_ref().map(|u| u.row).unwrap_or(0);
        f2.lock().unwrap().push((Instant::now(), row));
    };
    if legacy {
        let deadline = Instant::now() + std::time::Duration::from_secs(secs + 2);
        legacy_loop(&mut app, &mut term, &mut key_rx, &mut rx, &mut after, deadline).await;
    } else {
        let mut hooks = super::event_loop::Screen { term: &mut term, after: Some(Box::new(after)) };
        super::event_loop::run(&mut app, &mut key_rx, &mut rx, &mut hooks).await.unwrap();
    }
    let _keep = producer.await.unwrap();
    // Deltas still queued when the loop stopped were not applied.
    while rx.try_recv().is_ok() {}
    // How many deltas were applied: one word per delta.
    let applied = app
        .store
        .messages(&sid)
        .iter()
        .rev()
        .find(|m| m.id == "msg_zz_live")
        .map(|m| m.content.iter().map(|c| if let Content::Text { text, .. } = c { text.split_whitespace().count() } else { 0 }).sum())
        .unwrap_or(0);
    let keys = sent_keys.lock().unwrap().clone();
    let frames = frames.lock().unwrap().clone();
    // Key i (0-based) moves the row to (i + 1) % 4; its latency is the first frame showing that row
    // after the key was sent (and after key i-1 was shown).
    let mut latencies = Vec::new();
    let mut fi = 0;
    for (i, at) in keys.iter().enumerate() {
        let want = (i + 1) % 4;
        while fi < frames.len() && (frames[fi].0 < *at || frames[fi].1 != want) {
            fi += 1;
        }
        if fi == frames.len() {
            break;
        }
        latencies.push((frames[fi].0 - *at).as_secs_f64() * 1000.0);
    }
    FloodReport { sent: sent.load(std::sync::atomic::Ordering::Relaxed), applied, frames: frames.len(), keys: keys.len(), latencies }
}

/// Key latency and delta throughput under a 200/s flood, legacy loop vs. the new one.
/// Run with `--release -- --ignored --nocapture perf_flood_bench`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn perf_flood_bench() {
    for legacy in [true, false] {
        let r = flood(1500, 200, 6, legacy).await;
        eprintln!(
            "{} loop, 1500 msgs, 200 deltas/s for 6 s: applied {}/{} deltas, {} frames, keys shown {}/{}, latency p50 {:.0} ms p95 {:.0} ms max {:.0} ms",
            if legacy { "legacy" } else { "new" },
            r.applied,
            r.sent,
            r.frames,
            r.latencies.len(),
            r.keys,
            r.pct(0.5),
            r.pct(0.95),
            r.max()
        );
    }
}

/// The new loop applies every delta of a flood and shows every key promptly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn perf_flood_keys_responsive() {
    let r = flood(if cfg!(debug_assertions) { 40 } else { 400 }, 200, 3, false).await;
    eprintln!("flood: applied {}/{}, {} frames, keys {}/{}, p95 {:.0} ms, max {:.0} ms", r.applied, r.sent, r.frames, r.latencies.len(), r.keys, r.pct(0.95), r.max());
    assert!(r.applied >= r.sent, "deltas fell behind: applied {} of {}", r.applied, r.sent);
    assert_eq!(r.latencies.len(), r.keys, "every key must show up: {r:?}");
    let budget = if cfg!(debug_assertions) { 500.0 } else { 100.0 };
    assert!(r.pct(0.95) < budget, "key latency p95 {:.0} ms (budget {budget} ms)", r.pct(0.95));
}
