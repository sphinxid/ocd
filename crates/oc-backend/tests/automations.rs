//! The automation HTTP API with automations enabled (top-level `automations: true`) (`server/src/handlers/automation.ts`, upstream's
//! `server/test/automation.test.ts`), `/automation`, and automations running through the session API. Bodies were
//! checked against the 2.3.1 binary (parity group `automation`).

mod common;

use std::sync::Arc;

use common::*;
use serde_json::{Value, json};

struct Api {
    engine: Arc<oc_backend::Engine>,
    router: axum::Router,
    cwd: std::path::PathBuf,
    root: std::path::PathBuf,
    _tmp: tempfile::TempDir,
}

fn write(dir: &std::path::Path, config: Value) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("opencode.json"), config.to_string()).unwrap();
}

fn api() -> Api {
    unsafe { std::env::set_var("OPENCODE_DISABLE_MODELS_FETCH", "1") };
    let tmp = tempfile::tempdir().unwrap();
    let root = oc_util::fs::canonicalize(tmp.path()).unwrap();
    let cwd = root.join("enabled");
    write(&cwd, json!({ "automations": true, "experimental": { "mcp_events": true }, "permission": { "*": "allow" } }));
    write(&root.join("ask"), json!({ "automations": true, "permission": { "*": "allow", "execute": "ask" } }));
    let app = oc_core::app::App::new(global(&root.join("g")), oc_storage::Database::memory().unwrap(), "1.2.3");
    let runner = oc_core::runner::Engine::new(app.clone(), &oc_core::services::default_services(&app));
    let engine = oc_backend::Engine::with_runner(runner);
    oc_backend::automations::connect(&engine);
    let opts = oc_server::ServerOptions { cwd: Some(cwd.display().to_string()), version: "1.2.3".into(), ready: true, ..Default::default() };
    let (router, _) = oc_server::router(engine.clone() as Arc<dyn oc_server::Backend>, &opts).unwrap();
    Api { engine, router, cwd, root, _tmp: tmp }
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

    async fn session_at(&self, directory: &std::path::Path) -> String {
        let (status, body) = self.req("POST", "/api/session", Some(json!({ "location": { "directory": directory } }))).await;
        assert_eq!(status, 200, "{body}");
        body["data"]["id"].as_str().unwrap().to_string()
    }

    async fn create(&self, body: Value) -> (u16, Value) {
        self.req("POST", "/api/experimental/automation", Some(body)).await
    }

    async fn list(&self, query: &str) -> (u16, Value) {
        self.req("GET", &format!("/api/experimental/automation{query}"), None).await
    }
}

fn hourly() -> Value {
    json!({ "type": "every", "every": "1 hour" })
}

fn tag(body: &Value) -> &str {
    body["_tag"].as_str().unwrap_or_default()
}

fn ids(body: &Value) -> Vec<String> {
    body["data"].as_array().unwrap().iter().map(|i| i["id"].as_str().unwrap().to_string()).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manages_automations_when_enabled_and_honors_automations_false() {
    let a = api();
    let enabled = a.cwd.clone();
    let session = a.session_at(&enabled).await;
    // An event watch needs a tool call; a session ID over HTTP is not a watch authorization.
    let (status, body) = a
        .create(json!({ "sessionID": session, "name": "build watch", "trigger": { "type": "event", "server": "ci", "event": "build.changed" }, "action": { "type": "prompt", "prompt": "Check the build" } }))
        .await;
    assert_eq!((status, tag(&body)), (400, "InvalidRequestError"));
    assert!(body["message"].as_str().unwrap().contains("tool call"), "{body}");

    let (status, created) = a
        .create(json!({ "sessionID": session, "name": "build", "trigger": hourly(), "expires": "in 8 hours", "action": { "type": "prompt", "prompt": "check the build" } }))
        .await;
    assert_eq!(status, 200, "{created}");
    let info = created["data"].clone();
    let id = info["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("aut_"));
    assert_eq!(info["sessionID"], session);
    assert_eq!(info["directory"], enabled.display().to_string());
    assert_eq!(info["trigger"], hourly());
    assert_eq!(info["action"], json!({ "type": "prompt", "prompt": "check the build", "to": { "session": session } }));
    assert_eq!(info["status"], "active");
    assert!(info["expires"].is_number());
    let keys: Vec<&str> = info.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, ["id", "name", "projectID", "sessionID", "directory", "trigger", "tz", "action", "expires", "status", "nextDueAt", "runCount", "time"]);
    assert_eq!(ids(&a.list(&format!("?sessionID={session}")).await.1), vec![id.clone()]);
    assert_eq!(ids(&a.list(&format!("?directory={}", enc(&enabled.display().to_string()))).await.1), vec![id.clone()]);
    assert_eq!(ids(&a.list("").await.1), vec![id.clone()]);
    assert_eq!(tag(&a.list("?directory=relative").await.1), "InvalidRequestError");
    assert_eq!(a.req("GET", &format!("/api/experimental/automation/{id}"), None).await.1["data"]["id"], id);
    assert_eq!(a.req("POST", &format!("/api/experimental/automation/{id}/pause"), None).await.1["data"]["status"], "paused");
    assert_eq!(a.req("POST", &format!("/api/experimental/automation/{id}/resume"), None).await.1["data"]["status"], "active");
    assert_eq!(a.req("DELETE", &format!("/api/experimental/automation/{id}"), None).await.0, 204);
    let (status, body) = a.req("GET", &format!("/api/experimental/automation/{id}"), None).await;
    assert_eq!((status, tag(&body)), (404, "AutomationNotFoundError"));

    // Without a session, in a directory, prompting an existing session.
    let (status, standalone) = a
        .create(json!({ "name": "standalone", "directory": enabled, "trigger": hourly(), "action": { "type": "prompt", "prompt": "hello", "to": { "session": session } } }))
        .await;
    assert_eq!(status, 200, "{standalone}");
    let standalone = standalone["data"].clone();
    assert_eq!(standalone["action"]["to"], json!({ "session": session }));
    assert!(standalone.get("sessionID").is_none());
    // A subagent target needs a creating session to be the parent of each run's child.
    let (status, body) =
        a.create(json!({ "name": "orphan", "directory": enabled, "trigger": hourly(), "action": { "type": "prompt", "prompt": "hello", "to": { "subagent": "general" } } })).await;
    assert_eq!((status, tag(&body)), (400, "InvalidRequestError"));
    let (status, triage) =
        a.create(json!({ "sessionID": session, "name": "triage", "trigger": hourly(), "action": { "type": "prompt", "prompt": "triage", "to": { "subagent": "general" } } })).await;
    assert_eq!(status, 200, "{triage}");
    assert_eq!(triage["data"]["action"]["to"], json!({ "subagent": "general" }));
    a.req("DELETE", &format!("/api/experimental/automation/{}", triage["data"]["id"].as_str().unwrap()), None).await;

    let (status, body) = a.create(json!({ "sessionID": session, "name": "bad", "trigger": { "type": "every", "every": "soon" }, "action": { "type": "prompt", "prompt": "ping" } })).await;
    assert_eq!((status, body), (400, json!({ "_tag": "InvalidRequestError", "message": "Invalid duration: soon", "kind": "automation" })));
    // A trigger without its type, or a target naming both a session and an agent, is rejected rather than guessed.
    assert_eq!(a.create(json!({ "sessionID": session, "name": "x", "trigger": { "every": "1 hour" }, "action": { "type": "prompt", "prompt": "x" } })).await.0, 400);
    let both = json!({ "type": "prompt", "prompt": "x", "to": { "session": session, "subagent": "general" } });
    assert_eq!(a.create(json!({ "sessionID": session, "name": "x", "trigger": hourly(), "action": both })).await.0, 400);
    assert_eq!(a.list(&format!("?sessionID={session}")).await.1, json!({ "data": [] }));

    // Turn off the directory that still owns the standalone automation. Even unfiltered lists must hide it.
    write(&enabled, json!({ "automations": false }));
    a.engine.runner.as_ref().unwrap().reload_location(&enabled.display().to_string()).await.unwrap();
    assert_eq!(a.list("").await.1, json!({ "data": [] }));
    let (status, body) = a.req("GET", &format!("/api/experimental/automation/{}", standalone["id"].as_str().unwrap()), None).await;
    assert_eq!((status, tag(&body)), (403, "ForbiddenError"));
    let (status, body) = a.create(json!({ "sessionID": session, "name": "x", "trigger": hourly(), "action": { "type": "prompt", "prompt": "ping" } })).await;
    assert_eq!((status, body), (403, json!({ "_tag": "ForbiddenError", "message": "Automations are disabled. Set automations to true in config to enable them." })));
    assert_eq!(tag(&a.list(&format!("?sessionID={session}")).await.1), "ForbiddenError");

    // A code action that would need an interactive approval is refused over stateless HTTP instead of hanging.
    let ask = a.session_at(&a.root.join("ask")).await;
    let (status, body) = a.create(json!({ "sessionID": ask, "name": "blocked", "trigger": hourly(), "action": { "type": "code", "code": "return" } })).await;
    assert_eq!((status, tag(&body)), (403, "ForbiddenError"));
    assert_eq!(body["message"], "Permission execute needs approval, and nobody is present to give it");
}

/// With the flag off, an automation whose folder is gone stays manageable: listed, read, paused and deleted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_automation_whose_folder_is_gone_stays_manageable() {
    let a = api();
    let worktree = a.root.join("worktree");
    write(&worktree, json!({ "automations": true }));
    let session = a.session_at(&worktree).await;
    let (status, created) = a
        .create(json!({ "name": "gone", "directory": worktree, "trigger": hourly(), "action": { "type": "prompt", "prompt": "x", "to": { "session": session } } }))
        .await;
    assert_eq!(status, 200, "{created}");
    let id = created["data"]["id"].as_str().unwrap().to_string();
    std::fs::remove_dir_all(&worktree).unwrap();
    write(&a.cwd, json!({ "automations": false }));
    a.engine.runner.as_ref().unwrap().reload_location(&a.cwd.display().to_string()).await.unwrap();
    assert_eq!(ids(&a.list("").await.1), vec![id.clone()]);
    assert_eq!(ids(&a.list(&format!("?directory={}", enc(&worktree.display().to_string()))).await.1), vec![id.clone()]);
    assert_eq!(a.req("GET", &format!("/api/experimental/automation/{id}"), None).await.0, 200);
    assert_eq!(a.req("POST", &format!("/api/experimental/automation/{id}/pause"), None).await.1["data"]["status"], "paused");
    assert_eq!(a.req("DELETE", &format!("/api/experimental/automation/{id}"), None).await.0, 204);
}

/// `/automation` is listed only where automations are on, and hands the request to the model with the template.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_automation_command_follows_the_flag() {
    let a = api();
    let (_, commands) = a.req("GET", "/api/command", None).await;
    let names: Vec<&str> = commands["data"].as_array().unwrap().iter().filter_map(|c| c["name"].as_str()).collect();
    assert!(names.contains(&"automation"), "{names:?}");
    let command = commands["data"].as_array().unwrap().iter().find(|c| c["name"] == "automation").unwrap();
    assert_eq!(command["description"], "create an automation, or list, pause, resume and delete this project's automations");
    write(&a.cwd, json!({ "automations": false }));
    a.engine.runner.as_ref().unwrap().reload_location(&a.cwd.display().to_string()).await.unwrap();
    // The server drops its location services when it sees the reload's `config.updated` on the bus, in a task of its
    // own (as for a config file change), so the command list follows shortly after rather than at once.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let (_, commands) = a.req("GET", "/api/command", None).await;
        if !commands["data"].as_array().unwrap().iter().any(|c| c["name"] == "automation") {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "/automation is still listed after the reload: {commands}");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(oc_core::automation::COMMAND_TEMPLATE.contains("Request: $ARGUMENTS"));
}

/// A due automation runs from the server loop (`ServerProcess.start` in upstream's test): a code action reads the
/// model catalog through Code Mode and keeps state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runs_due_automations_from_the_server_loop() {
    let a = api();
    let session = a.session_at(&a.cwd).await;
    let code = "const models = await tools.opencode.models({}); console.log(\"models=\" + models.total); return { state: { total: models.total } }";
    let (status, created) =
        a.create(json!({ "sessionID": session, "name": "models", "trigger": { "type": "at", "at": "in 150 millis" }, "action": { "type": "code", "code": code } })).await;
    assert_eq!(status, 200, "{created}");
    let id = created["data"]["id"].as_str().unwrap().to_string();
    let runner = tokio::spawn(a.engine.automations.clone().run());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut current = created["data"].clone();
    while current["status"] == "active" && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        current = a.req("GET", &format!("/api/experimental/automation/{id}"), None).await.1["data"].clone();
    }
    runner.abort();
    assert_eq!((current["status"].as_str(), current["runCount"].as_i64(), current["lastOutcome"].as_str()), (Some("done"), Some(1), Some("fired")), "{current}");
    assert!(current["lastOutput"].as_str().unwrap().contains("models="), "{current}");
    assert!(current["state"]["total"].is_number(), "{current}");
}

/// A shell action (2.4.1, #65186) created over HTTP and run by the server loop: the command's exit code and output
/// become the automation's last output, and the creating session gets no prompt.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runs_a_shell_action_from_the_server_loop() {
    let a = api();
    let session = a.session_at(&a.cwd).await;
    let (status, created) = a
        .create(json!({ "sessionID": session, "name": "echo", "trigger": { "type": "at", "at": "in 150 millis" }, "action": { "type": "shell", "command": "echo from-automation; exit 3" } }))
        .await;
    assert_eq!(status, 200, "{created}");
    assert_eq!(created["data"]["action"], json!({ "type": "shell", "command": "echo from-automation; exit 3" }));
    let id = created["data"]["id"].as_str().unwrap().to_string();
    let runner = tokio::spawn(a.engine.automations.clone().run());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut current = created["data"].clone();
    while current.get("lastOutput").is_none() && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        current = a.req("GET", &format!("/api/experimental/automation/{id}"), None).await.1["data"].clone();
    }
    runner.abort();
    assert_eq!((current["status"].as_str(), current["runCount"].as_i64(), current["lastOutcome"].as_str()), (Some("done"), Some(1), Some("failed")), "{current}");
    assert_eq!(current["lastOutput"], "Exit code: 3\nfrom-automation\n", "{current}");
    let (_, messages) = a.req("GET", &format!("/api/session/{session}/message"), None).await;
    assert_eq!(messages["data"], json!([]), "{messages}");
}

/// `ctx.experimental.automation.create/list/delete` (2.4.1, #65747, `plugin/automation.test.ts`): a plugin's calls reach
/// the automation handlers, so they create, list by directory and delete like an HTTP caller, and deleting a missing
/// automation fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plugins_create_list_and_delete_automations() {
    use oc_plugin::PluginApi;
    let a = api();
    let session = a.session_at(&a.cwd).await;
    let api = oc_backend::plugins::BackendApi::new(&a.engine, a.router.clone());
    let call = |method: &str, input: Value| oc_plugin::ApiRequest {
        location: oc_plugin::LocationRef::new(a.cwd.display().to_string()),
        plugin: Some("test.automation".into()),
        method: method.into(),
        input,
    };
    let directory = a.cwd.display().to_string();
    let created = api
        .call(call(
            "automation.create",
            json!({ "directory": directory, "name": "promise inbox", "trigger": { "type": "cron", "cron": "0 9 * * *" }, "action": { "type": "prompt", "prompt": "Check the balance now.", "to": { "session": session } } }),
        ))
        .await
        .unwrap();
    assert_eq!((created["name"].as_str(), created["status"].as_str()), (Some("promise inbox"), Some("active")), "{created}");
    assert_eq!(created["action"]["to"], json!({ "session": session }));
    let id = created["id"].as_str().unwrap().to_string();
    let listed = api.call(call("automation.list", json!({ "directory": directory }))).await.unwrap();
    assert!(listed.as_array().unwrap().iter().any(|i| i["id"] == id.as_str()), "{listed}");
    assert_eq!(api.call(call("automation.delete", json!({ "automationID": id }))).await.unwrap(), Value::Null);
    assert!(api.call(call("automation.delete", json!({ "automationID": id }))).await.is_err());
    assert_eq!(a.list("").await.1, json!({ "data": [] }));
}
