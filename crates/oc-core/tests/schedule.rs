//! Scheduled prompts (`core/src/schedule.ts`): upstream's `test/schedule.test.ts` (2.2.5) over the real service, with
//! a manual clock, a prompter that admits into the session inbox and records wakes, real shell checks and test
//! Code Mode tools in the `slack` namespace. Each test builds its own app (explicit paths, no process-wide XDG).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use async_trait::async_trait;
use futures::future::BoxFuture;
use oc_core::app::App;
use oc_core::runner::Engine;
use oc_core::schedule::{Clock, CreateInput, Prompter, ScheduleError, Schedules, StepError, time};
use oc_core::session::{CreateInput as SessionInput, PromptInput};
use oc_schema::types::{LocationRef, PermissionRule};
use oc_tools::tool::{DefinitionContext, ToolOptions};
use oc_tools::{Tool, ToolContext, ToolDefinition, ToolError, ToolResult};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};

const HOUR: i64 = 3_600_000;

fn ms(iso: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(iso).unwrap().timestamp_millis()
}

fn iso(v: &Value) -> String {
    time::format_iso(v.as_i64().expect("a time"))
}

/// Records wakes and admits prompts into the inbox, like `sessions.prompt` (which wakes the session itself).
struct TestPrompter {
    app: Arc<App>,
    wakes: Mutex<Vec<String>>,
    /// Runs inside the next prompt's wake (`duringWake`).
    during: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    fail_next: AtomicBool,
}

impl Prompter for TestPrompter {
    fn prompt(&self, session_id: String, id: String, text: String, metadata: Map<String, Value>) -> BoxFuture<'static, Result<(), StepError>> {
        let app = self.app.clone();
        let fail = self.fail_next.swap(false, Ordering::SeqCst);
        let during = self.during.lock().take();
        let wake = !fail;
        if wake {
            self.wakes.lock().push(session_id.clone());
        }
        Box::pin(async move {
            app.sessions
                .prompt(&session_id, PromptInput { id: Some(id), text, metadata: Some(metadata), ..Default::default() })
                .await
                .map_err(|e| StepError::Other(e.to_string()))?;
            if fail {
                return Err(StepError::Other("wake failed".into()));
            }
            if let Some(f) = during {
                f();
            }
            Ok(())
        })
    }

    fn wake(&self, session_id: &str) {
        self.wakes.lock().push(session_id.to_string());
    }
}

/// `slack.read_thread`, `slack.export_thread` and `slack.dangerous_edit` (upstream's `configureAgent` tools).
#[derive(Default)]
struct Slack {
    replies: AtomicUsize,
    reads: AtomicUsize,
    exports: AtomicUsize,
    failing: AtomicBool,
    /// Call ids `read_thread` ran with.
    calls: Mutex<Vec<String>>,
}

fn denied(e: oc_tools::host::PermissionError) -> ToolError {
    use oc_tools::host::PermissionError as P;
    match e {
        P::Blocked { message } | P::Other { message } => ToolError::new(message),
        P::Corrected { feedback } => ToolError::new(feedback),
        other => other.into_tool_error("declined"),
    }
}

struct SlackTool {
    name: &'static str,
    slack: Arc<Slack>,
}

#[async_trait]
impl Tool for SlackTool {
    fn name(&self) -> &str {
        self.name
    }
    fn options(&self) -> ToolOptions {
        let mut o = ToolOptions::codemode("slack");
        if self.name == "dangerous_edit" {
            o.permission = Some("edit".into());
        }
        o
    }
    fn definition(&self, _: &DefinitionContext) -> ToolDefinition {
        let output = (self.name == "read_thread").then(
            || json!({"type": "object", "properties": {"replies": {"type": "number"}}, "required": ["replies"], "additionalProperties": false}),
        );
        ToolDefinition::new(self.name, "test tool", json!({"type": "object", "properties": {}, "additionalProperties": false})).with_output(output)
    }
    async fn execute(&self, _input: Value, ctx: ToolContext) -> Result<ToolResult, ToolError> {
        match self.name {
            "read_thread" => {
                ctx.host.ask_permission(ctx.permission("slack_read_thread", vec!["*".into()], None, None)).await.map_err(denied)?;
                if self.slack.failing.load(Ordering::SeqCst) {
                    return Err(ToolError::new("Slack is down"));
                }
                self.slack.reads.fetch_add(1, Ordering::SeqCst);
                self.slack.calls.lock().push(ctx.call_id.clone());
                Ok(ToolResult { output: Some(json!({"replies": self.slack.replies.load(Ordering::SeqCst)})), content: vec![], metadata: None })
            }
            "export_thread" => {
                ctx.host.ask_permission(ctx.permission("file_write", vec!["*".into()], None, None)).await.map_err(denied)?;
                self.slack.exports.fetch_add(1, Ordering::SeqCst);
                Ok(ToolResult { output: None, content: vec![oc_schema::ToolContent::Text { text: "exported".into() }], metadata: None })
            }
            _ => Ok(ToolResult { output: None, content: vec![oc_schema::ToolContent::Text { text: "edited".into() }], metadata: None }),
        }
    }
}

struct H {
    app: Arc<App>,
    engine: Arc<Engine>,
    schedules: Arc<Schedules>,
    prompter: Arc<TestPrompter>,
    slack: Arc<Slack>,
    root: PathBuf,
    _tmp: tempfile::TempDir,
}

fn global(root: &Path) -> oc_util::Global {
    let g = |p: &str| {
        let d = root.join(p);
        std::fs::create_dir_all(&d).unwrap();
        d
    };
    oc_util::Global {
        home: g("home"),
        data: g("data"),
        cache: g("cache"),
        config: g("config"),
        state: g("state"),
        tmp: g("tmp"),
        bin: g("bin"),
        log: g("log"),
        repos: g("repos"),
    }
}

struct NoStore;
#[async_trait]
impl oc_tools::websearch::WebSearchStore for NoStore {
    async fn kv_get(&self, _: &str) -> Option<Value> {
        None
    }
    async fn kv_set(&self, _: &str, _: Value) {}
    async fn kv_remove(&self, _: &str) {}
    async fn api_key(&self, _: &str) -> Option<String> {
        None
    }
}

fn write_config(dir: &Path, config: Value) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("opencode.json"), config.to_string()).unwrap();
}

fn enabled_config() -> Value {
    json!({"timezone": "America/New_York", "experimental": {"schedules": true}})
}

/// An app over `db` (a fresh in-memory one when `None`) with directories `enabled`, `disabled` and `toggle`.
fn harness_with(db: Option<oc_storage::Database>, clock: Clock, tmp: Option<tempfile::TempDir>) -> H {
    unsafe { std::env::set_var("OPENCODE_DISABLE_MODELS_FETCH", "1") };
    let tmp = tmp.unwrap_or_else(|| tempfile::tempdir().unwrap());
    let root = oc_util::fs::canonicalize(tmp.path()).unwrap();
    write_config(&root.join("enabled"), enabled_config());
    write_config(&root.join("disabled"), json!({"experimental": {"schedules": false}}));
    write_config(&root.join("toggle"), json!({"experimental": {"schedules": false}}));
    let app = App::new(global(&root.join("g")), db.unwrap_or_else(|| oc_storage::Database::memory().unwrap()), "2.2.5");
    let services = oc_tools::tools::Services {
        formatter: Arc::new(oc_tools::formatter::Formatter::new()),
        ripgrep: Arc::new(oc_tools::ripgrep::Ripgrep::new(app.global.bin.clone(), reqwest::Client::new())),
        shell: oc_tools::shell::service::ShellService::new(app.global.data.clone(), None),
        websearch: Arc::new(oc_tools::websearch::WebSearch::new(Arc::new(NoStore), reqwest::Client::new())),
        http: reqwest::Client::new(),
    };
    let engine = Engine::new(app.clone(), &services);
    let slack = Arc::new(Slack::default());
    for name in ["read_thread", "export_thread", "dangerous_edit"] {
        engine.extra_tools.lock().push(Arc::new(SlackTool { name, slack: slack.clone() }));
    }
    let schedules = Schedules::with_clock(&app, clock);
    schedules.set_engine(Arc::downgrade(&engine));
    let prompter =
        Arc::new(TestPrompter { app: app.clone(), wakes: Mutex::new(Vec::new()), during: Mutex::new(None), fail_next: AtomicBool::new(false) });
    schedules.set_prompter(prompter.clone());
    H { app, engine, schedules, prompter, slack, root, _tmp: tmp }
}

/// A clock that stands still (ticks are driven by the test).
fn harness() -> H {
    harness_with(None, Clock::fixed(0), None)
}

/// A clock that follows tokio's paused time (the loop sleeps).
fn paused_harness() -> H {
    harness_with(None, Clock::manual(0), None)
}

impl H {
    fn dir(&self, name: &str) -> String {
        self.root.join(name).display().to_string()
    }

    async fn session_in(&self, name: &str, permissions: Option<Vec<PermissionRule>>) -> String {
        self.app
            .sessions
            .create(SessionInput {
                location: Some(LocationRef { directory: self.dir(name), workspace_id: None }),
                agent: Some("build".into()),
                permissions: Some(permissions.unwrap_or_else(|| vec![rule("*", "allow")])),
                ..Default::default()
            })
            .await
            .unwrap()
            .id
    }

    async fn session(&self) -> String {
        self.session_in("enabled", None).await
    }

    fn pending(&self) -> Vec<String> {
        self.app
            .db
            .with(|c| {
                let mut s = c.prepare("SELECT id FROM session_inbox ORDER BY enqueued_seq")?;
                s.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap()
    }

    fn inbox(&self, id: &str) -> Value {
        let (kind, delivery, payload): (String, String, String) = self
            .app
            .db
            .with(|c| c.query_row("SELECT type, delivery, payload FROM session_inbox WHERE id = ?", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))))
            .unwrap();
        json!({"type": kind, "delivery": delivery, "payload": serde_json::from_str::<Value>(&payload).unwrap()})
    }

    async fn deliver(&self, session: &str) {
        oc_core::inbox::promote(&self.app.bus, &self.app.sessions.locks, session, oc_core::inbox::Scope::Input).await.unwrap();
    }

    fn adjust(&self, ms: i64) {
        self.schedules.clock().advance(ms);
    }

    fn set_time(&self, iso: &str) {
        self.schedules.clock().set(ms(iso));
    }

    async fn tick(&self) {
        self.schedules.tick().await;
    }

    fn get(&self, id: &str) -> Value {
        self.schedules.get(id).unwrap()
    }

    fn wakes(&self) -> Vec<String> {
        self.prompter.wakes.lock().clone()
    }

    async fn create(&self, session: &str, input: Value) -> Result<Value, ScheduleError> {
        let mut request = input.clone();
        request["prompt"] = input.get("prompt").cloned().unwrap_or(json!("x"));
        self.schedules.create(CreateInput::from_request(session, &request)).await
    }
}

fn rule(action: &str, effect: &str) -> PermissionRule {
    serde_json::from_value(json!({"action": action, "resource": "*", "effect": effect})).unwrap()
}

fn msg(info: &Value, run: i64) -> String {
    format!("msg_{}_{run}", info["id"].as_str().unwrap())
}

fn subset(actual: &Value, expected: Value) {
    for (k, v) in expected.as_object().unwrap() {
        if v.is_null() {
            assert!(actual.get(k).is_none(), "{k} should be absent in {actual}");
        } else {
            assert_eq!(actual.get(k), Some(v), "{k} in {actual}");
        }
    }
}

fn invalid(result: Result<Value, ScheduleError>) -> String {
    match result {
        Err(ScheduleError::Invalid(reason)) => reason,
        other => panic!("expected InvalidError, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fires_an_in_schedule_once() {
    let h = harness();
    let s = h.session().await;
    let schedule = h.create(&s, json!({"when": {"in": "5 minutes"}, "prompt": "check the build"})).await.unwrap();
    assert_eq!(schedule["tz"], "America/New_York");
    h.adjust(4 * 60_000);
    h.tick().await;
    assert!(h.pending().is_empty());
    h.adjust(60_000);
    h.tick().await;
    assert_eq!(h.pending(), vec![msg(&schedule, 1)]);
    assert_eq!(h.wakes(), vec![s.clone()]);
    let item = h.inbox(&msg(&schedule, 1));
    assert_eq!((item["type"].as_str(), item["delivery"].as_str()), (Some("user"), Some("steer")));
    assert_eq!(item["payload"]["text"], "check the build");
    assert_eq!(item["payload"]["metadata"], json!({"schedule": {"id": schedule["id"], "run": 1, "dueAt": 300_000, "late": false}}));
    subset(&h.get(schedule["id"].as_str().unwrap()), json!({"status": "done", "runCount": 1, "lastOutcome": "fired", "nextDueAt": null}));
    h.adjust(10 * 60_000);
    h.tick().await;
    assert_eq!(h.pending(), vec![msg(&schedule, 1)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fires_an_at_schedule_at_the_wall_clock_time_in_its_zone() {
    let h = harness();
    h.set_time("2026-11-01T12:00:00Z");
    let s = h.session().await;
    let schedule = h.create(&s, json!({"when": {"at": "2026-11-02T09:30", "tz": "Europe/Paris"}, "prompt": "standup"})).await.unwrap();
    assert_eq!(iso(&schedule["nextDueAt"]), "2026-11-02T08:30:00.000Z");
    assert_eq!(schedule["when"], json!({"type": "at", "at": "2026-11-02T09:30", "tz": "Europe/Paris"}));
    assert_eq!(
        invalid(h.create(&s, json!({"when": {"at": "2026-11-01T11:00:00Z"}, "prompt": "too late"})).await),
        "Time is in the past: 2026-11-01T11:00:00Z"
    );
    h.set_time("2026-11-02T08:30:00Z");
    h.tick().await;
    assert_eq!(h.pending(), vec![msg(&schedule, 1)]);
    assert_eq!(h.get(schedule["id"].as_str().unwrap())["status"], "done");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stops_every_schedules_at_ends_times_and_ends_at() {
    let h = harness();
    let s = h.session().await;
    let schedule = h.create(&s, json!({"when": {"every": "1 minute"}, "ends": {"times": 2}, "prompt": "poll"})).await.unwrap();
    for _ in 0..3 {
        h.adjust(60_000);
        h.tick().await;
        h.deliver(&s).await;
    }
    subset(&h.get(schedule["id"].as_str().unwrap()), json!({"status": "done", "runCount": 2, "remainingRuns": 0}));
    assert_eq!(h.wakes(), vec![s.clone(), s.clone()]);

    h.set_time("2026-10-26T12:00:00Z");
    let at = h.create(&s, json!({"when": {"every": "10 minutes"}, "ends": {"at": "2026-10-26T12:25:00Z"}, "prompt": "poll"})).await.unwrap();
    for _ in 0..3 {
        h.adjust(600_000);
        h.tick().await;
        h.deliver(&s).await;
    }
    subset(&h.get(at["id"].as_str().unwrap()), json!({"status": "done", "runCount": 2}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cron_follows_wall_clock_time_across_dst() {
    let h = harness();
    let s = h.session().await;
    let cron = |expr: &str| json!({"when": {"cron": expr, "tz": "America/New_York"}, "prompt": expr});
    h.set_time("2026-10-31T12:00:00Z");
    let schedule = h.create(&s, cron("30 9 * * *")).await.unwrap();
    assert_eq!(iso(&schedule["nextDueAt"]), "2026-10-31T13:30:00.000Z");
    h.set_time("2026-10-31T13:30:00Z");
    h.tick().await;
    assert_eq!(iso(&h.get(schedule["id"].as_str().unwrap())["nextDueAt"]), "2026-11-01T14:30:00.000Z");

    // On 2026-11-01 New York falls back at 06:00Z, so 1:00-1:59 runs twice.
    h.set_time("2026-11-01T05:50:00Z");
    assert_eq!(iso(&h.create(&s, cron("*/10 * * * *")).await.unwrap()["nextDueAt"]), "2026-11-01T07:00:00.000Z");
    h.set_time("2026-11-01T06:35:00Z");
    for expr in ["*/10 * * * *", "0,30 * * * *", "0 * * * *"] {
        assert_eq!(iso(&h.create(&s, cron(expr)).await.unwrap()["nextDueAt"]), "2026-11-01T07:00:00.000Z", "{expr}");
    }
    h.set_time("2026-11-01T05:00:00Z");
    let fixed = h.create(&s, cron("30 1 * * *")).await.unwrap();
    assert_eq!(iso(&fixed["nextDueAt"]), "2026-11-01T05:30:00.000Z");
    h.set_time("2026-11-01T05:30:00Z");
    h.deliver(&s).await;
    h.tick().await;
    assert_eq!(iso(&h.get(fixed["id"].as_str().unwrap())["nextDueAt"]), "2026-11-02T06:30:00.000Z");
    h.set_time("2026-11-01T06:05:00Z");
    assert_eq!(iso(&h.create(&s, cron("30 1 * * *")).await.unwrap()["nextDueAt"]), "2026-11-02T06:30:00.000Z");

    // Spring forward: on 2026-03-08 New York jumps from 2:00 EST to 3:00 EDT at 07:00Z.
    h.set_time("2026-03-08T06:55:00Z");
    assert_eq!(iso(&h.create(&s, cron("*/10 * * * *")).await.unwrap()["nextDueAt"]), "2026-03-08T07:00:00.000Z");
    assert_eq!(iso(&h.create(&s, cron("30 2 * * *")).await.unwrap()["nextDueAt"]), "2026-03-08T07:30:00.000Z");

    assert_eq!(invalid(h.create(&s, json!({"when": {"cron": "0 0 31 2 *"}, "prompt": "never"})).await), "cron never fires: 0 0 31 2 *");
    assert_eq!(invalid(h.create(&s, json!({"when": {"cron": "* * * * * *"}, "prompt": "x"})).await), "cron must fire at most once a minute");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_firing_is_marked_and_phase_kept() {
    let h = harness();
    h.set_time("2026-10-26T12:00:00Z");
    let s = h.session().await;
    let schedule = h.create(&s, json!({"when": {"every": "10 minutes"}, "prompt": "poll"})).await.unwrap();
    h.set_time("2026-10-26T13:05:00Z");
    h.tick().await;
    h.tick().await;
    assert_eq!(h.pending(), vec![msg(&schedule, 1)]);
    assert_eq!(h.inbox(&msg(&schedule, 1))["payload"]["metadata"]["schedule"]["late"], true);
    let info = h.get(schedule["id"].as_str().unwrap());
    subset(&info, json!({"runCount": 1, "lastOutcome": "late"}));
    assert_eq!(iso(&info["nextDueAt"]), "2026-10-26T13:10:00.000Z");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn skips_while_unconsumed_and_never_duplicates_a_crashed_admission() {
    let h = harness();
    let s = h.session().await;
    let schedule = h.create(&s, json!({"when": {"every": "1 minute"}, "prompt": "poll"})).await.unwrap();
    h.adjust(60_000);
    h.tick().await;
    h.adjust(60_000);
    h.tick().await;
    assert_eq!(h.pending(), vec![msg(&schedule, 1)]);
    subset(&h.get(schedule["id"].as_str().unwrap()), json!({"runCount": 1, "lastOutcome": "skipped"}));
    assert_eq!(h.wakes(), vec![s.clone(), s.clone()]);
    h.deliver(&s).await;
    h.adjust(60_000);
    h.tick().await;
    assert_eq!(h.pending(), vec![msg(&schedule, 2)]);
    h.deliver(&s).await;

    let once = h.create(&s, json!({"when": {"in": "1 minute"}, "prompt": "once"})).await.unwrap();
    h.adjust(60_000);
    // The previous process admitted run 1 and died before advancing the row.
    h.app.sessions.prompt(&s, PromptInput { id: Some(msg(&once, 1)), text: "once".into(), ..Default::default() }).await.unwrap();
    h.prompter.wakes.lock().clear();
    h.tick().await;
    assert!(h.pending().contains(&msg(&once, 1)));
    subset(&h.get(once["id"].as_str().unwrap()), json!({"status": "done", "runCount": 1}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn removes_schedules_with_the_session_and_ignores_disabled_locations() {
    let h = harness();
    let s = h.session().await;
    h.create(&s, json!({"when": {"every": "1 hour"}, "prompt": "poll"})).await.unwrap();
    h.app.db.with(|c| c.execute("DELETE FROM session_v2 WHERE id = ?", [&s])).unwrap();
    assert!(h.schedules.list(None).is_empty());

    let off = h.session_in("disabled", None).await;
    assert_eq!(h.create(&off, json!({"when": {"in": "1 minute"}})).await, Err(ScheduleError::Disabled));
    h.app
        .db
        .with(|c| {
            c.execute(
                "INSERT INTO schedule (id, session_id, action, \"when\", tz, next_due_at, time_created, time_updated) VALUES ('sch_off', ?, '{\"type\":\"prompt\",\"prompt\":\"poll\"}', '{\"type\":\"every\",\"every\":\"1 minute\"}', 'UTC', 60000, 0, 0)",
                [&off],
            )
        })
        .unwrap();
    h.adjust(60_000);
    h.tick().await;
    assert!(h.pending().is_empty());
    subset(&h.get("sch_off"), json!({"status": "active", "runCount": 0, "lastOutcome": "disabled", "nextDueAt": 60_000}));
    // Within the hour the flag is not read again; once it is on and the hour has passed, the slot fires once, late.
    h.adjust(59 * 60_000);
    h.tick().await;
    assert!(h.pending().is_empty());
    write_config(&h.root.join("disabled"), enabled_config());
    h.engine.reload_location(&h.dir("disabled")).await.unwrap();
    h.adjust(2 * 60_000);
    h.tick().await;
    assert_eq!(h.pending(), vec!["msg_sch_off_1".to_string()]);
    subset(&h.get("sch_off"), json!({"status": "active", "runCount": 1, "lastOutcome": "late", "nextDueAt": 63 * 60_000}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pauses_resumes_and_keeps_the_run_count_when_paused_during_admission() {
    let h = harness();
    let s = h.session().await;
    let schedule = h.create(&s, json!({"when": {"every": "1 minute"}, "prompt": "poll"})).await.unwrap();
    let id = schedule["id"].as_str().unwrap().to_string();
    assert_eq!(h.schedules.pause(&id).unwrap()["status"], "paused");
    h.adjust(150_000);
    h.tick().await;
    assert!(h.pending().is_empty());
    let resumed = h.schedules.resume(&id).unwrap();
    assert_eq!(resumed["status"], "active");
    assert_eq!(resumed["nextDueAt"], 180_000);
    h.schedules.remove(&id).unwrap();
    assert_eq!(h.schedules.get(&id), Err(ScheduleError::NotFound(id.clone())));
    assert_eq!(ScheduleError::NotFound(id.clone()).message(), format!("Schedule not found: {id}"));

    let schedule = h.create(&s, json!({"when": {"every": "1 minute"}, "prompt": "poll"})).await.unwrap();
    let id = schedule["id"].as_str().unwrap().to_string();
    h.adjust(60_000);
    let schedules = h.schedules.clone();
    let pause_id = id.clone();
    *h.prompter.during.lock() = Some(Box::new(move || {
        schedules.pause(&pause_id).unwrap();
    }));
    h.tick().await;
    subset(&h.get(&id), json!({"status": "paused", "runCount": 1}));
    h.deliver(&s).await;
    h.schedules.resume(&id).unwrap();
    h.adjust(60_000);
    h.tick().await;
    assert_eq!(h.pending(), vec![msg(&schedule, 2)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backs_off_five_minutes_after_a_failed_prompt() {
    let h = harness();
    let s = h.session().await;
    let schedule = h.create(&s, json!({"when": {"in": "1 minute"}, "prompt": "flaky"})).await.unwrap();
    let id = schedule["id"].as_str().unwrap();
    h.adjust(60_000);
    h.prompter.fail_next.store(true, Ordering::SeqCst);
    h.tick().await;
    subset(&h.get(id), json!({"status": "active", "runCount": 0, "lastOutcome": "failed"}));
    h.app.db.with(|c| c.execute("DELETE FROM session_inbox", [])).unwrap();
    h.adjust(4 * 60_000);
    h.tick().await;
    assert!(h.wakes().is_empty());
    h.adjust(60_000);
    h.tick().await;
    assert_eq!(h.wakes(), vec![s.clone()]);
    assert_eq!(h.pending(), vec![msg(&schedule, 1)]);
    subset(&h.get(id), json!({"status": "done", "runCount": 1, "lastOutcome": "late"}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn validates_durations_times_and_checks_at_creation() {
    let h = harness();
    h.set_time("2026-11-01T12:00:00Z");
    let s = h.session().await;
    assert_eq!(invalid(h.create(&s, json!({"when": {"in": "0 seconds"}})).await), "Duration must be positive: 0 seconds");
    assert_eq!(invalid(h.create(&s, json!({"when": {"in": "soon"}})).await), "Invalid duration: soon");
    assert_eq!(invalid(h.create(&s, json!({"when": {"every": "1 hour"}, "ends": {"times": 0}})).await), "ends.times must be at least 1");
    assert_eq!(invalid(h.create(&s, json!({"when": {"every": "30 seconds"}})).await), "every must be at least 1 minute");
    assert_eq!(invalid(h.create(&s, json!({"when": {"in": "1 minute", "tz": "Mars/Base"}})).await), "Unknown time zone: Mars/Base");
    assert_eq!(invalid(h.create(&s, json!({"when": {"at": "tomorrow"}})).await), "Invalid time: tomorrow");
    assert_eq!(
        invalid(h.create(&s, json!({"when": {"in": "2 hours"}, "ends": {"after": "1 hour"}})).await),
        "The schedule ends before it first fires"
    );
    assert_eq!(invalid(h.create(&s, json!({})).await), "when is required unless until is set");
    assert_eq!(invalid(h.create(&s, json!({"if": {"run": "true"}, "until": {"run": "true"}})).await), "Choose either if or until, not both");
    assert_eq!(invalid(h.create(&s, json!({"when": {"every": "10 seconds"}, "until": {"run": "true"}})).await), "every must be at least 15 seconds");
    assert_eq!(
        invalid(h.create(&s, json!({"until": {"run": "echo hi", "is": "hi", "matches": "hi"}})).await),
        "Choose at most one of is, matches or changes"
    );
    assert_eq!(
        invalid(h.create(&s, json!({"until": {"run": "echo hi", "matches": "[unclosed"}})).await),
        "Invalid regular expression \"[unclosed\": Invalid regular expression: missing terminating ] for character class"
    );
    assert!(h.schedules.list(Some(&s)).is_empty());
    for (at, expected) in [
        ("2026-11-02T09:30:00-0500", "2026-11-02T14:30:00.000Z"),
        ("2026-11-02T09:30:00-05:00", "2026-11-02T14:30:00.000Z"),
        ("2026-11-02T09:30", "2026-11-02T14:30:00.000Z"),
        ("2026-11-02 09:30:00-05:00", "2026-11-02T14:30:00.000Z"),
        ("2026-11-02 14:30:00Z", "2026-11-02T14:30:00.000Z"),
        ("2026-11-02 09:30", "2026-11-02T14:30:00.000Z"),
    ] {
        assert_eq!(iso(&h.create(&s, json!({"when": {"at": at}})).await.unwrap()["nextDueAt"]), expected, "{at}");
    }
}

/// A shell check whose output and exit code the test controls through files in the location.
#[cfg(unix)]
fn shell_state(h: &H, stdout: &str, code: i32) {
    let dir = h.root.join("enabled");
    std::fs::write(dir.join("out.txt"), stdout).unwrap();
    std::fs::write(dir.join("code.txt"), code.to_string()).unwrap();
}

#[cfg(unix)]
const CHECK: &str = "cat out.txt; exit $(cat code.txt)";

/// Shell checks run POSIX `sh` commands.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell_until_checks_match_immediately_poll_and_expire() {
    let h = harness();
    let s = h.session().await;
    shell_state(&h, "all green\n", 0);
    let done = h.create(&s, json!({"until": {"run": CHECK}, "prompt": "CI passed."})).await.unwrap();
    subset(
        &done,
        json!({"status": "done", "runCount": 1, "lastOutcome": "fired", "lastOutput": "all green", "nextDueAt": null,
               "when": {"type": "every", "every": "30 seconds"}, "until": {"type": "shell", "run": CHECK}}),
    );
    assert!(h.pending().is_empty());
    assert!(h.wakes().is_empty());

    shell_state(&h, "", 1);
    let schedule = h.create(&s, json!({"until": {"run": CHECK}, "prompt": "CI passed."})).await.unwrap();
    subset(&schedule, json!({"status": "active", "remainingRuns": 1, "endsAt": 24 * HOUR}));
    h.adjust(30_000);
    h.tick().await;
    assert!(h.pending().is_empty());
    subset(&h.get(schedule["id"].as_str().unwrap()), json!({"status": "active", "runCount": 0, "remainingRuns": 1, "lastOutcome": "checked"}));
    shell_state(&h, "20 pass\n", 0);
    h.adjust(30_000);
    h.tick().await;
    assert_eq!(h.pending(), vec![msg(&schedule, 1)]);
    let item = h.inbox(&msg(&schedule, 1));
    assert_eq!(item["payload"]["text"], "CI passed.\n\n20 pass");
    assert_eq!(item["payload"]["metadata"]["schedule"]["check"], json!({"message": "20 pass"}));
    subset(
        &h.get(schedule["id"].as_str().unwrap()),
        json!({"status": "done", "runCount": 1, "remainingRuns": 0, "lastOutcome": "fired", "nextDueAt": null}),
    );
    h.deliver(&s).await;

    shell_state(&h, "", 1);
    let expiring = h
        .create(
            &s,
            json!({"when": {"every": "30 seconds"}, "until": {"run": CHECK}, "ends": {"after": "1 minute"}, "prompt": "Server check finished."}),
        )
        .await
        .unwrap();
    h.adjust(30_000);
    h.tick().await;
    assert!(h.pending().is_empty());
    h.adjust(30_000);
    h.tick().await;
    assert_eq!(h.pending(), vec![msg(&expiring, 1)]);
    let item = h.inbox(&msg(&expiring, 1));
    assert_eq!(item["payload"]["text"], format!("Server check finished.\n\n[Schedule timed out without matching: {CHECK}]"));
    assert_eq!(item["payload"]["metadata"]["schedule"]["check"], json!({"expired": true}));
    subset(&h.get(expiring["id"].as_str().unwrap()), json!({"status": "done", "runCount": 1, "lastOutcome": "expired", "nextDueAt": null}));
}

/// Shell checks run POSIX `sh` commands.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell_if_checks_seed_changes_and_support_is_and_matches() {
    let h = harness();
    let s = h.session().await;
    shell_state(&h, "sha-1\n", 0);
    let schedule = h
        .create(&s, json!({"when": {"every": "15 seconds"}, "if": {"run": CHECK, "changes": true}, "ends": {"times": 2}, "prompt": "HEAD moved."}))
        .await
        .unwrap();
    let id = schedule["id"].as_str().unwrap();
    h.adjust(15_000);
    h.tick().await;
    assert!(h.pending().is_empty());
    subset(&h.get(id), json!({"status": "active", "runCount": 0, "remainingRuns": 2, "lastOutcome": "checked"}));
    shell_state(&h, "sha-2\n", 0);
    h.adjust(15_000);
    h.tick().await;
    assert_eq!(h.pending(), vec![msg(&schedule, 1)]);
    subset(&h.get(id), json!({"status": "active", "runCount": 1, "remainingRuns": 1, "lastOutcome": "fired"}));
    h.deliver(&s).await;
    h.adjust(15_000);
    h.tick().await;
    assert!(h.pending().is_empty());
    shell_state(&h, "sha-3\n", 0);
    h.adjust(15_000);
    h.tick().await;
    assert_eq!(h.pending(), vec![msg(&schedule, 2)]);
    subset(&h.get(id), json!({"status": "done", "runCount": 2, "remainingRuns": 0, "lastOutcome": "fired"}));
    h.deliver(&s).await;

    shell_state(&h, "", 7);
    let exact = h.create(&s, json!({"when": {"every": "20 seconds"}, "until": {"run": CHECK, "is": "MERGED"}, "prompt": "Merged."})).await.unwrap();
    shell_state(&h, "OPEN\n", 0);
    let regex = h
        .create(&s, json!({"when": {"every": "20 seconds"}, "until": {"run": CHECK, "matches": "^MERGED"}, "prompt": "Regex merged."}))
        .await
        .unwrap();
    h.adjust(20_000);
    h.tick().await;
    assert!(h.pending().is_empty());
    shell_state(&h, "MERGED\n", 0);
    h.adjust(20_000);
    h.tick().await;
    let mut pending = h.pending();
    pending.sort();
    let mut expected = vec![msg(&exact, 1), msg(&regex, 1)];
    expected.sort();
    assert_eq!(pending, expected);
}

/// Answers every pending permission request with `once`, recording the requests. Polls the pending list rather than
/// following the bus, whose receiver may lag behind the session events under load.
fn answer_asks(h: &H) -> (Arc<Mutex<Vec<Value>>>, tokio::task::JoinHandle<()>) {
    let asked = Arc::new(Mutex::new(Vec::new()));
    let app = h.app.clone();
    let record = asked.clone();
    let task = tokio::spawn(async move {
        loop {
            for request in app.permissions.list() {
                record.lock().push(serde_json::to_value(&request).unwrap());
                let _ = app.permissions.reply(&request.id, oc_core::permission::Reply::Once, None).await;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    });
    (asked, task)
}

/// Shell checks run POSIX `sh` commands.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell_checks_ask_once_run_unattended_and_pause_after_three_failures() {
    let h = harness();
    write_config(
        &h.root.join("enabled"),
        json!({"timezone": "America/New_York", "experimental": {"schedules": true}, "permissions": [{"action": "shell", "resource": "*", "effect": "ask"}]}),
    );
    let s = h.session_in("enabled", Some(vec![])).await;
    shell_state(&h, "", 1);
    let (asked, task) = answer_asks(&h);
    let schedule = h.create(&s, json!({"name": "ci", "when": {"every": "1 minute"}, "if": {"run": CHECK}, "prompt": "CI green."})).await.unwrap();
    task.abort();
    let asked = asked.lock().clone();
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert!(asked[0].get("save").is_none());
    assert_eq!(asked[0]["action"], "shell");
    assert_eq!(asked[0]["resources"], json!(["cat out.txt", "exit $(cat code.txt)", "cat code.txt"]));
    assert_eq!(
        asked[0]["metadata"],
        json!({"schedule": "ci", "mode": "if", "when": {"type": "every", "every": "1 minute"}, "unattended": true, "run": CHECK})
    );
    let id = schedule["id"].as_str().unwrap();
    h.adjust(60_000);
    h.tick().await;
    subset(&h.get(id), json!({"lastOutcome": "checked"}));

    // Revoking shell permission: three failed checks pause the schedule with a wake.
    write_config(
        &h.root.join("enabled"),
        json!({"timezone": "America/New_York", "experimental": {"schedules": true}, "permissions": [{"action": "shell", "resource": "*", "effect": "deny"}]}),
    );
    h.engine.reload_location(&h.dir("enabled")).await.unwrap();
    for _ in 0..2 {
        h.adjust(60_000);
        h.tick().await;
        subset(&h.get(id), json!({"status": "active", "lastOutcome": "failed", "runCount": 0}));
    }
    h.adjust(60_000);
    h.tick().await;
    subset(&h.get(id), json!({"status": "paused", "lastOutcome": "failed", "runCount": 1}));
    assert_eq!(h.pending(), vec![msg(&schedule, 1)]);
    let text = h.inbox(&msg(&schedule, 1))["payload"]["text"].as_str().unwrap().to_string();
    assert!(text.starts_with("CI green.\n\n[Schedule paused after 3 failed checks: "), "{text}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_style_creation_settles_asks_unattended() {
    let h = harness();
    write_config(
        &h.root.join("enabled"),
        json!({"experimental": {"schedules": true}, "permissions": [{"action": "shell", "resource": "*", "effect": "ask"}]}),
    );
    let s = h.session_in("enabled", Some(vec![])).await;
    let created = oc_core::permission::unattended(Vec::new(), h.create(&s, json!({"until": {"run": "true"}}))).await;
    assert_eq!(created, Err(ScheduleError::Blocked("Permission shell needs approval, and nobody is present to give it".into())));
}

/// Shell checks run POSIX `sh` commands.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pauses_when_the_session_directory_disappears() {
    let h = harness();
    write_config(&h.root.join("gone"), enabled_config());
    let s = h.session_in("gone", None).await;
    let schedule = h.create(&s, json!({"when": {"every": "1 minute"}, "until": {"run": "exit 1"}, "prompt": "done"})).await.unwrap();
    std::fs::remove_dir_all(h.root.join("gone")).unwrap();
    h.adjust(60_000);
    h.tick().await;
    subset(&h.get(schedule["id"].as_str().unwrap()), json!({"status": "paused", "lastOutcome": "failed"}));
    assert!(
        matches!(h.create(&s, json!({"when": {"in": "1 minute"}})).await, Err(ScheduleError::Invalid(r)) if r.starts_with("Session directory does not exist: "))
    );
}

const WATCH_THREAD: &str = r#"
      const thread = await tools.slack.read_thread({})
      if (state === null || thread.replies <= state.replies) {
        return { wake: false, state: { replies: thread.replies, runs } }
      }
      return {
        wake: true,
        state: { replies: thread.replies, runs },
        message: thread.replies + " replies",
      }
    "#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_checks_seed_state_and_count_ends_times_by_wakes() {
    let h = harness();
    let s = h.session().await;
    h.slack.replies.store(3, Ordering::SeqCst);
    let schedule = h
        .create(&s, json!({"name": "thread", "when": {"every": "2 minutes"}, "if": {"code": WATCH_THREAD}, "ends": {"times": 1}, "prompt": "New thread activity."}))
        .await
        .unwrap();
    assert_eq!(h.slack.reads.load(Ordering::SeqCst), 1);
    let id = schedule["id"].as_str().unwrap();
    let row = h.schedules.row(id).unwrap();
    assert_eq!((row.state.clone(), row.checks), (json!({"replies": 3, "runs": 0}), 1));
    h.adjust(120_000);
    h.tick().await;
    assert!(h.pending().is_empty());
    subset(&h.get(id), json!({"status": "active", "runCount": 0, "remainingRuns": 1, "lastOutcome": "checked"}));
    h.slack.replies.store(4, Ordering::SeqCst);
    h.adjust(120_000);
    h.tick().await;
    assert_eq!(h.pending(), vec![msg(&schedule, 1)]);
    let item = h.inbox(&msg(&schedule, 1));
    assert_eq!(item["payload"]["text"], "New thread activity.\n\n4 replies");
    assert_eq!(item["payload"]["metadata"]["schedule"]["check"], json!({"message": "4 replies"}));
    assert_eq!(item["payload"]["metadata"]["schedule"]["name"], "thread");
    subset(&h.get(id), json!({"status": "done", "runCount": 1, "remainingRuns": 0, "lastOutcome": "fired"}));
    assert_eq!(h.schedules.row(id).unwrap().state, json!({"replies": 4, "runs": 2}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_until_checks_match_immediately_or_by_boolean() {
    let h = harness();
    let s = h.session().await;
    h.slack.replies.store(5, Ordering::SeqCst);
    let immediate = h
        .create(&s, json!({"until": {"code": "const t = await tools.slack.read_thread({}); return { wake: t.replies >= 5, message: \"already 5\" }"}, "prompt": "Done."}))
        .await
        .unwrap();
    subset(&immediate, json!({"status": "done", "lastOutcome": "fired", "lastOutput": "already 5"}));
    assert!(h.pending().is_empty());
    h.slack.replies.store(1, Ordering::SeqCst);
    let waiting = h
        .create(&s, json!({"until": {"code": "return (await tools.slack.read_thread({})).replies >= 2 && typeof fetch === \"undefined\""}, "prompt": "Reached 2."}))
        .await
        .unwrap();
    assert_eq!(waiting["status"], "active");
    h.slack.replies.store(2, Ordering::SeqCst);
    h.adjust(30_000);
    h.tick().await;
    assert_eq!(h.pending(), vec![msg(&waiting, 1)]);
    subset(&h.get(waiting["id"].as_str().unwrap()), json!({"status": "done", "lastOutcome": "fired"}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_checks_ask_once_and_deny_unapproved_side_permissions() {
    let h = harness();
    let s = h.session_in("enabled", Some(vec![rule("*", "ask")])).await;
    let (asked, task) = answer_asks(&h);
    let schedule =
        h.create(&s, json!({"name": "thread", "when": {"every": "1 minute"}, "if": {"code": WATCH_THREAD}, "prompt": "Changed."})).await.unwrap();
    let requests = asked.lock().clone();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(requests[0]["action"], "slack_read_thread");
    assert!(requests[0].get("save").is_none());
    assert_eq!(
        requests[0]["metadata"],
        json!({"schedule": "thread", "mode": "if", "when": {"type": "every", "every": "1 minute"}, "unattended": true, "code": WATCH_THREAD, "tools": ["slack.read_thread"]})
    );
    h.adjust(60_000);
    h.tick().await;
    assert_eq!(asked.lock().len(), 1);
    subset(&h.get(schedule["id"].as_str().unwrap()), json!({"lastOutcome": "checked"}));

    let failure = h.create(&s, json!({"until": {"code": "await tools.slack.export_thread({}); return true"}, "prompt": "Exported."})).await;
    task.abort();
    assert_eq!(h.slack.exports.load(Ordering::SeqCst), 0);
    let reason = invalid(failure);
    assert!(reason.contains("Permission file_write needs approval, and nobody is present to give it"), "{reason}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_checks_reject_dynamic_unknown_edit_and_recursive_tools() {
    let h = harness();
    let s = h.session().await;
    let bad = |code: &'static str| h.create(&s, json!({"until": {"code": code}}));
    assert!(invalid(bad("const x = {").await).starts_with("SyntaxError: "));
    assert!(invalid(bad("return await tools.slack[name]({})").await).starts_with("Schedule checks must reference tools statically"));
    assert_eq!(invalid(bad("return await tools.slack.missing({})").await), "Unknown Code Mode tool: slack.missing");
    assert_eq!(invalid(bad("return await tools.slack.dangerous_edit({})").await), "Tool slack.dangerous_edit cannot be used inside a schedule check");
    assert_eq!(
        invalid(bad("return await tools.opencode.schedule_create({})").await),
        "Tool opencode.schedule_create cannot be used inside a schedule check"
    );
    let denied = h.session_in("enabled", Some(vec![rule("*", "allow"), rule("execute", "deny")])).await;
    assert!(matches!(h.create(&denied, json!({"until": {"code": "return true"}})).await, Err(ScheduleError::Blocked(_))));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_checks_fail_creation_when_they_throw_and_pause_on_an_invalid_result() {
    let h = harness();
    let s = h.session().await;
    let thrown = h
        .create(&s, json!({"when": {"cron": "0 9 * * *"}, "if": {"code": "const t = await tools.slack.read_thread({}); return t.nonexistent.length > 0"}, "prompt": "Morning check."}))
        .await;
    invalid(thrown);
    let schedule = h
        .create(&s, json!({"when": {"every": "1 minute"}, "if": {"code": "if (state === null) return { wake: false, state: { ok: true } }; return \"not-a-check-result\""}, "prompt": "Watch."}))
        .await
        .unwrap();
    h.adjust(60_000);
    h.tick().await;
    subset(&h.get(schedule["id"].as_str().unwrap()), json!({"status": "paused", "lastOutcome": "failed", "runCount": 1}));
    assert_eq!(
        h.inbox(&msg(&schedule, 1))["payload"]["text"],
        "Watch.\n\n[Schedule paused after invalid check: A code check must return a boolean or { wake, state?, message? }: SchemaError(Expected object)]"
    );
}

/// Nested calls get `call_<messageID>_<n>` ids; a failing tool reaches the program as a thrown error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_check_calls_use_derived_ids() {
    let h = harness();
    let s = h.session().await;
    let schedule = h
        .create(&s, json!({"when": {"every": "1 minute"}, "if": {"code": "await tools.slack.read_thread({}); await tools.slack.read_thread({}); return false"}, "prompt": "x"}))
        .await
        .unwrap();
    let id = schedule["id"].as_str().unwrap();
    assert_eq!(*h.slack.calls.lock(), vec![format!("call_msg_{id}_check_0_0"), format!("call_msg_{id}_check_0_1")]);
    h.adjust(60_000);
    h.tick().await;
    assert_eq!(h.slack.calls.lock()[2..], [format!("call_msg_{id}_check_2_0"), format!("call_msg_{id}_check_2_1")]);
    h.slack.failing.store(true, Ordering::SeqCst);
    h.adjust(60_000);
    h.tick().await;
    subset(&h.get(id), json!({"status": "active", "lastOutcome": "failed", "runCount": 0}));
    assert_eq!(h.schedules.row(id).unwrap().failures, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn leader_lease_lets_one_process_fire() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("db.sqlite");
    let data = tmp.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let a = harness_with(Some(oc_storage::Database::open(&path, &data).unwrap()), Clock::fixed(0), None);
    let b = harness_with(Some(oc_storage::Database::open(&path, &data).unwrap()), Clock::fixed(0), None);
    assert!(a.schedules.claim_leader(0));
    assert!(!b.schedules.claim_leader(1_000), "a live lease belongs to its owner");
    assert!(a.schedules.claim_leader(20_000), "the owner renews");
    let lease = a.app.db.kv_get("schedule:leader").unwrap().unwrap();
    assert_eq!(lease, json!({"owner": a.schedules.owner(), "until": 80_000}));
    assert!(!b.schedules.claim_leader(79_999));
    assert!(b.schedules.claim_leader(80_000), "an expired lease is taken over");
    b.schedules.release_leader();
    assert_eq!(a.app.db.kv_get("schedule:leader").unwrap(), None);
    a.schedules.release_leader();
}

#[tokio::test(start_paused = true)]
async fn run_sleeps_until_due_rewakes_at_start_and_releases_when_idle() {
    let h = paused_harness();
    let s = h.session().await;
    let first = h.create(&s, json!({"when": {"in": "1 minute"}, "prompt": "once"})).await.unwrap();
    h.adjust(60_000);
    h.tick().await;
    // The process died before the session claimed the prompt, so that wake was lost: `run` wakes it on start.
    h.prompter.wakes.lock().clear();
    let runner = tokio::spawn(h.schedules.clone().run());
    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    assert_eq!(h.wakes(), vec![s.clone()]);
    // Nothing active: the lease is released.
    assert_eq!(h.app.db.kv_get("schedule:leader").unwrap(), None);
    h.deliver(&s).await;
    assert_eq!(h.pending(), Vec::<String>::new());

    let schedule = h.create(&s, json!({"when": {"in": "10 seconds"}, "prompt": "soon"})).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(9_000)).await;
    assert!(h.pending().is_empty());
    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
    assert_eq!(h.pending(), vec![msg(&schedule, 1)]);
    runner.abort();
    let _ = runner.await;
    assert_eq!(h.app.db.kv_get("schedule:leader").unwrap(), None);
    let _ = first;
}

#[tokio::test(start_paused = true)]
async fn run_picks_up_a_schedule_another_process_wrote() {
    let h = paused_harness();
    let s = h.session().await;
    let runner = tokio::spawn(h.schedules.clone().run());
    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    let now = h.schedules.clock().now();
    h.app
        .db
        .with(|c| {
            c.execute(
                "INSERT INTO schedule (id, session_id, name, action, \"when\", tz, status, next_due_at, remaining_runs, time_created, time_updated) \
                 VALUES ('sch_external', ?, 'external', '{\"type\":\"prompt\",\"prompt\":\"Cross-process wake.\"}', '{\"type\":\"in\",\"in\":\"10 seconds\"}', 'UTC', 'active', ?, 1, ?, ?)",
                rusqlite::params![s, now + 10_000, now, now],
            )
        })
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(30_100)).await;
    runner.abort();
    subset(&h.get("sch_external"), json!({"status": "done", "runCount": 1, "lastOutcome": "fired"}));
}

#[tokio::test(start_paused = true)]
async fn two_processes_on_one_database_fire_a_schedule_once() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("db.sqlite");
    let data = tmp.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let a = harness_with(Some(oc_storage::Database::open(&path, &data).unwrap()), Clock::manual(0), None);
    let b = harness_with(Some(oc_storage::Database::open(&path, &data).unwrap()), Clock::manual(0), Some(tempfile::tempdir().unwrap()));
    // The same session row is visible to both; its directory must exist for each process's config.
    let s = a.session().await;
    let schedule = a.create(&s, json!({"when": {"every": "1 minute"}, "ends": {"times": 3}, "prompt": "poll"})).await.unwrap();
    let ra = tokio::spawn(a.schedules.clone().run());
    let rb = tokio::spawn(b.schedules.clone().run());
    for _ in 0..4 {
        tokio::time::sleep(std::time::Duration::from_millis(60_000)).await;
        a.deliver(&s).await;
    }
    ra.abort();
    rb.abort();
    subset(&a.get(schedule["id"].as_str().unwrap()), json!({"status": "done", "runCount": 3}));
    assert_eq!(a.wakes().len() + b.wakes().len(), 3, "a: {:?}, b: {:?}", a.wakes(), b.wakes());
}

/// `schedule_create` input issues match Effect's decoding of `Schedule.Request` (cases captured from 2.2.5 in
/// `fixtures/schedule_input_issues.jsonl`), including every union member's issues and `Expected never`.
#[tokio::test]
async fn schedule_create_input_issues_match_effect() {
    let h = harness();
    let mut registry = oc_tools::Registry::new();
    oc_core::schedule::tools::register(&mut registry, &h.schedules);
    let tool = registry.get("opencode_schedule_create").unwrap();
    for line in include_str!("fixtures/schedule_input_issues.jsonl").lines() {
        let case: Value = serde_json::from_str(line).unwrap();
        let issues: Vec<Value> = tool
            .validate(&case["input"])
            .into_iter()
            .map(|i| {
                let path: Vec<Value> = i
                    .path
                    .iter()
                    .map(|p| match p {
                        oc_tools::validate::PathSegment::Key(k) => json!(k),
                        oc_tools::validate::PathSegment::Index(n) => json!(n),
                    })
                    .collect();
                json!({"path": path, "message": i.message})
            })
            .collect();
        let expected = if case["issues"].is_null() { json!([]) } else { case["issues"].clone() };
        assert_eq!(json!(issues), expected, "{}", case["input"]);
    }
}
