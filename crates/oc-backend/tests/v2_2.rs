//! opencode 2.2.2 API behaviour: session routes that no longer need their location, forms of unloaded
//! locations, canonical location directories, media blobs, and the endpoints of features ocd answers as
//! disabled (schedules, GUI plugin bundles, image and decision generation). Expected bodies were checked
//! against the 2.2.2 binary.

mod common;

use base64::Engine as _;
use common::*;
use serde_json::{Value, json};

fn dir_of(h: &Harness, name: &str) -> String {
    let d = h.cwd.parent().unwrap().join(name);
    std::fs::create_dir_all(&d).unwrap();
    d.display().to_string()
}

async fn session_at(h: &Harness, directory: &str) -> Value {
    let r = h.post("/api/session", json!({ "location": { "directory": directory } })).await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    r.json()["data"].clone()
}

#[tokio::test]
async fn session_routes_work_after_the_directory_is_deleted() {
    let h = harness();
    let gone = dir_of(&h, "gone");
    let id = session_at(&h, &gone).await["id"].as_str().unwrap().to_string();
    std::fs::remove_dir_all(&gone).unwrap();
    // An empty session has nothing to fork (400), but the route no longer answers LocationNotFoundError.
    let r = h.post(&format!("/api/session/{id}/fork"), json!({})).await;
    assert_eq!(r.status, 400, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(h.post(&format!("/api/session/{id}/agent"), json!({ "agent": "build" })).await.status, 204);
    let r = h.req("POST", &format!("/api/session/{id}/interrupt"), None).await;
    assert_eq!((r.status.as_u16(), r.json()), (200, json!({ "interrupted": false })));
    assert_eq!(h.req("POST", &format!("/api/experimental/session/{id}/wait"), None).await.status, 204);
    let put = h.req("PUT", &format!("/api/experimental/session/{id}/instructions/entries/k"), Some(json!({ "value": 1 }))).await;
    assert_eq!(put.status, 204);
    let r = h.get(&format!("/api/experimental/session/{id}/instructions/entries")).await;
    assert_eq!(r.json(), json!({ "data": [{ "key": "k", "value": 1 }] }));
    assert_eq!(h.req("DELETE", &format!("/api/experimental/session/{id}/instructions/entries/k"), None).await.status, 204);
    assert_eq!(h.req("PATCH", &format!("/api/session/{id}"), Some(json!({ "metadata": { "a": 1 } }))).await.status, 204);
    // A missing session is still a 404 SessionNotFoundError.
    let r = h.post("/api/session/ses_ffffffffffffZZZZZZZZZZZZZZ/fork", json!({})).await;
    assert_eq!(r.status, 404);
    assert_eq!(r.json()["_tag"], "SessionNotFoundError");
    // No location middleware on the project list or saved permission updates.
    assert_eq!(h.req_in(Some(&gone), "GET", "/api/project", None).await.status, 200);
    assert_eq!(h.req_in(Some(&gone), "DELETE", "/api/permission/saved/per_x", None).await.status, 204);
}

#[tokio::test]
async fn polling_forms_never_boots_an_unloaded_location() {
    let h = harness();
    let other = dir_of(&h, "other");
    let id = session_at(&h, &other).await["id"].as_str().unwrap().to_string();
    let r = h.req_in(Some(&other), "DELETE", "/api/debug/location", None).await;
    assert!(r.status.is_success(), "{}", r.status);
    let loaded = |h: &Harness| h.engine.locations.keys();
    assert!(!loaded(&h).contains(&other));
    let r = h.get(&format!("/api/session/{id}/form")).await;
    assert_eq!((r.status.as_u16(), r.json()), (200, json!({ "data": [] })));
    assert!(!loaded(&h).contains(&other), "the form poll booted the location");
}

#[tokio::test]
async fn equivalent_directories_share_one_location() {
    let h = harness();
    let dir = h.dir();
    let s = session_at(&h, &format!("{dir}/")).await;
    assert_eq!(s["location"]["directory"], json!(dir));
    let s = session_at(&h, &format!("{dir}/./")).await;
    assert_eq!(s["location"]["directory"], json!(dir));
    let r = h.get(&format!("/api/session?directory={}", enc(&dir))).await;
    assert_eq!(r.json()["data"].as_array().unwrap().len(), 2);
    let r = h.req_in(Some(&format!("{dir}//")), "GET", "/api/location", None).await;
    assert_eq!(r.json()["directory"], json!(dir));
    assert_eq!(h.engine.locations.keys().iter().filter(|k| k.starts_with(&dir)).count(), 1);
}

#[tokio::test]
async fn media_blobs_are_served_while_referenced() {
    let h = harness();
    let id = h.session().await;
    let bytes = b"\x89PNG data";
    let hash = "a".repeat(64);
    h.app
        .db
        .with(|c| {
            c.execute(
                "INSERT INTO blob (hash, mime, bytes, width, height, data, time_created) VALUES (?, 'image/png', ?, NULL, NULL, ?, 0)",
                rusqlite::params![hash, bytes.len() as i64, &bytes[..]],
            )
        })
        .unwrap();
    let uri = format!("/api/experimental/media/{hash}");
    let r = h.get(&uri).await;
    assert_eq!(r.status, 404, "an unreferenced blob is not served");
    assert_eq!(
        r.json(),
        json!({ "_tag": "FileNotFoundError", "path": uri, "message": format!("Media blob not found: {hash}") })
    );
    h.app
        .db
        .with(|c| c.execute("INSERT INTO blob_ref (session_id, message_id, hash, seq) VALUES (?, 'msg_x', ?, 0)", rusqlite::params![id, hash]))
        .unwrap();
    let r = h.get(&uri).await;
    assert_eq!(r.status, 200);
    assert_eq!(&r.body[..], bytes);
    assert_eq!(r.headers["content-type"], "image/png");
    // A prompt can send the ref back; it is admitted with the stored bytes.
    let r = h
        .post(&format!("/api/session/{id}/prompt"), json!({ "text": "look", "files": [{ "uri": format!("media:{hash}"), "name": "a.png" }] }))
        .await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let file = &r.json()["data"]["payload"]["files"][0];
    assert_eq!(file["data"], json!(base64::engine::general_purpose::STANDARD.encode(bytes)));
    assert_eq!(file["mime"], "image/png");
    assert_eq!(file["source"], json!({ "type": "inline" }));
    let missing = format!("media:{}", "b".repeat(64));
    let r = h.post(&format!("/api/session/{id}/prompt"), json!({ "text": "x", "files": [{ "uri": missing }] })).await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["message"], json!(format!("Attachment is no longer stored: {missing}")));
}

#[tokio::test]
async fn data_attachments_keep_their_source() {
    let h = harness();
    let id = h.session().await;
    let r = h
        .post(
            &format!("/api/session/{id}/prompt"),
            json!({ "text": "x", "files": [{ "uri": "data:text/plain;base64,aGk=", "source": { "type": "uri", "uri": "file:///tmp/a.txt" } }] }),
        )
        .await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(r.json()["data"]["payload"]["files"][0]["source"], json!({ "type": "uri", "uri": "file:///tmp/a.txt" }));
}

#[tokio::test]
async fn schedules_answer_as_disabled() {
    let h = harness();
    let id = h.session().await;
    let r = h.post("/api/experimental/schedule", json!({ "sessionID": id, "when": { "in": "1h" }, "prompt": "x" })).await;
    assert_eq!(r.status, 403);
    assert_eq!(r.json(), json!({ "_tag": "ForbiddenError", "message": "Scheduled tasks are disabled; set experimental.schedules in config" }));
    let r = h.post("/api/experimental/schedule", json!({ "sessionID": "ses_ffffffffffffZZZZZZZZZZZZZZ", "when": { "in": "1h" }, "prompt": "x" })).await;
    assert_eq!(r.status, 404);
    assert_eq!(r.json()["_tag"], "SessionNotFoundError");
    assert_eq!(h.get(&format!("/api/experimental/schedule?sessionID={id}")).await.status, 403);
    let r = h.get("/api/experimental/schedule/sch_ffffffffffffZZZZZZZZZZZZZZ").await;
    assert_eq!(r.status, 404);
    assert_eq!(
        r.json(),
        json!({ "_tag": "ScheduleNotFoundError", "scheduleID": "sch_ffffffffffffZZZZZZZZZZZZZZ", "message": "Schedule not found: sch_ffffffffffffZZZZZZZZZZZZZZ" })
    );
    // A schedule row another release created reads as forbidden while its session exists.
    h.app
        .db
        .with(|c| {
            c.execute(
                "INSERT INTO schedule (id, session_id, action, \"when\", tz, time_created, time_updated) VALUES ('sch_1', ?, '{}', '{}', 'UTC', 0, 0)",
                [&id],
            )
        })
        .unwrap();
    assert_eq!(h.req("POST", "/api/experimental/schedule/sch_1/pause", None).await.status, 403);
    assert_eq!(h.req("DELETE", "/api/experimental/schedule/sch_1", None).await.status, 403);
}

#[tokio::test]
async fn gui_plugin_bundles_are_never_found() {
    let h = harness();
    let r = h.get("/api/plugin/gui?id=acme").await;
    assert_eq!(r.status, 404);
    assert_eq!(r.json(), json!({ "_tag": "PluginNotFoundError", "pluginID": "acme", "message": "No active plugin with a ./gui-experimental bundle: acme" }));
}

#[tokio::test]
async fn decision_question_names_must_be_unique() {
    let h = harness();
    let q = json!({ "type": "predicate", "name": "a", "instructions": "is it?" });
    let r = h
        .post(
            "/api/experimental/generate/decisions",
            json!({ "model": { "providerID": "p", "id": "m" }, "input": [{ "type": "text", "text": "t" }], "questions": [q, q] }),
        )
        .await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json(), json!({ "_tag": "InvalidRequestError", "message": "Decision question names must be unique", "field": "questions" }));
}

/// `generate.test.ts`: refs to unknown models are 404s, refs to another type's model 422s.
#[tokio::test]
async fn generate_reports_typed_model_errors() {
    unsafe { std::env::set_var("OPENCODE_DISABLE_MODELS_FETCH", "1") };
    let root = tempfile::tempdir().unwrap();
    let root_path = std::fs::canonicalize(root.path()).unwrap();
    let g = global(&root_path);
    std::fs::write(
        g.config.join("opencode.json"),
        json!({ "providers": { "custom": { "name": "Custom", "package": "@ai-sdk/openai-compatible",
            "settings": { "baseURL": "http://127.0.0.1:9/v1", "apiKey": "k" }, "models": { "m": { "name": "M" } } } } })
        .to_string(),
    )
    .unwrap();
    let cwd = root_path.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let app = oc_core::app::App::new(g, oc_storage::Database::memory().unwrap(), "1.2.3");
    let runner = oc_core::runner::Engine::new(app.clone(), &oc_core::services::default_services(&app));
    let opts = oc_server::ServerOptions { cwd: Some(cwd.display().to_string()), version: "1.2.3".into(), ready: true, ..Default::default() };
    let (router, _) = oc_backend::router(app.clone(), Some(runner), &opts).unwrap();
    let post = |path: &'static str, body: Value| {
        let router = router.clone();
        async move {
            use tower::ServiceExt;
            let res = router
                .oneshot(
                    axum::http::Request::post(path)
                        .header("content-type", "application/json")
                        .body(axum::body::Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = res.status().as_u16();
            let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
            (status, serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null))
        }
    };
    let unknown = json!({ "_tag": "ModelNotFoundError", "providerID": "nope", "modelID": "x", "message": "Model not found: nope/x" });
    for path in ["/api/experimental/generate/text", "/api/experimental/generate"] {
        assert_eq!(post(path, json!({ "prompt": "hi", "model": { "providerID": "nope", "id": "x" } })).await, (404, unknown.clone()));
    }
    assert_eq!(
        post("/api/experimental/generate/image", json!({ "prompt": "hi", "model": { "providerID": "nope", "id": "x" } })).await,
        (404, unknown.clone())
    );
    assert_eq!(
        post("/api/experimental/generate/image", json!({ "prompt": "hi", "model": { "providerID": "custom", "id": "m" } })).await,
        (
            422,
            json!({ "_tag": "UnsupportedModelError", "providerID": "custom", "modelID": "m", "capability": "image",
                    "message": "custom/m is a chat model, not a image model" })
        )
    );
    let q = json!({ "type": "predicate", "name": "a", "instructions": "is it?" });
    assert_eq!(
        post(
            "/api/experimental/generate/decisions",
            json!({ "model": { "providerID": "custom", "id": "m" }, "input": [{ "type": "text", "text": "t" }], "questions": [q] })
        )
        .await,
        (
            422,
            json!({ "_tag": "UnsupportedModelError", "providerID": "custom", "modelID": "m", "capability": "decision",
                    "message": "custom/m is a chat model, not a decision model" })
        )
    );
    let (status, body) =
        post("/api/experimental/generate/text", json!({ "prompt": "hi", "model": { "providerID": "custom", "id": "m", "variant": "nope" } })).await;
    assert_eq!(status, 400);
    assert_eq!(body["field"], "model.variant");
}
