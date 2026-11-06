//! opencode 2.2.5 API behaviour: `integration.connect` (listing, connecting, credential creation), `integration.use`
//! by account, the artifact routes answering as disabled, Code Mode schedule checks, and session deletion clearing
//! the session's background jobs. Expected bodies were checked against the 2.2.5 binary.

mod common;

use common::*;
use serde_json::{Value, json};

fn write_config(dir: &std::path::Path, value: Value) {
    std::fs::write(dir.join("opencode.json"), value.to_string()).unwrap();
}

fn denied(id: &str) -> Value {
    json!({
        "_tag": "InvalidRequestError",
        "message": format!("Blocked by configuration policy: connecting {id} is not allowed"),
        "kind": "integration_policy",
    })
}

#[tokio::test]
async fn integration_connect_filters_the_list_and_refuses_connections_before_lookup() {
    let h = harness();
    write_config(
        &h.cwd,
        json!({ "policies": [
            { "action": "integration.connect", "resource": "github-copilot", "effect": "deny" },
            { "action": "integration.connect", "resource": "nope*", "effect": "deny" },
            { "action": "integration.connect", "resource": "https://wk.example.com", "effect": "deny" },
        ]}),
    );
    let ids: Vec<String> =
        h.get("/api/integration").await.json()["data"].as_array().unwrap().iter().map(|i| i["id"].as_str().unwrap().to_string()).collect();
    assert!(!ids.iter().any(|i| i == "github-copilot"));
    assert!(ids.iter().any(|i| i == "anthropic"));
    for (path, body) in [
        ("/api/integration/github-copilot/connect/key", json!({ "key": "x" })),
        ("/api/integration/github-copilot/connect/external", json!({ "methodID": "m" })),
        ("/api/integration/github-copilot/connect/oauth", json!({ "methodID": "m" })),
        ("/api/integration/github-copilot/connect/command", json!({ "methodID": "m" })),
    ] {
        let r = h.post(path, body).await;
        assert_eq!((r.status.as_u16(), r.json()), (400, denied("github-copilot")), "{path}");
    }
    // The policy answers before the 404 of an unknown integration.
    let r = h.post("/api/integration/nope-x/connect/key", json!({ "key": "x" })).await;
    assert_eq!((r.status.as_u16(), r.json()), (400, denied("nope-x")));
    let r = h.post("/api/integration/zzz/connect/key", json!({ "key": "x" })).await;
    assert_eq!(r.status, 404);
    // A well-known source is keyed by its URL without trailing slashes, checked before any discovery.
    let r = h.post("/api/experimental/integration/wellknown", json!({ "url": "https://wk.example.com//" })).await;
    assert_eq!((r.status.as_u16(), r.json()), (400, denied("https://wk.example.com")));
}

#[tokio::test]
async fn credential_creation_reads_integration_connect_from_the_global_config() {
    let h = harness();
    write_config(&h.app.global.config, json!({ "policies": [{ "action": "integration.connect", "resource": "anthropic", "effect": "deny" }] }));
    // A project policy does not apply to credentials, which are global.
    write_config(&h.cwd, json!({ "policies": [{ "action": "integration.connect", "resource": "github-copilot", "effect": "deny" }] }));
    let r = h.post("/api/credential", json!({ "integrationID": "anthropic", "value": { "type": "key", "key": "sk" } })).await;
    assert_eq!((r.status.as_u16(), r.json()), (400, denied("anthropic")));
    let r = h.post("/api/credential", json!({ "integrationID": "github-copilot", "value": { "type": "key", "key": "sk" } })).await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    assert!(h.get("/api/credential").await.json()["data"].as_array().unwrap().iter().all(|c| c["integrationID"] != "anthropic"));
}

#[tokio::test]
async fn integration_use_withholds_providers_and_web_search_by_account() {
    let h = harness();
    write_config(
        &h.cwd,
        json!({ "policies": [
            { "action": "integration.use", "resource": "opencode#org_1", "effect": "deny" },
            { "action": "integration.use", "resource": "exa", "effect": "deny" },
        ]}),
    );
    let providers = || async {
        let list = h.get("/api/provider").await.json()["data"].clone();
        list.as_array().unwrap().iter().map(|p| p["id"].as_str().unwrap().to_string()).collect::<Vec<_>>()
    };
    assert!(providers().await.iter().any(|p| p == "opencode"));
    let websearch: Vec<String> = h.get("/api/websearch/provider").await.json()["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(websearch, ["firecrawl", "parallel", "tavily", "tinyfish"]);
    // A stored OpenCode credential for org_1 makes `opencode#org_1` the resource of everything it supplies.
    let r = h
        .post(
            "/api/credential",
            json!({ "integrationID": "opencode", "value": { "type": "key", "key": "sk", "metadata": { "orgID": "org_1" } } }),
        )
        .await;
    assert_eq!(r.status, 200);
    assert!(!providers().await.iter().any(|p| p == "opencode"));
}

#[tokio::test]
async fn config_shows_converted_policies_and_2_2_5_keys() {
    let h = harness();
    write_config(
        &h.cwd,
        json!({
            "compaction": { "buffer": "20%" },
            "experimental": { "artifacts": false, "portable_shell_scanner": true },
            "policies": [
                { "action": "integration.use", "resource": "mcp:github", "effect": "deny" },
                { "action": "integration.use", "resource": "*", "effect": "allow" },
            ],
        }),
    );
    let entries = h.get("/api/config").await.json();
    let document = entries
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["path"].as_str().is_some_and(|p| p.starts_with(&h.dir())))
        .cloned()
        .unwrap();
    assert_eq!(
        document["info"],
        json!({
            "policies": [
                { "action": "mcp.use", "resource": "github", "effect": "deny" },
                { "action": "integration.use", "resource": "*", "effect": "allow" },
            ],
            "compaction": { "buffer": "20%" },
            "experimental": { "portable_shell_scanner": true, "artifacts": false },
        })
    );
}

fn forbidden() -> Value {
    json!({ "_tag": "ForbiddenError", "message": "Artifacts are disabled; set experimental.artifacts in config" })
}

#[tokio::test]
async fn artifact_routes_validate_resolve_the_location_then_answer_disabled() {
    let h = harness();
    let sid = h.session().await;
    let missing = "ses_000000000000aaaaaaaaaaaaaa";
    let files = json!({ "index.html": "x" });
    for (method, path, body) in [
        ("GET", "/api/experimental/artifacts/providers".to_string(), None),
        ("GET", "/api/experimental/artifacts".to_string(), None),
        ("GET", format!("/api/experimental/artifacts?sessionID={sid}"), None),
        ("GET", "/api/experimental/artifacts/art_x".to_string(), None),
        ("GET", "/api/experimental/artifacts/art_x/view".to_string(), None),
        ("GET", "/api/experimental/artifacts/art_x/view/".to_string(), None),
        ("GET", "/api/experimental/artifacts/art_x/view/a/b.html".to_string(), None),
        ("POST", "/api/experimental/artifacts".to_string(), Some(json!({ "title": "t", "files": files }))),
        ("PATCH", "/api/experimental/artifacts/art_x".to_string(), Some(json!({ "files": files }))),
        ("DELETE", "/api/experimental/artifacts/art_x".to_string(), None),
    ] {
        let r = h.req(method, &path, body).await;
        assert_eq!((r.status.as_u16(), r.json()), (403, forbidden()), "{method} {path}");
    }
    let r = h.get("/api/experimental/artifacts/nope").await;
    assert_eq!(
        (r.status.as_u16(), r.json()),
        (400, json!({ "_tag": "InvalidRequestError", "message": "Expected a string starting with \"art_\"\n  at [\"artifactID\"]", "kind": "Params" }))
    );
    // Validation comes before the location.
    let r = h.req_in(Some("/nonexistent-dir"), "POST", "/api/experimental/artifacts", Some(json!({}))).await;
    assert_eq!(r.json()["message"], "Missing key\n  at [\"title\"]");
    let r = h.req_in(Some("/nonexistent-dir"), "GET", "/api/experimental/artifacts/art_x", None).await;
    assert_eq!((r.status.as_u16(), r.json()["_tag"].clone()), (404, json!("LocationNotFoundError")));
    // Without an explicit directory the session decides the location.
    let r = h.post("/api/experimental/artifacts", json!({ "title": "t", "files": files, "sessionID": missing })).await;
    assert_eq!((r.status.as_u16(), r.json()["_tag"].clone()), (404, json!("SessionNotFoundError")));
    let r = h.req_in(Some(&h.dir()), "POST", "/api/experimental/artifacts", Some(json!({ "title": "t", "files": files, "sessionID": missing }))).await;
    assert_eq!(r.status, 403);
}

#[tokio::test]
async fn schedule_code_checks_pass_validation_and_answer_disabled() {
    let h = harness();
    let sid = h.session().await;
    let r = h.post("/api/experimental/schedule", json!({ "sessionID": sid, "prompt": "p", "if": { "code": "return true" } })).await;
    assert_eq!(
        (r.status.as_u16(), r.json()),
        (403, json!({ "_tag": "ForbiddenError", "message": "Scheduled tasks are disabled; set experimental.schedules in config" }))
    );
    let r = h.post("/api/experimental/schedule", json!({ "sessionID": sid, "prompt": "p", "if": { "run": "true", "code": "x" } })).await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["message"], "Expected never\n  at [\"if\"][\"code\"]\nExpected never\n  at [\"if\"][\"run\"]");
}

#[tokio::test]
async fn deleting_a_session_cancels_its_jobs_and_deletes_their_rows() {
    let h = harness();
    let sid = h.session().await;
    let recovery = oc_tools::host::JobRecovery::Shell { session_id: sid.clone(), shell_id: "sh_1".into(), command: "sleep 100".into() };
    let job = h.app.jobs.start("sh_1".into(), "shell".into(), None, Some(recovery), Box::pin(futures::future::pending()));
    h.app.jobs.background(&job.id);
    let other = oc_tools::host::JobRecovery::Shell { session_id: "ses_other".into(), shell_id: "sh_2".into(), command: "true".into() };
    let kept = h.app.jobs.start("sh_2".into(), "shell".into(), None, Some(other), Box::pin(futures::future::pending()));
    h.app.jobs.background(&kept.id);
    let count = |session: &str| -> i64 {
        h.app.db.with(|c| c.query_row("SELECT count(*) FROM job WHERE session_id = ?", [session], |r| r.get(0))).unwrap()
    };
    assert_eq!(count(&sid), 1);
    assert_eq!(h.req("DELETE", &format!("/api/session/{sid}"), None).await.status, 204);
    assert_eq!(count(&sid), 0);
    assert!(h.app.jobs.get("sh_1").is_none());
    assert_eq!(count("ses_other"), 1);
    assert_eq!(h.app.jobs.get("sh_2").map(|j| j.status), Some(oc_tools::host::JobStatus::Running));
}
