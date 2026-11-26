//! Automations (`core/src/automation.ts`): upstream 2.3.1's `test/automation.test.ts` and `test/tool-automation.test.ts`
//! over the real service, with a manual clock, a prompter that admits into the session inbox and records wakes, and
//! test Code Mode tools in the `slack` namespace. Each test builds its own app (explicit paths, no process-wide XDG).
//!
//! The MCP event watch cases (`automation-event.test.ts`) are in `automation_event.rs`. Not ported: the plugin
//! activation gate of "waits for plugins when authorizing" (ocd activates plugins while it builds the location).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use async_trait::async_trait;
use futures::future::BoxFuture;
use oc_core::app::App;
use oc_core::automation::{AutomationError, Automations, Clock, CreateInput, Invocation, ListInput, Prompter, time};
use oc_core::permission::Source;
use oc_core::runner::Engine;
use oc_core::session::{CreateInput as SessionInput, PromptInput};
use oc_schema::types::{LocationRef, PermissionRule};
use oc_tools::tool::{DefinitionContext, ToolOptions};
use oc_tools::{Tool, ToolContext, ToolDefinition, ToolError, ToolResult};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};

const MINUTE: i64 = 60_000;

fn ms(iso: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(iso).unwrap().timestamp_millis()
}

fn iso(v: &Value) -> String {
    time::format_iso(v.as_i64().expect("a time"))
}

type During = Box<dyn FnOnce() + Send>;

/// Admits prompts into the inbox and records wakes, like `sessions.prompt` (which wakes the session itself).
struct TestPrompter {
    app: Arc<App>,
    wakes: Arc<Mutex<Vec<String>>>,
    /// Runs inside the next wake, which happens while an automation is running (`duringWake`).
    during: Mutex<Option<During>>,
    /// The next wake fails after its admission (`duringWake = Effect.die(...)`).
    fail_next: AtomicBool,
}

impl Prompter for TestPrompter {
    fn prompt(&self, session_id: String, id: String, text: String, metadata: Map<String, Value>) -> BoxFuture<'static, Result<(), AutomationError>> {
        let app = self.app.clone();
        let fail = self.fail_next.swap(false, Ordering::SeqCst);
        let during = self.during.lock().take();
        let wakes = self.wakes.clone();
        Box::pin(async move {
            // A new admission needs the session's location (`instances.provide(session)`); an existing one is
            // reconciled without it.
            let session = app.sessions.get(&session_id).map_err(|e| AutomationError::Other(e.to_string()))?;
            let known = app.db.with(|c| c.query_row("SELECT 1 FROM session_inbox WHERE id = ?", [&id], |_| Ok(()))).is_ok();
            if !known && !std::path::Path::new(&session.location.directory).is_dir() {
                return Err(AutomationError::DirectoryNotFound(session.location.directory));
            }
            // A rejected admission (a conflicting id) fails before any wake.
            app.sessions
                .prompt(&session_id, PromptInput { id: Some(id), text, metadata: Some(metadata), ..Default::default() })
                .await
                .map_err(|e| AutomationError::Other(e.to_string()))?;
            if fail {
                return Err(AutomationError::Other("wake failed".into()));
            }
            wakes.lock().push(session_id);
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

/// `slack.read_thread`, `slack.post` and `slack.wait` (upstream's `configureAgent` tools, and a gate).
#[derive(Default)]
struct Slack {
    replies: Mutex<i64>,
    reads: AtomicUsize,
    failing: AtomicBool,
    posts: Mutex<Vec<String>>,
    /// Message ids `read_thread` ran with.
    message_ids: Mutex<Vec<String>>,
    /// `slack.wait` calls that entered, and the gate they wait on.
    entered: AtomicUsize,
    gate: Gate,
}

struct Gate(tokio::sync::Semaphore);

impl Default for Gate {
    fn default() -> Gate {
        Gate(tokio::sync::Semaphore::new(0))
    }
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
        ToolOptions::codemode("slack")
    }
    fn definition(&self, _: &DefinitionContext) -> ToolDefinition {
        let (input, output) = match self.name {
            "read_thread" => (
                json!({"type": "object", "properties": {}, "additionalProperties": false}),
                Some(json!({"type": "object", "properties": {"replies": {"type": "number"}}, "required": ["replies"], "additionalProperties": false})),
            ),
            "post" => (
                json!({"type": "object", "properties": {"text": {"type": "string"}, "channel": {"type": "string"}}, "required": ["text"], "additionalProperties": false}),
                None,
            ),
            _ => (json!({"type": "object", "properties": {}, "additionalProperties": false}), None),
        };
        ToolDefinition::new(self.name, "test tool", input).with_output(output)
    }
    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolResult, ToolError> {
        match self.name {
            "read_thread" => {
                ctx.host.ask_permission(ctx.permission("slack_read_thread", vec!["*".into()], None, None)).await.map_err(denied)?;
                if self.slack.failing.load(Ordering::SeqCst) {
                    return Err(ToolError::new("Slack is down"));
                }
                self.slack.reads.fetch_add(1, Ordering::SeqCst);
                self.slack.message_ids.lock().push(ctx.message_id.clone());
                Ok(ToolResult { output: Some(json!({"replies": *self.slack.replies.lock()})), content: vec![], metadata: None })
            }
            "post" => {
                let channel = input.get("channel").and_then(Value::as_str).unwrap_or("general").to_string();
                ctx.host.ask_permission(ctx.permission("slack_post", vec![channel], None, None)).await.map_err(denied)?;
                self.slack.posts.lock().push(input["text"].as_str().unwrap_or_default().to_string());
                Ok(ToolResult::text("posted"))
            }
            _ => {
                self.slack.entered.fetch_add(1, Ordering::SeqCst);
                if let Ok(permit) = self.slack.gate.0.acquire().await {
                    permit.forget();
                }
                Ok(ToolResult::text("waited"))
            }
        }
    }
}

struct H {
    app: Arc<App>,
    engine: Arc<Engine>,
    automations: Arc<Automations>,
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

/// `enabledEntries`, with the `helper` subagent and the hidden `internal` one of `configureAgent`.
fn config(automations: bool, extra: Value) -> Value {
    let mut config = json!({
        "timezone": "America/New_York",
        "automations": automations,
        "agent": {"helper": {"mode": "subagent"}, "internal": {"mode": "subagent", "hidden": true}},
    });
    for (k, v) in extra.as_object().unwrap() {
        config[k] = v.clone();
    }
    config
}

/// An app over `db` (a fresh in-memory one when `None`) with directories `enabled`, `disabled` and `toggle`.
fn harness_with(db: Option<oc_storage::Database>, clock: Clock, tmp: Option<tempfile::TempDir>) -> H {
    unsafe { std::env::set_var("OPENCODE_DISABLE_MODELS_FETCH", "1") };
    let tmp = tmp.unwrap_or_else(|| tempfile::tempdir().unwrap());
    let root = oc_util::fs::canonicalize(tmp.path()).unwrap();
    write_config(&root.join("enabled"), config(true, json!({})));
    write_config(&root.join("disabled"), config(false, json!({})));
    write_config(&root.join("toggle"), config(false, json!({})));
    let db = db.unwrap_or_else(|| oc_storage::Database::memory().unwrap());
    let app = App::new(global(&root.join("g")), db, "2.3.1");
    let services = oc_tools::tools::Services {
        formatter: Arc::new(oc_tools::formatter::Formatter::new()),
        ripgrep: Arc::new(oc_tools::ripgrep::Ripgrep::new(app.global.bin.clone(), reqwest::Client::new())),
        shell: oc_tools::shell::service::ShellService::new(app.global.data.clone(), None),
        websearch: Arc::new(oc_tools::websearch::WebSearch::new(Arc::new(NoStore), reqwest::Client::new())),
        http: reqwest::Client::new(),
    };
    let engine = Engine::new(app.clone(), &services);
    let slack = Arc::new(Slack::default());
    for name in ["read_thread", "post", "wait"] {
        engine.extra_tools.lock().push(Arc::new(SlackTool { name, slack: slack.clone() }));
    }
    let automations = Automations::with_clock(&app, clock);
    automations.set_engine(Arc::downgrade(&engine));
    let prompter =
        Arc::new(TestPrompter { app: app.clone(), wakes: Default::default(), during: Mutex::new(None), fail_next: AtomicBool::new(false) });
    automations.set_prompter(prompter.clone());
    H { app, engine, automations, prompter, slack, root, _tmp: tmp }
}

/// A clock that stands still (ticks are driven by the test).
fn harness() -> H {
    harness_with(None, Clock::fixed(0), None)
}

/// A clock that follows tokio's paused time (the loop sleeps).
fn paused_harness() -> H {
    harness_with(None, Clock::manual(0), None)
}

fn rule(action: &str, resource: &str, effect: &str) -> PermissionRule {
    serde_json::from_value(json!({"action": action, "resource": resource, "effect": effect})).unwrap()
}

fn allow_all() -> Vec<PermissionRule> {
    vec![rule("*", "*", "allow")]
}

fn prompt(text: &str) -> Value {
    json!({"type": "prompt", "prompt": text})
}

fn code(text: &str) -> Value {
    json!({"type": "code", "code": text})
}

fn code_args(text: &str, args: Value) -> Value {
    json!({"type": "code", "code": text, "args": args})
}

fn every(value: &str) -> Value {
    json!({"type": "every", "every": value})
}

fn at(value: &str) -> Value {
    json!({"type": "at", "at": value})
}

/// `messageID(automation, run)`: a code action's handoff carries `_send`.
fn msg(info: &Value, run: i64) -> String {
    let suffix = if info["action"]["type"] == "code" { "_send" } else { "" };
    format!("msg_{}_{run}{suffix}", info["id"].as_str().unwrap())
}

fn id(info: &Value) -> &str {
    info["id"].as_str().unwrap()
}

/// `toMatchObject`: every key of `expected` matches; `null` means absent.
fn subset(actual: &Value, expected: Value) {
    for (k, v) in expected.as_object().unwrap() {
        if v.is_null() {
            assert!(actual.get(k).is_none(), "{k} should be absent in {actual}");
        } else {
            assert_eq!(actual.get(k), Some(v), "{k} in {actual}");
        }
    }
}

fn invalid(result: Result<Value, AutomationError>) -> String {
    match result {
        Err(AutomationError::Invalid(reason)) => reason,
        other => panic!("expected InvalidError, got {other:?}"),
    }
}

fn invocation(session: &str) -> Invocation {
    Invocation {
        session_id: session.into(),
        agent: "build".into(),
        source: Source { kind: "tool".into(), message_id: "msg_author".into(), id: "create_automation".into() },
    }
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
                permissions: Some(permissions.unwrap_or_else(allow_all)),
                ..Default::default()
            })
            .await
            .unwrap()
            .id
    }

    async fn session(&self) -> String {
        self.session_in("enabled", None).await
    }

    /// `(id, sessionID)` of every pending inbox item.
    fn pending(&self) -> Vec<(String, String)> {
        self.app
            .db
            .with(|c| {
                let mut s = c.prepare("SELECT id, session_id FROM session_inbox ORDER BY enqueued_seq")?;
                s.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap()
    }

    fn pending_ids(&self) -> Vec<String> {
        self.pending().into_iter().map(|(id, _)| id).collect()
    }

    fn inbox(&self, id: &str) -> Value {
        let (kind, delivery, payload): (String, String, String) = self
            .app
            .db
            .with(|c| c.query_row("SELECT type, delivery, payload FROM session_inbox WHERE id = ?", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))))
            .unwrap();
        json!({"type": kind, "delivery": delivery, "payload": serde_json::from_str::<Value>(&payload).unwrap()})
    }

    /// Moves pending prompts into the transcript, as a running session would.
    async fn deliver(&self, session: &str) {
        oc_core::inbox::promote(&self.app.bus, &self.app.sessions.locks, session, oc_core::inbox::Scope::Input).await.unwrap();
    }

    fn now(&self) -> i64 {
        self.automations.clock().now()
    }

    fn adjust(&self, ms: i64) {
        self.automations.clock().advance(ms);
    }

    fn set_time(&self, iso: &str) {
        self.automations.clock().set(ms(iso));
    }

    async fn tick(&self) {
        self.automations.tick().await;
    }

    /// `TestClock.adjust("1 minute")` then a tick.
    async fn minute(&self) {
        self.adjust(MINUTE);
        self.tick().await;
    }

    fn get(&self, id: &str) -> Value {
        self.automations.row(id).unwrap().info()
    }

    fn wakes(&self) -> Vec<String> {
        self.prompter.wakes.lock().clone()
    }

    fn clear_wakes(&self) {
        self.prompter.wakes.lock().clear();
    }

    async fn create(&self, input: Value) -> Result<Value, AutomationError> {
        self.automations.create(CreateInput::from_value(&input), None).await
    }

    async fn create_as(&self, input: Value, invocation: &Invocation) -> Result<Value, AutomationError> {
        self.automations.create(CreateInput::from_value(&input), Some(invocation)).await
    }

    fn children(&self, parent: &str) -> Vec<String> {
        self.app
            .db
            .with(|c| {
                let mut s = c.prepare("SELECT id FROM session_v2 WHERE parent_id = ? ORDER BY time_created, id")?;
                s.query_map([parent], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap()
    }

    async fn reload(&self, name: &str) {
        self.engine.reload_location(&self.dir(name)).await.unwrap();
    }
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runs_an_at_automation_once_into_the_creating_session_by_default() {
    let h = harness();
    let s = h.session().await;
    let automation =
        h.create(json!({"sessionID": s, "name": "build", "trigger": at("in 5 minutes"), "action": prompt("check the build")})).await.unwrap();
    let project = h.app.sessions.get(&s).unwrap().project_id;
    subset(
        &automation,
        json!({
            "tz": "America/New_York", "directory": h.dir("enabled"), "projectID": project, "sessionID": s,
            "trigger": {"type": "at", "at": time::format_iso(5 * MINUTE)},
            "action": {"type": "prompt", "prompt": "check the build", "to": {"session": s}},
        }),
    );
    h.adjust(4 * MINUTE);
    h.tick().await;
    assert!(h.pending().is_empty());
    h.minute().await;
    assert_eq!(h.pending_ids(), vec![msg(&automation, 1)]);
    assert_eq!(h.wakes(), vec![s.clone()]);
    let item = h.inbox(&msg(&automation, 1));
    assert_eq!((item["type"].as_str(), item["delivery"].as_str()), (Some("user"), Some("steer")));
    assert_eq!(item["payload"]["text"], "check the build");
    assert_eq!(
        item["payload"]["metadata"],
        json!({"automation": {"id": automation["id"], "name": "build", "run": 1, "dueAt": 5 * MINUTE, "late": false}})
    );
    subset(
        &h.get(id(&automation)),
        json!({"status": "done", "runCount": 1, "lastOutcome": "fired", "lastOutput": format!("Prompted {s}"), "nextDueAt": null}),
    );
    h.adjust(10 * MINUTE);
    h.tick().await;
    assert_eq!(h.pending_ids(), vec![msg(&automation, 1)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parses_at_times_with_offsets_or_in_the_automations_zone() {
    let h = harness();
    h.set_time("2026-11-01T12:00:00Z");
    let s = h.session().await;
    let create = |value: &str, tz: Option<&str>| {
        let mut trigger = at(value);
        if let Some(tz) = tz {
            trigger["tz"] = json!(tz);
        }
        h.create(json!({"sessionID": s, "name": "at", "trigger": trigger, "action": prompt("at")}))
    };
    for (value, tz, expected) in [
        ("2026-11-02T09:30:00-05:00", None, "2026-11-02T14:30:00.000Z"),
        ("2026-11-02T09:30", None, "2026-11-02T14:30:00.000Z"),
        ("2026-11-02 14:30:00Z", None, "2026-11-02T14:30:00.000Z"),
        ("2026-11-02T09:30", Some("Europe/Paris"), "2026-11-02T08:30:00.000Z"),
    ] {
        assert_eq!(iso(&create(value, tz).await.unwrap()["nextDueAt"]), expected, "{value}");
    }
    assert_eq!(invalid(create("2026-11-01T11:00:00Z", None).await), "Time is in the past: 2026-11-01T11:00:00Z");
    assert_eq!(invalid(create("in 0 seconds", None).await), "Invalid time: in 0 seconds");
    assert_eq!(invalid(create("tomorrow", None).await), "Invalid time: tomorrow");
    assert_eq!(invalid(create("in 1 minute", Some("Mars/Base")).await), "Unknown time zone: Mars/Base");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn creates_lists_pauses_resumes_and_deletes() {
    let h = harness();
    let s = h.session().await;
    let automation = h.create(json!({"sessionID": s, "name": "poll", "trigger": every("1 minute"), "action": prompt("poll")})).await.unwrap();
    let project = h.app.sessions.get(&s).unwrap().project_id;
    let ids = |list: Vec<Value>| list.iter().map(|i| i["id"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    assert_eq!(ids(h.automations.list(&ListInput { project_id: Some(project), ..Default::default() }).await.unwrap()), vec![id(&automation)]);
    // 2.4.1: a session filter needs the session.
    assert_eq!(h.automations.list(&ListInput { session_id: Some("ses_other".into()), ..Default::default() }).await, Err(AutomationError::SessionNotFound("ses_other".into())));

    assert_eq!(h.automations.pause(id(&automation)).await.unwrap()["status"], "paused");
    h.adjust(150_000);
    h.tick().await;
    assert!(h.pending().is_empty());
    let resumed = h.automations.resume(id(&automation)).await.unwrap();
    assert_eq!(resumed["status"], "active");
    assert_eq!(resumed["nextDueAt"], 180_000);
    h.automations.remove(id(&automation)).await.unwrap();
    assert_eq!(h.automations.get(id(&automation)).await, Err(AutomationError::NotFound(id(&automation).to_string())));
    assert_eq!(AutomationError::NotFound("aut_x".into()).message(), "Automation not found: aut_x");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejects_tool_created_code_and_child_actions_if_the_session_switched_agents() {
    let h = harness();
    let s = h.session().await;
    h.app.sessions.switch_agent(&s, "helper").unwrap();
    for action in [code("return"), json!({"type": "prompt", "prompt": "triage", "to": {"subagent": "helper"}})] {
        let reason = invalid(h.create_as(json!({"sessionID": s, "name": "stale", "trigger": every("1 minute"), "action": action}), &invocation(&s)).await);
        assert!(reason.contains("current agent"), "{reason}");
    }
    assert!(h.automations.list(&ListInput::default()).await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sends_a_prompt_to_another_existing_session() {
    let h = harness();
    let s = h.session().await;
    let other = h.session().await;
    let automation = h
        .create(json!({"sessionID": s, "name": "nudge", "trigger": every("1 minute"), "action": {"type": "prompt", "prompt": "nudge", "to": {"session": other}}}))
        .await
        .unwrap();
    h.minute().await;
    assert_eq!(h.pending(), vec![(msg(&automation, 1), other.clone())]);
    assert_eq!(h.wakes(), vec![other.clone()]);

    h.app
        .db
        .with(|c| {
            c.execute("INSERT INTO project (id, worktree, sandboxes, time_created, time_updated) VALUES ('prj_elsewhere', '/elsewhere', '[]', 0, 0)", [])?;
            c.execute("UPDATE session_v2 SET project_id = 'prj_elsewhere' WHERE id = ?", [&other])
        })
        .unwrap();
    let reach = h.create(json!({"sessionID": s, "name": "reach", "trigger": every("1 hour"), "action": {"type": "prompt", "prompt": "x", "to": {"session": other}}}));
    assert_eq!(invalid(reach.await), format!("Session {other} is in another project"));
    // A target that moves away after creation pauses the automation rather than prompting outside the project.
    h.minute().await;
    subset(
        &h.get(id(&automation)),
        json!({"status": "paused", "runCount": 1, "lastOutput": format!("Target session {other} moved to another project")}),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn starts_a_child_of_the_creating_session_with_the_agent_on_each_run_without_waking_the_parent() {
    let h = harness();
    let s = h.session().await;
    let automation = h
        .create(json!({"sessionID": s, "name": "triage", "trigger": every("1 minute"), "action": {"type": "prompt", "prompt": "triage the inbox", "to": {"subagent": "helper"}}}))
        .await
        .unwrap();
    assert_eq!(automation["action"], json!({"type": "prompt", "prompt": "triage the inbox", "to": {"subagent": "helper"}}));
    h.minute().await;
    h.minute().await;
    let rows = h.pending();
    assert_eq!(rows.iter().map(|(i, _)| i.clone()).collect::<Vec<_>>(), vec![msg(&automation, 1), msg(&automation, 2)]);
    // One child per run, with an id derived from the run so a retry adopts it rather than starting another.
    let children: Vec<String> = rows.iter().map(|(_, s)| s.clone()).collect();
    assert_eq!(children, vec![format!("ses_{}_1", id(&automation)), format!("ses_{}_2", id(&automation))]);
    // Only the children are woken; the parent hears nothing.
    assert_eq!(h.wakes(), children);
    let child = h.app.sessions.get(&children[0]).unwrap();
    assert_eq!(child.parent_id.as_deref(), Some(s.as_str()));
    assert_eq!(child.agent.as_deref(), Some("helper"));
    assert_eq!(child.title.as_deref(), Some("triage"));
    assert_eq!(child.location.directory, h.dir("enabled"));
    assert!(oc_core::automation::is_run_session(&child.id));
    subset(&h.get(id(&automation)), json!({"runCount": 2, "lastOutput": format!("Started subagent {}", children[1])}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adopts_the_child_a_crashed_run_already_started() {
    let h = harness();
    let s = h.session().await;
    let automation = h
        .create(json!({"sessionID": s, "name": "triage", "trigger": at("in 1 minute"), "action": {"type": "prompt", "prompt": "once", "to": {"subagent": "helper"}}}))
        .await
        .unwrap();
    h.adjust(MINUTE);
    // The previous process started run 1's child, admitted its prompt, and died before advancing the row.
    let child = format!("ses_{}_1", id(&automation));
    h.app
        .sessions
        .create(SessionInput { id: Some(child.clone()), parent_id: Some(s.clone()), agent: Some("helper".into()), title: Some("triage".into()), ..Default::default() })
        .await
        .unwrap();
    h.app.sessions.prompt(&child, PromptInput { id: Some(msg(&automation, 1)), text: "once".into(), ..Default::default() }).await.unwrap();
    h.tick().await;
    assert_eq!(h.pending(), vec![(msg(&automation, 1), child.clone())]);
    assert_eq!(h.children(&s), vec![child]);
    subset(&h.get(id(&automation)), json!({"status": "done", "runCount": 1}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn allows_any_agent_that_can_run_as_a_subagent_and_rejects_hidden_and_primary_only_agents() {
    let h = harness();
    let s = h.session().await;
    let create = |agent: &str| {
        let input = json!({"sessionID": s, "name": "x", "trigger": every("1 hour"), "action": {"type": "prompt", "prompt": "x", "to": {"subagent": agent}}});
        let invocation = invocation(&s);
        let h = &h;
        async move { h.create_as(input, &invocation).await }
    };
    let automation = create("helper").await.unwrap();
    assert_eq!(automation["status"], "active");
    assert_eq!(invalid(create("nobody").await), "Unknown subagent: nobody");
    assert_eq!(invalid(create("internal").await), "Subagent internal is hidden");
    assert_eq!(invalid(create("build").await), "Agent build is primary-only");

    // An agent the creating session's `subagent` permission denies is out too.
    h.app.sessions.set_permissions(&s, vec![rule("*", "*", "allow"), rule("subagent", "helper", "deny")]).unwrap();
    assert_eq!(invalid(create("helper").await), "Cannot start subagent helper: Permission denied: subagent");
    h.app.sessions.set_permissions(&s, vec![rule("*", "*", "allow"), rule("subagent", "helper", "ask")]).unwrap();
    let reason = invalid(create("helper").await);
    assert!(reason.contains("subagent helper: Permission subagent needs approval"), "{reason}");

    // An agent that is gone by the time a run comes up pauses the automation.
    h.app.sessions.set_permissions(&s, allow_all()).unwrap();
    let mut gone = config(true, json!({}));
    gone["agent"] = json!({"internal": {"mode": "subagent", "hidden": true}});
    write_config(&h.root.join("enabled"), gone);
    h.reload("enabled").await;
    h.adjust(60 * MINUTE);
    h.tick().await;
    subset(&h.get(id(&automation)), json!({"status": "paused", "runCount": 0, "lastOutput": "Unknown subagent: helper"}));
    assert!(h.pending().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runs_without_a_session_in_the_given_directory_and_needs_a_target_for_a_prompt() {
    let h = harness();
    // Created in the directory's own project, which a sessionless automation there resolves to.
    let target = h.session().await;
    let dir = h.dir("enabled");
    let automation = h
        .create(json!({"name": "standalone", "directory": dir, "trigger": at("in 1 minute"), "action": {"type": "prompt", "prompt": "hello", "to": {"session": target}}}))
        .await
        .unwrap();
    subset(&automation, json!({"sessionID": null, "directory": dir}));
    h.minute().await;
    assert_eq!(h.pending(), vec![(msg(&automation, 1), target.clone())]);

    let sessionless = |action: Value, directory: &str| json!({"name": "x", "directory": directory, "trigger": every("1 hour"), "action": action});
    assert_eq!(invalid(h.create(sessionless(prompt("x"), &dir)).await), "A prompt action needs `to` when there is no session");
    assert_eq!(
        invalid(h.create(sessionless(json!({"type": "prompt", "prompt": "x", "to": {"subagent": "helper"}}), &dir)).await),
        "A prompt to a subagent needs a session to be its parent"
    );
    assert_eq!(invalid(h.create(sessionless(code("return"), &dir)).await), "A code action needs a session");
    assert_eq!(
        invalid(h.create(sessionless(json!({"type": "prompt", "prompt": "x", "to": {"session": target}}), "relative")).await),
        "Directory must be an absolute path: relative"
    );
    let gone = h.dir("gone");
    assert_eq!(
        invalid(h.create(sessionless(json!({"type": "prompt", "prompt": "x", "to": {"session": target}}), &gone)).await),
        format!("Directory does not exist: {gone}")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stops_at_expires() {
    let h = harness();
    h.set_time("2026-10-26T12:00:00Z");
    let s = h.session().await;
    let automation = h
        .create(json!({"sessionID": s, "name": "poll", "trigger": every("10 minutes"), "expires": "2026-10-26T12:25:00Z", "action": prompt("poll")}))
        .await
        .unwrap();
    assert_eq!(iso(&automation["expires"]), "2026-10-26T12:25:00.000Z");
    for _ in 0..3 {
        h.adjust(10 * MINUTE);
        h.tick().await;
        h.deliver(&s).await;
    }
    subset(&h.get(id(&automation)), json!({"status": "done", "runCount": 2}));
    let late = h.create(json!({"sessionID": s, "name": "late", "trigger": at("in 2 hours"), "expires": "in 1 hour", "action": prompt("x")}));
    assert_eq!(invalid(late.await), "The automation expires before it first runs");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn never_runs_a_slot_that_only_comes_up_after_expires() {
    let h = harness();
    h.set_time("2026-10-26T12:00:00Z");
    let s = h.session().await;
    let poll = h
        .create(json!({"sessionID": s, "name": "poll", "trigger": every("10 minutes"), "expires": "2026-10-26T12:25:00Z", "action": prompt("poll")}))
        .await
        .unwrap();
    let once =
        h.create(json!({"sessionID": s, "name": "once", "trigger": at("in 10 minutes"), "expires": "in 20 minutes", "action": prompt("once")})).await.unwrap();
    h.automations.pause(id(&once)).await.unwrap();
    h.adjust(30 * MINUTE);
    h.tick().await;
    subset(&h.get(id(&poll)), json!({"status": "done", "runCount": 0, "lastOutcome": "expired"}));
    subset(&h.automations.resume(id(&once)).await.unwrap(), json!({"status": "done", "runCount": 0}));
    h.tick().await;
    assert!(h.pending().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drops_a_runs_result_when_another_process_recorded_a_run_meanwhile() {
    let h = harness();
    let s = h.session().await;
    let automation = h.create(json!({"sessionID": s, "name": "poll", "trigger": every("1 minute"), "action": prompt("poll")})).await.unwrap();
    h.adjust(MINUTE);
    // Another leader records this run while ours is still admitting it.
    let db = h.app.db.clone();
    let row = id(&automation).to_string();
    *h.prompter.during.lock() = Some(Box::new(move || {
        db.with(|c| c.execute("UPDATE automation SET run_count = 1, last_output = 'other' WHERE id = ?", [&row])).unwrap();
    }));
    h.tick().await;
    subset(&h.get(id(&automation)), json!({"runCount": 1, "lastOutput": "other"}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runs_once_after_downtime_marked_late_and_keeps_the_phase() {
    let h = harness();
    h.set_time("2026-10-26T12:00:00Z");
    let s = h.session().await;
    let automation = h.create(json!({"sessionID": s, "name": "poll", "trigger": every("10 minutes"), "action": prompt("poll")})).await.unwrap();
    h.set_time("2026-10-26T13:05:00Z");
    h.tick().await;
    h.tick().await;
    assert_eq!(h.pending_ids(), vec![msg(&automation, 1)]);
    assert_eq!(h.inbox(&msg(&automation, 1))["payload"]["metadata"]["automation"]["late"], true);
    let info = h.get(id(&automation));
    subset(&info, json!({"runCount": 1, "lastOutcome": "late"}));
    assert_eq!(iso(&info["nextDueAt"]), "2026-10-26T13:10:00.000Z");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn skips_a_run_while_the_previous_prompt_is_unconsumed() {
    let h = harness();
    let s = h.session().await;
    let automation = h.create(json!({"sessionID": s, "name": "poll", "trigger": every("1 minute"), "action": prompt("poll")})).await.unwrap();
    h.minute().await;
    h.minute().await;
    assert_eq!(h.pending_ids(), vec![msg(&automation, 1)]);
    subset(&h.get(id(&automation)), json!({"runCount": 1, "lastOutcome": "skipped"}));
    // The skip wakes the session again in case the wake after admission was lost.
    assert_eq!(h.wakes(), vec![s.clone(), s.clone()]);
    h.deliver(&s).await;
    h.minute().await;
    assert_eq!(h.pending_ids(), vec![msg(&automation, 2)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn does_not_duplicate_a_prompt_admitted_before_a_crash() {
    let h = harness();
    let s = h.session().await;
    let automation = h.create(json!({"sessionID": s, "name": "once", "trigger": at("in 1 minute"), "action": prompt("once")})).await.unwrap();
    h.adjust(MINUTE);
    // The previous process admitted run 1 and died before advancing the row.
    h.app.sessions.prompt(&s, PromptInput { id: Some(msg(&automation, 1)), text: "once".into(), ..Default::default() }).await.unwrap();
    h.tick().await;
    assert_eq!(h.pending_ids(), vec![msg(&automation, 1)]);
    assert_eq!(h.wakes(), vec![s.clone()]);
    subset(&h.get(id(&automation)), json!({"status": "done", "runCount": 1}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keeps_automations_when_their_creating_session_is_deleted_and_pauses_when_a_target_is_gone() {
    let h = harness();
    let s = h.session().await;
    let other = h.session().await;
    let create = |owner: &str, name: &str, action: Value| h.create(json!({"sessionID": owner, "name": name, "trigger": every("1 minute"), "action": action}));
    let elsewhere = create(&other, "elsewhere", json!({"type": "prompt", "prompt": "go", "to": {"session": other}})).await.unwrap();
    let child = create(&s, "child", json!({"type": "prompt", "prompt": "go", "to": {"subagent": "helper"}})).await.unwrap();
    let targeted = create(&other, "targeted", json!({"type": "prompt", "prompt": "go", "to": {"session": s}})).await.unwrap();
    let coded = create(&s, "coded", code("return")).await.unwrap();
    h.app.db.with(|c| c.execute("DELETE FROM session_v2 WHERE id = ?", [&s])).unwrap();
    let ids: Vec<String> = h.automations.list(&ListInput::default()).await.unwrap().iter().map(|i| i["id"].as_str().unwrap().to_string()).collect();
    assert_eq!(ids, [&elsewhere, &child, &targeted, &coded].map(|i| id(i).to_string()));

    h.minute().await;
    subset(&h.get(id(&elsewhere)), json!({"status": "active", "runCount": 1}));
    subset(&h.get(id(&child)), json!({"status": "paused", "runCount": 0, "lastOutput": "The session that created this automation no longer exists"}));
    subset(&h.get(id(&targeted)), json!({"status": "paused", "runCount": 0, "lastOutput": format!("Target session {s} no longer exists")}));
    subset(
        &h.get(id(&coded)),
        json!({"status": "paused", "runCount": 0, "lastOutput": "The session that authorized this automation's tools no longer exists"}),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejects_creation_when_automations_are_off() {
    let h = harness();
    let s = h.session_in("disabled", None).await;
    let created = h.create(json!({"sessionID": s, "name": "x", "trigger": at("in 1 minute"), "action": prompt("x")})).await;
    assert_eq!(created, Err(AutomationError::Disabled));
    assert_eq!(AutomationError::Disabled.message(), "Automations are disabled. Set automations to true in config to enable them.");
    assert!(h.automations.list(&ListInput::default()).await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rechecks_hourly_while_its_location_has_the_flag_off_then_catches_up_once_it_is_on() {
    let h = harness();
    write_config(&h.root.join("toggle"), config(true, json!({})));
    let s = h.session_in("toggle", None).await;
    let automation = h.create(json!({"sessionID": s, "name": "poll", "trigger": every("1 minute"), "action": prompt("poll")})).await.unwrap();
    let id = id(&automation).to_string();
    write_config(&h.root.join("toggle"), config(false, json!({})));
    h.reload("toggle").await;
    h.minute().await;
    subset(&h.get(&id), json!({"status": "active", "runCount": 0, "lastOutcome": "disabled"}));
    assert!(h.pending().is_empty());
    // Within the hour the flag is not read again.
    write_config(&h.root.join("toggle"), config(true, json!({})));
    h.reload("toggle").await;
    h.adjust(59 * MINUTE);
    h.tick().await;
    assert!(h.pending().is_empty());
    // Once the hour has passed, the missed slot runs once, late, and keeps the phase.
    h.adjust(2 * MINUTE);
    h.tick().await;
    assert_eq!(h.pending_ids(), vec![format!("msg_{id}_1")]);
    subset(&h.get(&id), json!({"status": "active", "runCount": 1, "lastOutcome": "late", "nextDueAt": 63 * MINUTE}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keeps_the_run_count_when_paused_while_a_prompt_is_being_admitted() {
    let h = harness();
    let s = h.session().await;
    let automation = h.create(json!({"sessionID": s, "name": "poll", "trigger": every("1 minute"), "action": prompt("poll")})).await.unwrap();
    h.adjust(MINUTE);
    let automations = h.automations.clone();
    let pause = id(&automation).to_string();
    *h.prompter.during.lock() = Some(Box::new(move || {
        tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(automations.pause(&pause))).unwrap();
    }));
    h.tick().await;
    subset(&h.get(id(&automation)), json!({"status": "paused", "runCount": 1}));
    h.deliver(&s).await;
    h.automations.resume(id(&automation)).await.unwrap();
    h.minute().await;
    assert_eq!(h.pending_ids(), vec![msg(&automation, 2)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn does_not_start_queued_timers_that_were_paused_deleted_or_rescheduled() {
    let h = harness();
    let s = h.session().await;
    // Four earlier actions occupy the workers after the tick has selected every due timer.
    let mut first = Vec::new();
    for n in 1..=4 {
        let input = json!({"sessionID": s, "name": format!("first {n}"), "trigger": at("in 1 second"), "action": code("await tools.slack.wait({}); return")});
        first.push(h.create(input).await.unwrap());
    }
    let mut queued = Vec::new();
    for name in ["paused", "deleted", "rescheduled"] {
        let action = code(&format!("await tools.slack.post({{ text: \"{name}\" }})"));
        queued.push(h.create(json!({"sessionID": s, "name": name, "trigger": every("1 minute"), "action": action})).await.unwrap());
    }
    h.adjust(MINUTE);
    let automations = h.automations.clone();
    let ticking = tokio::spawn(async move { automations.tick().await });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while h.slack.entered.load(Ordering::SeqCst) < 4 {
        assert!(std::time::Instant::now() < deadline, "the first runs never started");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    h.automations.pause(id(&queued[0])).await.unwrap();
    h.automations.remove(id(&queued[1])).await.unwrap();
    h.automations.pause(id(&queued[2])).await.unwrap();
    h.automations.resume(id(&queued[2])).await.unwrap();
    h.slack.gate.0.add_permits(4);
    ticking.await.unwrap();
    assert!(h.slack.posts.lock().is_empty());
    assert_eq!(h.get(id(&queued[0]))["runCount"], 0);
    assert_eq!(h.get(id(&queued[2]))["runCount"], 0);
    for timer in &first {
        let info = h.get(id(timer));
        assert_eq!(info["runCount"], 1, "{info}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pauses_with_a_reason_when_its_directory_disappears() {
    let h = harness();
    write_config(&h.root.join("gone"), config(true, json!({})));
    let s = h.session_in("gone", None).await;
    let automation = h.create(json!({"sessionID": s, "name": "poll", "trigger": every("1 minute"), "action": prompt("poll")})).await.unwrap();
    std::fs::remove_dir_all(h.root.join("gone")).unwrap();
    h.minute().await;
    let gone = h.dir("gone");
    subset(
        &h.get(id(&automation)),
        json!({"status": "paused", "lastOutcome": "failed", "lastOutput": format!("Directory no longer exists: {gone}")}),
    );
    let again = h.create(json!({"sessionID": s, "name": "x", "trigger": every("1 hour"), "action": prompt("x")}));
    assert_eq!(invalid(again.await), format!("Directory does not exist: {gone}"));
}

#[tokio::test(start_paused = true)]
async fn run_sleeps_until_the_next_automation_is_due() {
    let h = paused_harness();
    let s = h.session().await;
    let runner = tokio::spawn(h.automations.clone().run());
    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    let automation = h.create(json!({"sessionID": s, "name": "soon", "trigger": at("in 10 seconds"), "action": prompt("soon")})).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(9_000)).await;
    assert!(h.pending().is_empty());
    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
    assert_eq!(h.pending_ids(), vec![msg(&automation, 1)]);
    runner.abort();
    let _ = runner.await;
    // Leaving releases the lease.
    assert_eq!(h.app.db.kv_get("automation:leader").unwrap(), None);
}

#[tokio::test(start_paused = true)]
async fn wakes_a_session_on_start_when_its_automation_prompt_was_never_claimed() {
    let h = paused_harness();
    let s = h.session().await;
    h.create(json!({"sessionID": s, "name": "once", "trigger": at("in 1 minute"), "action": prompt("once")})).await.unwrap();
    h.adjust(MINUTE);
    h.tick().await;
    // The process died before the session claimed the prompt, so that wake was lost.
    h.clear_wakes();
    let runner = tokio::spawn(h.automations.clone().run());
    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    assert_eq!(h.wakes(), vec![s.clone()]);
    // Nothing active: the lease is released.
    assert_eq!(h.app.db.kv_get("automation:leader").unwrap(), None);
    runner.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backs_off_for_five_minutes_when_a_prompt_fails_then_runs_once() {
    let h = harness();
    let s = h.session().await;
    let automation = h.create(json!({"sessionID": s, "name": "flaky", "trigger": at("in 1 minute"), "action": prompt("flaky")})).await.unwrap();
    h.adjust(MINUTE);
    h.prompter.fail_next.store(true, Ordering::SeqCst);
    h.tick().await;
    subset(&h.get(id(&automation)), json!({"status": "active", "runCount": 0, "lastOutcome": "failed"}));
    // Not retried on every tick.
    h.clear_wakes();
    h.adjust(4 * MINUTE);
    h.tick().await;
    assert!(h.wakes().is_empty());
    h.minute().await;
    assert_eq!(h.wakes(), vec![s.clone()]);
    subset(&h.get(id(&automation)), json!({"status": "done", "runCount": 1, "lastOutcome": "late"}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keeps_cron_at_the_same_wall_clock_time_across_dst_changes() {
    let h = harness();
    let s = h.session().await;
    let cron = |expr: &str| {
        h.create(json!({"sessionID": s, "name": expr, "trigger": {"type": "cron", "cron": expr, "tz": "America/New_York"}, "action": prompt(expr)}))
    };
    // On 2026-11-01 New York falls back at 06:00Z, so 1:00-1:59 runs twice: 05:00Z EDT, then 06:00Z EST.
    h.set_time("2026-11-01T05:50:00Z");
    // 1:50 EDT ran the last 1:xx slot; the next is 2:00 EST, an hour and ten minutes later.
    assert_eq!(iso(&cron("*/10 * * * *").await.unwrap()["nextDueAt"]), "2026-11-01T07:00:00.000Z");
    h.set_time("2026-11-01T05:00:00Z");
    let fixed = cron("30 1 * * *").await.unwrap();
    h.set_time("2026-11-01T05:30:00Z");
    h.tick().await;
    assert_eq!(iso(&h.get(id(&fixed))["nextDueAt"]), "2026-11-02T06:30:00.000Z");
    assert_eq!(invalid(cron("0 0 31 2 *").await), "cron never runs: 0 0 31 2 *");
    assert_eq!(invalid(cron("* * * * * *").await), "cron must run at most once a minute");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn validates_durations_at_creation() {
    let h = harness();
    let s = h.session().await;
    let create = |trigger: Value, action: Value| h.create(json!({"sessionID": s, "name": "x", "trigger": trigger, "action": action}));
    assert_eq!(invalid(create(every("soon"), prompt("x")).await), "Invalid duration: soon");
    assert_eq!(invalid(create(every("0 seconds"), prompt("x")).await), "Duration must be positive: 0 seconds");
    assert_eq!(invalid(create(every("30 seconds"), prompt("x")).await), "every must be at least 1 minute");
    assert_eq!(invalid(create(every("10 seconds"), code("return")).await), "every must be at least 15 seconds");
    assert_eq!(create(every("15 seconds"), code("return")).await.unwrap()["status"], "active");
    let event = json!({"type": "event", "server": "ci", "event": "build.changed"});
    // 2.4.1 needs no flag: an event watch is checked like any other, and refused outside a tool call.
    assert_eq!(invalid(create(event, prompt("x")).await), "Create an event automation from a tool call in a local session");
}

/// An event watch needs a tool call in a local session, then a configured server offering push events (the event
/// cases are in `automation_event.rs`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn event_watches_need_a_tool_call_and_a_server_offering_events() {
    let h = harness();
    write_config(&h.root.join("enabled"), config(true, json!({})));
    let s = h.session().await;
    let input =
        json!({"sessionID": s, "name": "watch", "trigger": {"type": "event", "server": "ci", "event": "build.changed"}, "action": prompt("Check the build")});
    assert_eq!(invalid(h.create(input.clone()).await), "Create an event automation from a tool call in a local session");
    assert_eq!(invalid(h.create_as(input, &invocation(&s)).await), "MCP server not found: ci");
    assert!(h.automations.list(&ListInput::default()).await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn does_not_call_a_tool_while_another_server_holds_the_lease_then_takes_over_when_it_expires() {
    let h = harness();
    let s = h.session().await;
    let now = h.now();
    h.app.db.kv_set("automation:leader", &json!({"owner": "other-server", "until": now + 60_000})).unwrap();
    let action = code("await tools.slack.post({ text: \"ready\" })");
    h.create(json!({"sessionID": s, "name": "one post", "trigger": at("in 30 seconds"), "action": action})).await.unwrap();
    h.adjust(30_000);
    h.tick().await;
    assert!(h.slack.posts.lock().is_empty());
    h.adjust(30_000);
    h.tick().await;
    assert_eq!(*h.slack.posts.lock(), vec!["ready".to_string()]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn leader_lease_lets_one_process_run() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("db.sqlite");
    let data = tmp.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let a = harness_with(Some(oc_storage::Database::open(&path, &data).unwrap()), Clock::fixed(0), None);
    let b = harness_with(Some(oc_storage::Database::open(&path, &data).unwrap()), Clock::fixed(0), None);
    assert!(a.automations.claim_leader(0));
    assert!(!b.automations.claim_leader(1_000), "a live lease belongs to its owner");
    assert!(a.automations.claim_leader(20_000), "the owner renews");
    let lease = a.app.db.kv_get("automation:leader").unwrap().unwrap();
    assert_eq!(lease, json!({"owner": a.automations.owner(), "until": 80_000}));
    assert!(!b.automations.claim_leader(79_999));
    assert!(b.automations.claim_leader(80_000), "an expired lease is taken over");
    b.automations.release_leader();
    assert_eq!(a.app.db.kv_get("automation:leader").unwrap(), None);
    a.automations.release_leader();
}

#[tokio::test(start_paused = true)]
async fn picks_up_an_automation_created_by_another_process_sharing_the_db_while_idle() {
    let h = paused_harness();
    let s = h.session().await;
    let project = h.app.sessions.get(&s).unwrap().project_id;
    let runner = tokio::spawn(h.automations.clone().run());
    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    // Insert directly into SQLite, so this process's doorbell is never opened.
    let now = h.now();
    let dir = h.dir("enabled");
    h.app
        .db
        .with(|c| {
            c.execute(
                "INSERT INTO automation (id, project_id, session_id, directory, name, action, \"trigger\", tz, next_due_at, time_created, time_updated) \
                 VALUES ('aut_external', ?1, ?2, ?3, 'external', ?4, ?5, 'UTC', ?6, ?7, ?7)",
                rusqlite::params![
                    project,
                    s,
                    dir,
                    json!({"type": "prompt", "prompt": "Cross-process wake.", "to": {"session": s}}).to_string(),
                    json!({"type": "at", "at": time::format_iso(now + 10_000)}).to_string(),
                    now + 10_000,
                    now
                ],
            )
        })
        .unwrap();
    // The bounded sleep (30 s) wakes the idle loop, which claims leadership and runs the row.
    tokio::time::sleep(std::time::Duration::from_millis(30_100)).await;
    runner.abort();
    subset(&h.get("aut_external"), json!({"status": "done", "runCount": 1, "lastOutcome": "fired"}));
}

#[tokio::test(start_paused = true)]
async fn two_processes_on_one_database_run_an_automation_once() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("db.sqlite");
    let data = tmp.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let a = harness_with(Some(oc_storage::Database::open(&path, &data).unwrap()), Clock::manual(0), None);
    let b = harness_with(Some(oc_storage::Database::open(&path, &data).unwrap()), Clock::manual(0), Some(tempfile::tempdir().unwrap()));
    // The same session row is visible to both; its directory must exist for each process's config.
    let s = a.session().await;
    let automation = a
        .create(json!({"sessionID": s, "name": "poll", "trigger": every("1 minute"), "expires": "in 3 minutes", "action": prompt("poll")}))
        .await
        .unwrap();
    let ra = tokio::spawn(a.automations.clone().run());
    let rb = tokio::spawn(b.automations.clone().run());
    for _ in 0..4 {
        tokio::time::sleep(std::time::Duration::from_millis(60_000)).await;
        a.deliver(&s).await;
    }
    ra.abort();
    rb.abort();
    subset(&a.get(id(&automation)), json!({"status": "done", "runCount": 3}));
    assert_eq!(a.wakes().len() + b.wakes().len(), 3, "a: {:?}, b: {:?}", a.wakes(), b.wakes());
}

/// An automation 2.3.1 wrote, or its migration copied from a 2.3.0 schedule, lists and runs here; the info encodes
/// the stored trigger in schema key order.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reads_rows_the_migration_copied_from_schedules() {
    let h = harness();
    let s = h.session().await;
    let project = h.app.sessions.get(&s).unwrap().project_id;
    let dir = h.dir("enabled");
    h.app
        .db
        .with(|c| {
            c.execute(
                "INSERT INTO automation (id, project_id, session_id, directory, name, \"trigger\", tz, action, next_due_at, run_count, time_created, time_updated) \
                 VALUES ('aut_copied', ?1, ?2, ?3, 'poll', '{\"tz\":\"UTC\",\"every\":\"1 minute\",\"type\":\"every\"}', 'UTC', ?4, 60000, 2, 0, 0)",
                rusqlite::params![project, s, dir, json!({"type": "prompt", "prompt": "poll", "to": {"session": s}}).to_string()],
            )
        })
        .unwrap();
    let info = h.get("aut_copied");
    assert_eq!(serde_json::to_string(&info["trigger"]).unwrap(), r#"{"type":"every","every":"1 minute","tz":"UTC"}"#);
    h.minute().await;
    assert_eq!(h.pending_ids(), vec!["msg_aut_copied_3".to_string()]);
    subset(&h.get("aut_copied"), json!({"runCount": 3, "nextDueAt": 2 * MINUTE}));
}

const WATCH_THREAD: &str = r#"
      const thread = await tools.slack.read_thread({})
      console.log("replies: " + thread.replies)
      if (state !== null && thread.replies > state.replies) await tools.slack.post({ text: args.channel + ": " + thread.replies })
      if (thread.replies >= 10) return { done: true }
      return { state: { replies: thread.replies, runs } }
    "#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_keeps_state_across_runs_stays_quiet_without_a_return_and_stops_when_done() {
    let h = harness();
    let s = h.session().await;
    *h.slack.replies.lock() = 3;
    let action = code_args(WATCH_THREAD, json!({"channel": "#ops"}));
    let automation = h.create(json!({"sessionID": s, "name": "thread", "trigger": every("1 minute"), "action": action})).await.unwrap();
    // Nothing runs at creation.
    assert_eq!(h.slack.reads.load(Ordering::SeqCst), 0);
    h.minute().await;
    subset(
        &h.get(id(&automation)),
        json!({"status": "active", "runCount": 1, "lastOutcome": "fired", "lastOutput": "replies: 3", "state": {"replies": 3, "runs": 0}}),
    );
    assert!(h.slack.posts.lock().is_empty());
    *h.slack.replies.lock() = 4;
    h.minute().await;
    assert_eq!(*h.slack.posts.lock(), vec!["#ops: 4".to_string()]);
    subset(&h.get(id(&automation)), json!({"runCount": 2, "state": {"replies": 4, "runs": 1}}));
    // No send was returned.
    assert!(h.pending().is_empty());
    *h.slack.replies.lock() = 10;
    h.minute().await;
    // `done` without `state` keeps the previous state.
    subset(&h.get(id(&automation)), json!({"status": "done", "runCount": 3, "nextDueAt": null, "state": {"replies": 4, "runs": 1}}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_that_returns_nothing_leaves_state_unchanged() {
    let h = harness();
    let s = h.session().await;
    let action = code("if (state === null) return { state: { seen: input.runs } }");
    let automation = h.create(json!({"sessionID": s, "name": "quiet", "trigger": every("1 minute"), "action": action})).await.unwrap();
    for _ in 0..3 {
        h.minute().await;
    }
    subset(&h.get(id(&automation)), json!({"status": "active", "runCount": 3, "state": {"seen": 0}, "lastOutput": null}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_keeps_state_on_failure_retries_a_minute_later_and_pauses_after_3_failed_runs() {
    let h = harness();
    let s = h.session().await;
    let action = code("const t = await tools.slack.read_thread({}); return { state: { replies: t.replies } }");
    let automation = h.create(json!({"sessionID": s, "name": "thread", "trigger": every("1 minute"), "action": action})).await.unwrap();
    *h.slack.replies.lock() = 2;
    h.minute().await;
    assert_eq!(h.get(id(&automation))["state"], json!({"replies": 2}));
    h.slack.failing.store(true, Ordering::SeqCst);
    *h.slack.replies.lock() = 5;
    for _ in 0..2 {
        h.minute().await;
        let info = h.get(id(&automation));
        subset(&info, json!({"status": "active", "runCount": 1, "lastOutcome": "failed", "state": {"replies": 2}}));
        assert!(info["lastOutput"].as_str().unwrap().contains("Slack is down"), "{info}");
    }
    h.minute().await;
    let info = h.get(id(&automation));
    subset(&info, json!({"status": "paused", "runCount": 1, "lastOutcome": "failed", "state": {"replies": 2}}));
    assert!(info["lastOutput"].as_str().unwrap().contains("Paused after 3 failed runs"), "{info}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_pauses_at_once_when_a_run_returns_an_invalid_shape() {
    let h = harness();
    let s = h.session().await;
    let automation = h.create(json!({"sessionID": s, "name": "bad", "trigger": every("1 minute"), "action": code("return \"not-a-result\"")})).await.unwrap();
    h.minute().await;
    subset(
        &h.get(id(&automation)),
        json!({
            "status": "paused", "runCount": 0, "lastOutcome": "failed",
            "lastOutput": "Paused: An automation must return nothing or { state?, done?, send? }: SchemaError(Expected Automation.CodeResult | null)",
        }),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_stays_quiet_while_pending_then_admits_a_progress_and_a_terminal_send_to_the_creator() {
    let h = harness();
    let s = h.session().await;
    let action = code(
        r#"
          const t = await tools.slack.read_thread({})
          if (!t.replies) return
          if (t.replies === 1) return { state: "in progress", send: { prompt: "First reply" } }
          console.log("checks complete")
          return { done: true, send: { prompt: "All checks passed" } }
        "#,
    );
    let automation = h.create(json!({"sessionID": s, "name": "checks", "trigger": every("1 minute"), "action": action})).await.unwrap();
    h.minute().await;
    assert!(h.wakes().is_empty());
    assert!(h.pending().is_empty());
    assert!(h.get(id(&automation)).get("lastOutput").is_none());
    *h.slack.replies.lock() = 1;
    h.minute().await;
    subset(&h.get(id(&automation)), json!({"status": "active", "runCount": 2, "state": "in progress", "lastOutput": format!("Prompted {s}")}));
    *h.slack.replies.lock() = 2;
    h.minute().await;
    assert_eq!(h.pending(), vec![(msg(&automation, 2), s.clone()), (msg(&automation, 3), s.clone())]);
    assert_eq!(h.wakes(), vec![s.clone(), s.clone()]);
    let item = h.inbox(&msg(&automation, 3));
    assert_eq!((item["type"].as_str(), item["delivery"].as_str()), (Some("user"), Some("steer")));
    assert_eq!(item["payload"]["text"], "All checks passed");
    subset(&item["payload"]["metadata"]["automation"], json!({"id": automation["id"], "run": 3}));
    subset(
        &h.get(id(&automation)),
        json!({"status": "done", "runCount": 3, "state": "in progress", "lastOutput": format!("Prompted {s}\nchecks complete")}),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_hands_a_failed_check_to_one_quiet_child_with_the_creators_permissions() {
    let h = harness();
    let permissions = vec![rule("*", "*", "allow"), rule("edit", "*", "deny")];
    let s = h.session_in("enabled", Some(permissions.clone())).await;
    let action = code(
        r#"
          const t = await tools.slack.read_thread({})
          if (t.replies < 0) return { send: { to: { subagent: "helper" }, prompt: "Investigate the failed check" } }
        "#,
    );
    let automation = h.create(json!({"sessionID": s, "name": "triage", "trigger": every("1 minute"), "action": action})).await.unwrap();
    *h.slack.replies.lock() = -1;
    h.minute().await;
    let child = format!("ses_{}_1", id(&automation));
    assert_eq!(h.wakes(), vec![child.clone()]);
    assert_eq!(h.pending(), vec![(msg(&automation, 1), child.clone())]);
    assert_eq!(*h.slack.message_ids.lock(), vec![format!("msg_{}_1", id(&automation))]);
    let info = h.app.sessions.get(&child).unwrap();
    assert_eq!((info.agent.as_deref(), info.parent_id.as_deref()), (Some("helper"), Some(s.as_str())));
    assert_eq!(info.permissions, Some(permissions));
    let list = h.automations.list(&ListInput { session_id: Some(s.clone()), ..Default::default() }).await.unwrap();
    assert_eq!(list.len(), 1);
    subset(&list[0], json!({"id": automation["id"], "status": "active", "runCount": 1, "lastOutput": format!("Started subagent {child}")}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_keeps_the_handoff_visible_and_logs_within_the_utf8_output_budget() {
    let h = harness();
    let s = format!("ses_{}", "x".repeat(128));
    let location = Some(LocationRef { directory: h.dir("enabled"), workspace_id: None });
    h.app.sessions.create(SessionInput { id: Some(s.clone()), location, permissions: Some(allow_all()), ..Default::default() }).await.unwrap();
    let action = code("console.log(\"界\".repeat(10900)); return { send: { prompt: \"ready\" } }");
    let automation = h.create(json!({"sessionID": s, "name": "verbose", "trigger": every("1 minute"), "action": action})).await.unwrap();
    h.minute().await;
    let output = h.get(id(&automation))["lastOutput"].as_str().unwrap_or_default().to_string();
    assert!(output.starts_with(&format!("Prompted {s}\n界")), "{}", output.chars().take(60).collect::<String>());
    assert!(output.ends_with("界…"));
    assert!(output.len() <= 32 * 1024);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_bounds_failed_output_including_the_pause_reason_in_utf8_bytes() {
    let h = harness();
    let s = h.session().await;
    let action = code("console.log(\"starting check\"); throw new Error(\"check failed: \" + \"🧪\".repeat(20000))");
    let automation = h.create(json!({"sessionID": s, "name": "verbose failure", "trigger": every("1 minute"), "action": action})).await.unwrap();
    for strike in 1..=3 {
        h.minute().await;
        let info = h.get(id(&automation));
        let output = info["lastOutput"].as_str().unwrap_or_default();
        assert!(output.len() <= 32 * 1024);
        assert_eq!(info["status"], if strike == 3 { "paused" } else { "active" });
        assert!(output.contains("check failed: "));
        assert!(output.ends_with("🧪…"));
        if strike == 3 {
            assert!(output.starts_with("Paused after 3 failed runs: "), "{}", output.chars().take(40).collect::<String>());
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_bad_targets_and_session_or_agent_policy_never_deliver_or_commit_the_rest_of_the_result() {
    let h = harness();
    let s = h.session_in("enabled", Some(vec![rule("*", "*", "allow"), rule("subagent", "helper", "ask")])).await;
    let other = h.session().await;
    let nested = h.app.sessions.create(SessionInput { parent_id: Some(s.clone()), ..Default::default() }).await.unwrap().id;
    let program = r#"if (runs === 0) return { state: "before" }; return { state: "after", done: true, send: { prompt: "act", to: args } }"#;
    let create = |owner: &str, to: Value| h.create(json!({"sessionID": owner, "name": "invalid", "trigger": every("1 minute"), "action": code_args(program, to)}));
    let mut rows = Vec::new();
    for to in [json!({"session": other}), json!({"subagent": "internal"}), json!({"subagent": "build"}), json!({"subagent": "missing"}), json!({"subagent": "helper"})] {
        rows.push(create(&s, to).await.unwrap());
    }
    // Explicitly allow this child on the nested session: depth alone must still block it.
    h.app.sessions.set_permissions(&nested, vec![rule("*", "*", "allow"), rule("subagent", "helper", "allow")]).unwrap();
    let deep = create(&nested, json!({"subagent": "helper"})).await.unwrap();
    let denied = h.session_in("enabled", Some(vec![rule("*", "*", "allow"), rule("subagent", "helper", "deny")])).await;
    let blocked = create(&denied, json!({"subagent": "helper"})).await.unwrap();
    h.minute().await;
    h.minute().await;
    for row in rows.iter().chain([&deep, &blocked]) {
        let info = h.get(id(row));
        subset(&info, json!({"status": "paused", "lastOutcome": "failed", "runCount": 1, "state": "before"}));
        assert!(info["lastOutput"].as_str().unwrap().contains("Paused:"), "{info}");
    }
    assert_eq!(h.get(id(&deep))["lastOutput"], "Paused: Subagent depth limit reached (1)");
    assert_eq!(h.get(id(&blocked))["lastOutput"], "Paused: Cannot start subagent helper: Permission denied: subagent");
    let outputs: Vec<String> = rows.iter().map(|r| h.get(id(r))["lastOutput"].as_str().unwrap().to_string()).collect();
    assert!(outputs[0].contains("must return nothing or"), "{}", outputs[0]);
    assert_eq!(outputs[1..4], ["Paused: Subagent internal is hidden", "Paused: Agent build is primary-only", "Paused: Unknown subagent: missing"]);
    assert!(outputs[4].contains("subagent helper: Permission subagent needs approval"), "{}", outputs[4]);
    assert!(h.pending().is_empty());
    assert!(h.wakes().is_empty());
    assert_eq!(h.children(&s), vec![nested.clone()]);
    assert!(h.children(&nested).is_empty());
    assert!(h.children(&denied).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_retries_failed_admission_without_committing_state_or_done_then_settles_the_handoff_once() {
    let h = harness();
    let s = h.session().await;
    let other = h.session().await;
    let action = code("return { state: 42, done: true, send: { prompt: \"ready\" } }");
    let automation = h.create(json!({"sessionID": s, "name": "retry", "trigger": every("1 minute"), "action": action})).await.unwrap();
    let item = msg(&automation, 1);
    // A cross-session conflict rejects the admission, before any wake. Remove the conflicting item to retry.
    h.app.sessions.prompt(&other, PromptInput { id: Some(item.clone()), text: "other".into(), ..Default::default() }).await.unwrap();
    h.minute().await;
    assert_eq!(h.pending(), vec![(item.clone(), other.clone())]);
    assert!(h.wakes().is_empty());
    subset(&h.get(id(&automation)), json!({"status": "active", "runCount": 0, "state": null, "lastOutcome": "failed"}));
    oc_core::inbox::cancel(&h.app.bus, &h.app.sessions.locks, &other, &item).await.unwrap();
    h.adjust(59_000);
    h.tick().await;
    assert!(h.pending().is_empty());
    h.adjust(1_000);
    h.tick().await;
    assert_eq!(h.pending(), vec![(item.clone(), s.clone())]);
    assert_eq!(h.wakes(), vec![s.clone()]);
    subset(&h.get(id(&automation)), json!({"status": "done", "runCount": 1, "state": 42}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_pauses_on_a_missing_handoff_directory_without_rerunning_the_check() {
    let h = harness();
    let s = h.session().await;
    let action = code("await tools.slack.post({ text: \"checked\" }); return { state: 1, done: true, send: { prompt: \"ready\" } }");
    let automation = h.create(json!({"sessionID": s, "name": "gone", "trigger": every("1 minute"), "action": action})).await.unwrap();
    // The code's authorized location remains, but its creating session now points at a missing worktree.
    let missing = h.dir("missing");
    h.app.db.with(|c| c.execute("UPDATE session_v2 SET directory = ? WHERE id = ?", [&missing, &s])).unwrap();
    h.minute().await;
    let info = h.get(id(&automation));
    subset(&info, json!({"status": "paused", "runCount": 0, "state": null}));
    assert!(info["lastOutput"].as_str().unwrap().contains(&missing), "{info}");
    h.minute().await;
    assert_eq!(*h.slack.posts.lock(), vec!["checked".to_string()]);
    assert!(h.pending().is_empty());
    assert!(h.wakes().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_keeps_distinct_child_handoffs_pending_and_rewakes_the_recorded_child_on_restart() {
    let h = harness();
    let s = h.session().await;
    let action = code("return { done: runs === 1, send: { prompt: \"Check \" + runs, to: { subagent: \"helper\" } } }");
    let automation = h.create(json!({"sessionID": s, "name": "checks", "trigger": every("1 minute"), "action": action})).await.unwrap();
    h.minute().await;
    h.minute().await;
    let children: Vec<String> = [1, 2].iter().map(|run| format!("ses_{}_{run}", id(&automation))).collect();
    assert_eq!(h.wakes(), children);
    h.clear_wakes();
    let runner = tokio::spawn(h.automations.clone().run());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !h.wakes().contains(&children[1]) {
        assert!(std::time::Instant::now() < deadline, "no rewake");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    runner.abort();
    assert_eq!(h.pending_ids(), vec![msg(&automation, 1), msg(&automation, 2)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_does_not_use_an_inbox_id_to_override_changed_authority_when_retrying_an_admitted_child_prompt() {
    let h = harness();
    let s = h.session().await;
    let action = code("return { state: 1, done: true, send: { to: { subagent: \"helper\" }, prompt: \"triage\" } }");
    let automation = h.create(json!({"sessionID": s, "name": "triage", "trigger": every("1 minute"), "action": action})).await.unwrap();
    // The real admission succeeded, but the wake failed before the automation could record it.
    h.prompter.fail_next.store(true, Ordering::SeqCst);
    h.minute().await;
    let child = format!("ses_{}_1", id(&automation));
    let item = vec![(msg(&automation, 1), child.clone())];
    assert_eq!(h.pending(), item);
    subset(&h.get(id(&automation)), json!({"status": "active", "runCount": 0}));
    h.app.sessions.set_permissions(&s, vec![rule("edit", "*", "deny")]).unwrap();
    h.clear_wakes();
    h.adjust(5 * MINUTE);
    h.tick().await;
    assert!(h.wakes().is_empty());
    assert_eq!(h.pending(), item);
    let info = h.get(id(&automation));
    subset(&info, json!({"status": "paused", "runCount": 0, "state": null}));
    assert!(info["lastOutput"].as_str().unwrap().contains("no longer matches the creator's permissions"), "{info}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_reconciles_a_child_handoff_already_delivered_before_settlement() {
    let h = harness();
    let s = h.session().await;
    let action = code("return { done: true, send: { to: { subagent: \"helper\" }, prompt: \"Triage the failure\" } }");
    let automation = h.create(json!({"sessionID": s, "name": "triage", "trigger": every("1 minute"), "action": action})).await.unwrap();
    let child = format!("ses_{}_1", id(&automation));
    // The first admission was consumed but the scheduler never recorded the run.
    h.app
        .sessions
        .create(SessionInput { id: Some(child.clone()), parent_id: Some(s.clone()), agent: Some("helper".into()), ..Default::default() })
        .await
        .unwrap();
    h.app.sessions.prompt(&child, PromptInput { id: Some(msg(&automation, 1)), text: "Triage the failure".into(), ..Default::default() }).await.unwrap();
    h.deliver(&child).await;
    h.minute().await;
    assert!(h.pending().is_empty());
    assert_eq!(h.wakes(), vec![child.clone()]);
    assert_eq!(h.children(&s), vec![child]);
    subset(&h.get(id(&automation)), json!({"runCount": 1, "status": "done"}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_extracts_tools_at_creation_asks_once_without_save_and_runs_unattended_after() {
    let h = harness();
    let s = h.session_in("enabled", Some(vec![rule("*", "*", "ask")])).await;
    let (asked, task) = answer_asks(&h);
    let action = code_args(WATCH_THREAD, json!({"channel": "#ops"}));
    let automation = h.create_as(json!({"sessionID": s, "name": "thread", "trigger": every("1 minute"), "action": action}), &invocation(&s)).await.unwrap();
    task.abort();
    let requests = asked.lock().clone();
    assert_eq!(requests.iter().map(|r| r["action"].as_str().unwrap()).collect::<Vec<_>>(), ["slack_read_thread", "slack_post"]);
    for request in &requests {
        assert_eq!(request["sessionID"], s);
        assert_eq!(request["source"], json!({"type": "tool", "messageID": "msg_author", "id": "create_automation"}));
        assert!(request.get("save").is_none());
    }
    subset(
        &requests[0]["metadata"],
        json!({"automation": "thread", "trigger": {"type": "every", "every": "1 minute"}, "unattended": true, "tools": ["slack.read_thread", "slack.post"]}),
    );
    // Runs proceed without asking again.
    *h.slack.replies.lock() = 1;
    h.minute().await;
    *h.slack.replies.lock() = 2;
    h.minute().await;
    assert_eq!(asked.lock().len(), 2);
    assert_eq!(*h.slack.posts.lock(), vec!["#ops: 2".to_string()]);
    subset(&h.get(id(&automation)), json!({"runCount": 2, "lastOutcome": "fired"}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_asks_for_resource_scoped_ask_rules_at_creation_and_still_needs_a_person_for_them_at_run_time() {
    let h = harness();
    let rules = vec![rule("*", "*", "allow"), rule("slack_post", "vault:*", "ask"), rule("slack_post", "vault:public", "allow")];
    let s = h.session_in("enabled", Some(rules)).await;
    let (asked, task) = answer_asks(&h);
    let action = code_args("for (const channel of args) await tools.slack.post({ text: \"hi\", channel })", json!(["general", "vault:public"]));
    let automation = h.create(json!({"sessionID": s, "name": "post", "trigger": every("1 minute"), "action": action})).await.unwrap();
    task.abort();
    let requests: Vec<Value> = asked.lock().iter().map(|r| json!([r["action"], r["resources"]])).collect();
    assert_eq!(requests, vec![json!(["slack_post", ["*", "vault:*"]])]);
    // The approval covers everything the catch-all allows, and narrower allows still apply.
    h.minute().await;
    assert_eq!(*h.slack.posts.lock(), vec!["hi".to_string(), "hi".to_string()]);
    // A scoped ask is not settled by the unattended grant, so that call fails and the run is recorded as failed.
    let action = code("await tools.slack.post({ text: \"leak\", channel: \"vault:keys\" })");
    let secret = oc_core::permission::unattended(
        allow_all(),
        h.create(json!({"sessionID": s, "name": "secret", "trigger": every("1 minute"), "action": action})),
    )
    .await
    .unwrap();
    h.minute().await;
    assert!(!h.slack.posts.lock().contains(&"leak".to_string()));
    subset(&h.get(id(&secret)), json!({"runCount": 0, "lastOutcome": "failed"}));
    assert_eq!(h.get(id(&automation))["runCount"], 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_asks_for_execute_when_its_tools_are_already_allowed() {
    let h = harness();
    let s = h.session_in("enabled", Some(vec![rule("*", "*", "allow"), rule("execute", "*", "ask")])).await;
    let (asked, task) = answer_asks(&h);
    let input = json!({"sessionID": s, "name": "read", "trigger": every("1 minute"), "action": code("await tools.slack.read_thread({})")});
    h.create_as(input.clone(), &invocation(&s)).await.unwrap();
    task.abort();
    let requests = asked.lock().clone();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!((requests[0]["action"].as_str(), requests[0]["sessionID"].as_str()), (Some("execute"), Some(s.as_str())));
    // Nobody present to approve: refused rather than created.
    let refused = oc_core::permission::unattended(Vec::new(), h.create(input)).await;
    assert!(matches!(refused, Err(AutomationError::Blocked(_))), "{refused:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_rejects_dynamic_tool_access_unknown_tools_recursive_automation_tools_and_denied_permissions() {
    let h = harness();
    let s = h.session().await;
    let bad = |owner: &str, text: &str| h.create(json!({"sessionID": owner, "name": "bad", "trigger": every("1 minute"), "action": code(text)}));
    assert!(invalid(bad(&s, "const x = {").await).starts_with("SyntaxError: "));
    assert!(invalid(bad(&s, "return await tools.slack[name]({})").await).starts_with("Automation code must reference tools statically"));
    assert!(invalid(bad(&s, "const t = require(\"tools\"); await t.slack.post({})").await).starts_with("Automation code must call tools directly"));
    assert_eq!(invalid(bad(&s, "return await tools.slack.missing({})").await), "Unknown Code Mode tool: slack.missing");
    // 2.4.1 (#65217): the automation tools and session moves stay off-limits; shell is not on the list any more.
    assert_eq!(invalid(bad(&s, "return await tools.automation.create({})").await), "Tool automation.create cannot be used inside an automation");
    assert_eq!(invalid(bad(&s, "return await tools.opencode.session.move({})").await), "Tool opencode.session.move cannot be used inside an automation");
    let denied = h.session_in("enabled", Some(vec![rule("*", "*", "allow"), rule("slack_post", "*", "deny")])).await;
    assert!(matches!(bad(&denied, "await tools.slack.post({ text: 'x' })").await, Err(AutomationError::Blocked(_))));
    assert!(h.automations.list(&ListInput::default()).await.unwrap().is_empty());
}

// ---- the tools (`tool-automation.test.ts`) ----

fn tool_context(h: &H, session: &str, call: &str) -> ToolContext {
    let dir = h.root.join("enabled");
    ToolContext {
        codemode: false,
        session_id: session.into(),
        message_id: "msg_tool".into(),
        call_id: call.into(),
        agent: "build".into(),
        directory: dir.clone(),
        worktree: dir,
        project_id: String::new(),
        workspace_id: None,
        global: h.app.global.clone(),
        cancel: tokio_util::sync::CancellationToken::new(),
        stop: Default::default(),
        progress: Arc::new(|_| {}),
        config: Arc::new(oc_tools::tool::ToolConfig::default()),
        host: Arc::new(oc_core::host::CoreHost { app: h.app.clone(), skills: Arc::new(Vec::new()), mcp: None, location: None, engine: None, blocking_parent: None }),
    }
}

impl H {
    fn tool(&self, name: &str) -> Arc<dyn Tool> {
        // Registry IDs (`automation_create`, ...): the tools are `create`, ... in the `automation` namespace.
        oc_core::automation::tools::tools(&self.automations)
            .into_iter()
            .find(|t| t.definition(&Default::default()).name == name)
            .unwrap()
    }

    async fn call(&self, session: &str, name: &str, input: Value) -> Result<ToolResult, ToolError> {
        self.tool(name).execute(input, tool_context(self, session, &format!("call-{name}"))).await
    }

    async fn content(&self, session: &str, name: &str, input: Value) -> String {
        let result = self.call(session, name, input).await.unwrap();
        result.content.iter().map(|c| if let oc_schema::ToolContent::Text { text } = c { text.as_str() } else { "" }).collect()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tools_are_absent_unless_automations_are_enabled() {
    let h = harness();
    let names = ["automation_create", "automation_list", "automation_pause", "automation_resume", "automation_delete"];
    // 2.4.1: available by default and absent when `automations` is false.
    write_config(&h.root.join("plain"), json!({}));
    for (dir, present) in [("enabled", true), ("disabled", false), ("plain", true)] {
        let s = h.session_in(dir, None).await;
        let session = h.app.sessions.get(&s).unwrap();
        let env = h.engine.check_env(&session, &h.dir(dir)).await.unwrap();
        let listed: Vec<String> = env.registry.list().into_iter().map(|(id, _)| id).collect();
        for name in names {
            assert_eq!(listed.contains(&name.to_string()), present, "{dir}: {name} in {listed:?}");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tools_create_list_pause_resume_and_delete_this_projects_automations() {
    let h = harness();
    h.set_time("2026-10-27T13:00:00Z");
    let s = h.session().await;
    let created = h.call(&s, "automation_create", json!({"name": "build", "trigger": at("in 20 minutes"), "action": prompt("check the build")})).await.unwrap();
    let view = created.output.unwrap();
    subset(
        &view,
        json!({
            "name": "build", "trigger": {"type": "at", "at": "2026-10-27T13:20:00.000Z"},
            "action": {"type": "prompt", "prompt": "check the build", "to": {"session": s}},
            "status": "active", "nextDueAt": "2026-10-27T09:20:00.000-04:00",
        }),
    );
    let aut = view["id"].as_str().unwrap().to_string();
    assert_eq!(h.get(&aut)["sessionID"], s);
    let list = h.call(&s, "automation_list", json!({})).await.unwrap().output.unwrap();
    assert_eq!(list["now"], "2026-10-27T13:00:00.000Z");
    subset(&list["automations"][0], json!({"id": aut, "status": "active"}));
    subset(&h.call(&s, "automation_pause", json!({"id": aut})).await.unwrap().output.unwrap(), json!({"status": "paused"}));
    subset(&h.call(&s, "automation_resume", json!({"id": aut})).await.unwrap().output.unwrap(), json!({"status": "active"}));
    let deleted = h.call(&s, "automation_delete", json!({"id": aut})).await.unwrap();
    assert_eq!(deleted.output, Some(json!({"id": aut, "removed": true})));
    assert!(h.automations.list(&ListInput::default()).await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tools_recheck_the_flag_on_each_call() {
    let h = harness();
    let s = h.session().await;
    let view = h.call(&s, "automation_create", json!({"name": "ping", "trigger": every("1 hour"), "action": prompt("ping")})).await.unwrap().output.unwrap();
    let aut = view["id"].as_str().unwrap().to_string();
    write_config(&h.root.join("enabled"), config(false, json!({})));
    h.reload("enabled").await;
    for (name, input) in [
        ("automation_list", json!({})),
        ("automation_pause", json!({"id": aut})),
        ("automation_resume", json!({"id": aut})),
        ("automation_delete", json!({"id": aut})),
        ("automation_create", json!({"name": "ping", "trigger": every("1 hour"), "action": prompt("ping")})),
    ] {
        let error = h.call(&s, name, input).await.unwrap_err();
        assert!(error.model_message().contains("Automations are disabled"), "{name}: {}", error.model_message());
    }
    assert_eq!(h.get(&aut)["status"], "active");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tools_reject_a_malformed_trigger_target_or_empty_name() {
    let h = harness();
    let tool = h.tool("automation_create");
    for input in [
        json!({"name": "x", "trigger": {"every": "1 hour"}, "action": prompt("ping")}),
        json!({"name": "x", "trigger": every("1 hour"), "action": {"type": "prompt", "prompt": "ping", "to": {"session": "ses_other", "subagent": "general"}}}),
        json!({"name": "", "trigger": every("1 hour"), "action": prompt("ping")}),
    ] {
        assert!(!tool.validate(&input).is_empty(), "{input}");
    }
    assert!(tool.validate(&json!({"name": "x", "trigger": every("1 hour"), "action": prompt("ping")})).is_empty());
    // Effect decodes `trigger` and `action` as tagged unions: by `type`, only that member's issues (captured from 2.3.1).
    let messages = |input: Value| {
        tool.validate(&input)
            .into_iter()
            .map(|i| {
                let path: Vec<String> = i
                    .path
                    .iter()
                    .map(|p| match p {
                        oc_tools::validate::PathSegment::Key(k) => k.clone(),
                        oc_tools::validate::PathSegment::Index(n) => n.to_string(),
                    })
                    .collect();
                format!("{}: {}", path.join("."), i.message)
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(messages(json!({"name": "x", "trigger": {"every": "1 hour"}, "action": prompt("two")})), ["trigger: Expected Automation.Trigger"]);
    assert_eq!(
        messages(json!({"name": "x", "trigger": every("1 hour"), "action": {"type": "prompt", "prompt": "x", "to": {"session": "ses_1", "subagent": "general"}}})),
        ["action.to.subagent: Expected never", "action.to.session: Expected never"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tools_put_each_automations_id_trigger_status_next_run_and_action_in_the_text() {
    let h = harness();
    h.set_time("2026-10-27T13:00:00Z");
    let s = h.session().await;
    let ci = h.content(&s, "automation_create", json!({"name": "ci", "trigger": every("1 hour"), "action": prompt("check CI")})).await;
    let first = h.automations.list(&ListInput::default()).await.unwrap()[0]["id"].as_str().unwrap().to_string();
    let ci_line = format!("{first} \"ci\": every 1 hour (America/New_York), active, next 2026-10-27T10:00:00.000-04:00, prompt {s}: \"check CI\"");
    assert_eq!(ci, format!("Created: {ci_line}"));
    let watch = json!({"name": "watch", "trigger": {"type": "cron", "cron": "0 9 * * 1-5"}, "action": code("\n  const t = await tools.slack.read_thread({})\n  return { state: t }")});
    h.content(&s, "automation_create", watch).await;
    let second = h.automations.list(&ListInput::default()).await.unwrap()[1]["id"].as_str().unwrap().to_string();
    h.content(&s, "automation_pause", json!({"id": second})).await;
    let list = h.content(&s, "automation_list", json!({})).await;
    let lines: Vec<&str> = list.split('\n').collect();
    assert_eq!(lines[0], "Now: 2026-10-27T13:00:00.000Z");
    assert_eq!(
        lines[1..],
        [
            ci_line,
            format!(
                "{second} \"watch\": cron \"0 9 * * 1-5\" (America/New_York), paused, next 2026-10-28T09:00:00.000-04:00, code: \"const t = await tools.slack.read_thread({{}})\""
            ),
        ]
    );
    h.content(&s, "automation_delete", json!({"id": first})).await;
    h.content(&s, "automation_delete", json!({"id": second})).await;
    assert_eq!(h.content(&s, "automation_list", json!({})).await, "Now: 2026-10-27T13:00:00.000Z\nNo automations.");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tools_treat_another_projects_automation_as_missing() {
    let h = harness();
    let s = h.session().await;
    let other = h.session().await;
    let view = h.call(&other, "automation_create", json!({"name": "ping", "trigger": every("1 hour"), "action": prompt("ping")})).await.unwrap().output.unwrap();
    let aut = view["id"].as_str().unwrap().to_string();
    h.app.db.with(|c| c.execute("UPDATE automation SET project_id = 'prj_two' WHERE id = ?", [&aut])).unwrap();
    assert_eq!(h.call(&s, "automation_list", json!({})).await.unwrap().output.unwrap()["automations"], json!([]));
    for name in ["automation_pause", "automation_resume", "automation_delete"] {
        let error = h.call(&s, name, json!({"id": aut})).await.unwrap_err();
        assert!(error.model_message().contains(&format!("Automation not found: {aut}")), "{}", error.model_message());
    }
    subset(&h.get(&aut), json!({"status": "active", "sessionID": other}));
}

/// 2.4.1 (#65296): automations follow the top-level `automations` of the highest-priority document that sets it, and
/// are on when none does; `experimental.automations` and `experimental.schedules` are removed and no longer count, and
/// `OPENCODE_FLAGS` no longer names them.
#[test]
fn the_top_level_automations_setting_turns_automations_on() {
    let doc = |text: &str| oc_config::Entry::Document {
        path: None,
        info: oc_config::load::parse_info(text, "test").unwrap(),
        integration: None,
        wellknown: None,
    };
    let enabled = |entries: &[oc_config::Entry]| oc_core::automation::settings_of(entries).enabled;
    assert!(oc_core::automation::DEFAULT_ENABLED);
    assert!(enabled(&[]));
    assert!(enabled(&[doc(r#"{"automations": true}"#)]));
    assert!(!enabled(&[doc(r#"{"automations": true}"#), doc(r#"{"automations": false}"#)]));
    // A project document that does not set it leaves the global one in effect.
    assert!(!enabled(&[doc(r#"{"automations": false}"#), doc(r#"{"timezone": "UTC", "experimental": {}}"#)]));
    assert!(enabled(&[doc(r#"{"experimental": {"automations": false, "schedules": false}}"#)]));
    assert!(enabled(&[doc(r#"{"automations": true, "experimental": {"automations": false}}"#)]));
}

// ---- 2.4.1: automations are on by default, the shell action and the visibility checks -------------------------------

/// "hides and refuses automations while their location has automations off" (`automation.test.ts`, #65747): list,
/// pause and delete answer `Disabled` there, a directory filter must be absolute, and the rows come back once
/// automations are on again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hides_and_refuses_automations_while_their_location_has_automations_off() {
    let h = harness();
    write_config(&h.root.join("toggle"), config(true, json!({})));
    let s = h.session_in("toggle", None).await;
    let automation = h.create(json!({"sessionID": s, "name": "poll", "trigger": every("1 minute"), "action": prompt("poll")})).await.unwrap();
    let aut = id(&automation).to_string();
    let toggle = h.dir("toggle");
    let ids = |list: Vec<Value>| list.iter().map(|i| i["id"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    let by_directory = |directory: &str| ListInput { directory: Some(directory.to_string()), ..Default::default() };
    assert_eq!(ids(h.automations.list(&by_directory(&toggle)).await.unwrap()), vec![aut.clone()]);
    assert_eq!(h.automations.list(&by_directory("relative")).await, Err(AutomationError::Invalid("Directory must be an absolute path: relative".into())));

    write_config(&h.root.join("toggle"), config(false, json!({})));
    h.reload("toggle").await;
    assert_eq!(h.automations.list(&ListInput::default()).await.unwrap(), Vec::<Value>::new());
    assert_eq!(h.automations.list(&by_directory(&toggle)).await, Err(AutomationError::Disabled));
    assert_eq!(h.automations.list(&ListInput { session_id: Some(s.clone()), ..Default::default() }).await, Err(AutomationError::Disabled));
    assert_eq!(h.automations.get(&aut).await, Err(AutomationError::Disabled));
    assert_eq!(h.automations.pause(&aut).await, Err(AutomationError::Disabled));
    assert_eq!(h.automations.remove(&aut).await, Err(AutomationError::Disabled));

    write_config(&h.root.join("toggle"), config(true, json!({})));
    h.reload("toggle").await;
    h.automations.remove(&aut).await.unwrap();
    assert_eq!(h.automations.list(&ListInput::default()).await.unwrap(), Vec::<Value>::new());
}

/// An unset `automations` means on (2.4.1, #65296): a location whose config never names it creates and lists them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn automations_are_on_where_config_does_not_set_them() {
    let h = harness();
    write_config(&h.root.join("plain"), json!({"timezone": "America/New_York"}));
    let s = h.session_in("plain", None).await;
    let automation = h.create(json!({"sessionID": s, "name": "poll", "trigger": every("1 hour"), "action": prompt("poll")})).await.unwrap();
    let listed = h.automations.list(&ListInput { directory: Some(h.dir("plain")), ..Default::default() }).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["id"], automation["id"]);
}

/// `{ type: "shell" }` needs a creating session, like a code action.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shell_action_needs_a_session() {
    let h = harness();
    let input = json!({"name": "x", "directory": h.dir("enabled"), "trigger": every("1 hour"), "action": {"type": "shell", "command": "echo x"}});
    assert_eq!(invalid(h.create(input).await), "A shell action needs a session");
}

/// Waits for the row's `lastOutput` (a shell action records its result when the command exits).
async fn last_output(h: &H, aut: &str) -> Value {
    for _ in 0..2000 {
        let info = h.get(aut);
        if info.get("lastOutput").is_some() {
            return info;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("no lastOutput for {aut}: {}", h.get(aut));
}

/// Makes the automation due now, so a tick runs it.
fn due_now(h: &H, aut: &str) {
    h.app.db.with(|c| c.execute("UPDATE automation SET next_due_at = ? WHERE id = ?", rusqlite::params![h.now(), aut])).unwrap();
}

/// `automation-shell.test.ts` "approves the fixed command once and records a failed exit without waking a session".
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell_approves_the_fixed_command_once_and_records_a_failed_exit_without_waking_a_session() {
    let h = harness();
    let s = h.session_in("enabled", Some(vec![rule("*", "*", "allow"), rule("shell", "*", "ask")])).await;
    let command = "echo automation-output; exit 7";
    let trigger = every("1 hour");
    let (asked, task) = answer_asks(&h);
    let created = h
        .create_as(json!({"sessionID": s, "name": "shell test", "trigger": trigger, "action": {"type": "shell", "command": command}}), &invocation(&s))
        .await
        .unwrap();
    task.abort();
    let asks = asked.lock().clone();
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert_eq!(asks[0]["action"], "shell");
    assert!(asks[0]["resources"].as_array().unwrap().contains(&json!("echo automation-output")), "{}", asks[0]);
    assert_eq!(asks[0]["metadata"], json!({"automation": "shell test", "trigger": trigger, "unattended": true, "command": command}));
    assert!(asks[0].get("save").is_none_or(Value::is_null), "{}", asks[0]);
    subset(&created, json!({"action": {"type": "shell", "command": command}}));

    let aut = id(&created).to_string();
    due_now(&h, &aut);
    h.tick().await;
    let ran = last_output(&h, &aut).await;
    subset(&ran, json!({"status": "active", "runCount": 1, "lastOutcome": "failed"}));
    let output = ran["lastOutput"].as_str().unwrap();
    assert!(output.starts_with("Exit code: 7\n") && output.contains("automation-output"), "{output}");
    assert!(h.pending().is_empty());
    assert!(h.wakes().is_empty());
}

/// "uses the saved location after the creating session moves and pauses if that directory disappears".
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell_uses_the_saved_location_and_pauses_if_that_directory_disappears() {
    let h = harness();
    write_config(&h.root.join("saved"), config(true, json!({})));
    std::fs::create_dir_all(h.root.join("destination")).unwrap();
    let s = h.session_in("saved", None).await;
    let created = h
        .create(json!({"sessionID": s, "name": "saved location", "trigger": every("1 hour"), "action": {"type": "shell", "command": "echo automation-output"}}))
        .await
        .unwrap();
    let aut = id(&created).to_string();
    // The creating session moves; the automation keeps its directory.
    h.app.db.with(|c| c.execute("UPDATE session_v2 SET directory = ? WHERE id = ?", rusqlite::params![h.dir("destination"), s])).unwrap();
    due_now(&h, &aut);
    h.tick().await;
    let ran = last_output(&h, &aut).await;
    subset(&ran, json!({"lastOutcome": "fired", "directory": h.dir("saved")}));
    assert!(ran["lastOutput"].as_str().unwrap().contains("automation-output"));

    std::fs::remove_dir_all(h.root.join("saved")).unwrap();
    due_now(&h, &aut);
    h.tick().await;
    subset(&h.get(&aut), json!({"status": "paused", "runCount": 1, "lastOutput": format!("Directory no longer exists: {}", h.dir("saved"))}));
}

/// "rejects a shell action that changes to a denied external directory".
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell_rejects_a_command_that_changes_to_a_denied_external_directory() {
    let h = harness();
    let s = h.session_in("enabled", Some(vec![rule("*", "*", "allow"), rule("external_directory", "*", "deny")])).await;
    let result = h
        .create_as(json!({"sessionID": s, "name": "outside", "trigger": every("1 hour"), "action": {"type": "shell", "command": "cd ..; echo outside"}}), &invocation(&s))
        .await;
    match result {
        Err(AutomationError::Blocked(message)) => assert!(message.contains("external_directory"), "{message}"),
        other => panic!("expected a blocked external_directory, got {other:?}"),
    }
    assert!(h.automations.list(&ListInput::default()).await.unwrap().is_empty());
}

/// Scheduled code gets `fetch` (#65236): referencing it asks for `webfetch` once at creation, without saving, and a
/// run fetches unattended. ocd has no Bun globals (`full_codemode`), so only the fetch half of "scheduled code uses
/// session capabilities after one-time approval" applies.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scheduled_code_fetches_after_one_time_approval() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else { return };
            let mut buffer = [0u8; 4096];
            let _ = socket.read(&mut buffer).await;
            let _ = socket.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\nconnection: close\r\n\r\nready").await;
        }
    });
    let h = harness();
    let s = h.session_in("enabled", Some(vec![rule("*", "*", "allow"), rule("webfetch", "*", "ask")])).await;
    let (asked, task) = answer_asks(&h);
    let created = h
        .create_as(
            json!({"sessionID": s, "name": "check", "trigger": every("1 hour"),
                "action": code_args("const text = await (await fetch(args)).text(); return { state: { text }, done: true }", json!(format!("http://{address}/")))}),
            &invocation(&s),
        )
        .await
        .unwrap();
    task.abort();
    let asks = asked.lock().clone();
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert_eq!(asks[0]["action"], "webfetch");
    assert_eq!(asks[0]["metadata"]["unattended"], true);
    assert_eq!(asks[0]["metadata"]["tools"], json!([]));
    let aut = id(&created).to_string();
    due_now(&h, &aut);
    h.tick().await;
    subset(&h.get(&aut), json!({"status": "done", "lastOutcome": "fired", "state": {"text": "ready"}}));
    server.abort();
}

/// The tools are `create`, `list`, `pause`, `resume` and `delete` in their own `automation` namespace (2.4.1, #65001),
/// so programs call `tools.automation.create(...)`; their registry IDs stay `automation_<name>`.
#[test]
fn the_tools_live_in_the_automation_namespace() {
    let h = harness();
    let tools = oc_core::automation::tools::tools(&h.automations);
    let names: Vec<(String, Option<String>, String)> = tools
        .iter()
        .map(|t| (t.name().to_string(), t.options().namespace, t.definition(&Default::default()).name))
        .collect();
    assert_eq!(
        names,
        ["create", "list", "pause", "resume", "delete"]
            .map(|n| (n.to_string(), Some("automation".to_string()), format!("automation_{n}")))
            .to_vec()
    );
    let namespace = oc_core::automation::tools::namespace();
    assert_eq!(namespace.name, "automation");
    assert!(namespace.description.starts_with("Schedule recurring or one-off jobs (cron jobs, timers, reminders)."));
    assert_eq!(
        tools[0].definition(&Default::default()).description,
        "Create an automation that runs a prompt, a Code Mode script, or a shell command once at a set time, on an interval, on a cron, or when an MCP server sends an event."
    );
    assert_eq!(tools[2].definition(&Default::default()).input_schema["properties"]["id"]["description"], "Automation id from automation.list.");
    // The view puts the small fields before the large action and output.
    let view = oc_core::automation::tools::view(&json!({
        "id": "aut_1", "name": "ci", "projectID": "prj", "directory": "/p", "trigger": every("1 hour"), "tz": "UTC",
        "action": {"type": "shell", "command": "make test"}, "status": "active", "runCount": 0, "lastOutput": "ok",
        "time": {"created": 0, "updated": 0},
    }));
    let keys: Vec<&String> = view.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["id", "name", "trigger", "tz", "status", "runCount", "action", "lastOutput"]);
    assert_eq!(oc_core::automation::tools::summary(&view), "aut_1 \"ci\": every 1 hour (UTC), active, shell: \"make test\", output: \"ok\"");
}
