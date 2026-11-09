//! The schedule HTTP API with `experimental.schedules` on (`server/src/handlers/schedule.ts`, 2.2.5), `/schedule`,
//! and a scheduled wake admitted through the session API. Bodies were checked against the 2.2.5 binary (parity
//! group `schedule`).

mod common;

use std::sync::Arc;

use common::*;
use serde_json::{Value, json};

struct Api {
    app: Arc<oc_core::app::App>,
    engine: Arc<oc_backend::Engine>,
    router: axum::Router,
    cwd: std::path::PathBuf,
    _root: tempfile::TempDir,
}

fn api() -> Api {
    unsafe { std::env::set_var("OPENCODE_DISABLE_MODELS_FETCH", "1") };
    let root = tempfile::tempdir().unwrap();
    let root_path = std::fs::canonicalize(root.path()).unwrap();
    let cwd = root_path.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(
        cwd.join("opencode.json"),
        json!({ "timezone": "America/New_York", "experimental": { "schedules": true }, "permission": { "*": "allow" } }).to_string(),
    )
    .unwrap();
    let off = root_path.join("off");
    std::fs::create_dir_all(&off).unwrap();
    let app = oc_core::app::App::new(global(&root_path), oc_storage::Database::memory().unwrap(), "1.2.3");
    let runner = oc_core::runner::Engine::new(app.clone(), &oc_core::services::default_services(&app));
    let engine = oc_backend::Engine::with_runner(runner);
    oc_backend::schedules::connect(&engine);
    let opts = oc_server::ServerOptions { cwd: Some(cwd.display().to_string()), version: "1.2.3".into(), ready: true, ..Default::default() };
    let (router, _) = oc_server::router(engine.clone() as Arc<dyn oc_server::Backend>, &opts).unwrap();
    Api { app, engine, router, cwd, _root: root }
}

impl Api {
    async fn req(&self, method: &str, uri: &str, body: Option<Value>) -> (u16, Value) {
        use tower::ServiceExt;
        let mut b = axum::http::Request::builder().method(method).uri(uri).header("x-opencode-directory", self.cwd.display().to_string());
        let body = match body {
            Some(v) => {
                b = b.header("content-type", "application/json");
                axum::body::Body::from(v.to_string())
            }
            None => axum::body::Body::empty(),
        };
        let res = self.router.clone().oneshot(b.body(body).unwrap()).await.unwrap();
        let status = res.status().as_u16();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    async fn session_at(&self, directory: &str) -> String {
        let (status, body) = self.req("POST", "/api/session", Some(json!({ "location": { "directory": directory } }))).await;
        assert_eq!(status, 200, "{body}");
        body["data"]["id"].as_str().unwrap().to_string()
    }

    async fn create(&self, session: &str, body: Value) -> (u16, Value) {
        let mut payload = body;
        payload["sessionID"] = json!(session);
        self.req("POST", "/api/experimental/schedule", Some(payload)).await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn schedule_routes_with_schedules_on() {
    let a = api();
    let sid = a.session_at(&a.cwd.display().to_string()).await;
    let (status, created) = a.create(&sid, json!({ "name": "build", "when": { "in": "20 minutes" }, "prompt": "check the build" })).await;
    assert_eq!(status, 200, "{created}");
    let info = created["data"].clone();
    let id = info["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("sch_"));
    let keys: Vec<&str> = info.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, ["id", "sessionID", "name", "when", "tz", "action", "status", "nextDueAt", "runCount", "time"]);
    assert_eq!(info["when"], json!({ "type": "in", "in": "20 minutes" }));
    assert_eq!((info["tz"].as_str(), info["status"].as_str()), (Some("America/New_York"), Some("active")));
    assert_eq!(info["action"], json!({ "type": "prompt", "prompt": "check the build" }));

    let (status, body) = a.create(&sid, json!({ "when": { "in": "0 seconds" }, "prompt": "x" })).await;
    assert_eq!(
        (status, body),
        (400, json!({ "_tag": "InvalidRequestError", "message": "Duration must be positive: 0 seconds", "kind": "schedule" }))
    );
    let (status, body) = a.create("ses_000000000000aaaaaaaaaaaaaa", json!({ "when": { "in": "1 minute" }, "prompt": "x" })).await;
    assert_eq!((status, body["_tag"].as_str()), (404, Some("SessionNotFoundError")));

    let (status, list) = a.req("GET", &format!("/api/experimental/schedule?sessionID={sid}"), None).await;
    assert_eq!((status, list["data"].as_array().map(Vec::len)), (200, Some(1)));
    let (status, paused) = a.req("POST", &format!("/api/experimental/schedule/{id}/pause"), None).await;
    assert_eq!((status, paused["data"]["status"].as_str()), (200, Some("paused")));
    let (status, resumed) = a.req("POST", &format!("/api/experimental/schedule/{id}/resume"), None).await;
    assert_eq!((status, resumed["data"]["status"].as_str()), (200, Some("active")));
    let (status, got) = a.req("GET", &format!("/api/experimental/schedule/{id}"), None).await;
    assert_eq!((status, &got["data"]["id"]), (200, &json!(id)));
    assert_eq!(a.req("DELETE", &format!("/api/experimental/schedule/{id}"), None).await.0, 204);
    let (status, body) = a.req("GET", &format!("/api/experimental/schedule/{id}"), None).await;
    assert_eq!((status, body), (404, json!({ "_tag": "ScheduleNotFoundError", "scheduleID": id, "message": format!("Schedule not found: {id}") })));

    // A session whose location has the flag off: 403, and its schedules read as forbidden.
    let off = a.session_at(&a.cwd.parent().unwrap().join("off").display().to_string()).await;
    let forbidden = json!({ "_tag": "ForbiddenError", "message": "Scheduled tasks are disabled; set experimental.schedules in config" });
    assert_eq!(a.create(&off, json!({ "when": { "in": "1 minute" }, "prompt": "x" })).await, (403, forbidden.clone()));
    assert_eq!(a.req("GET", &format!("/api/experimental/schedule?sessionID={off}"), None).await, (403, forbidden));

    // `/schedule` is listed and hands the request to the model with the template.
    let (_, commands) = a.req("GET", "/api/command", None).await;
    let names: Vec<&str> = commands["data"].as_array().unwrap().iter().filter_map(|c| c["name"].as_str()).collect();
    assert!(names.contains(&"schedule"), "{names:?}");
    let (status, body) =
        a.req("POST", &format!("/api/session/{sid}/command"), Some(json!({ "name": "schedule", "text": "  every morning at 9 check CI  " }))).await;
    assert_eq!(status, 204, "{body}");
    let inbox = a.app.db.session_inbox(&sid).unwrap();
    let text = inbox.last().unwrap().2["text"].as_str().unwrap().to_string();
    assert!(text.starts_with("Manage scheduled prompts for this session.\n\nRequest: every morning at 9 check CI\n"), "{text}");
    assert!(!text.contains("$ARGUMENTS"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_due_schedule_wakes_its_session_through_the_session_api() {
    let a = api();
    let sid = a.session_at(&a.cwd.display().to_string()).await;
    let (_, created) = a.create(&sid, json!({ "name": "soon", "when": { "in": "1 second" }, "prompt": "check the build" })).await;
    let id = created["data"]["id"].as_str().unwrap().to_string();
    let due = created["data"]["nextDueAt"].as_i64().unwrap();
    a.engine.schedules.tick_at(due).await;
    let inbox = a.app.db.session_inbox(&sid).unwrap();
    let (message, _, payload, delivery, _, _) = inbox.last().unwrap().clone();
    assert_eq!(message, format!("msg_{id}_1"));
    assert_eq!(delivery, "steer");
    assert_eq!(payload["text"], "check the build");
    assert_eq!(payload["metadata"], json!({ "schedule": { "id": id, "name": "soon", "run": 1, "dueAt": due, "late": false } }));
    let (_, got) = a.req("GET", &format!("/api/experimental/schedule/{id}"), None).await;
    assert_eq!((got["data"]["status"].as_str(), got["data"]["runCount"].as_i64()), (Some("done"), Some(1)));
}

/// Without schedules on anywhere, the routes still answer as before.
#[tokio::test]
async fn schedule_routes_without_a_runner_answer_as_disabled() {
    let h = harness();
    let sid = h.session().await;
    let r = h.post("/api/experimental/schedule", json!({ "sessionID": sid, "when": { "in": "1 minute" }, "prompt": "x" })).await;
    assert_eq!((r.status.as_u16(), r.json()["_tag"].clone()), (403, json!("ForbiddenError")));
}
