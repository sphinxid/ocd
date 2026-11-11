//! Opt-in artifacts and stored media over HTTP (`server/test/artifact.test.ts`, `server/test/media.test.ts`):
//! the local artifact provider with its view route, project scoping and errors; `experimental.media_blob` imports,
//! reads with and without `X-OpenCode-Capabilities: experimental-media-uri`, the media route, and media sent back
//! as attachments. Bodies were checked against the 2.2.5 binary (parity groups `artifact` and `media`).

mod common;

use axum::body::Body;
use axum::http::Request;
use base64::Engine as _;
use common::*;
use serde_json::{Value, json};
use tower::ServiceExt;

fn write_config(dir: &std::path::Path, value: Value) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("opencode.json"), value.to_string()).unwrap();
}

/// A request with extra headers; `(status, headers, body)`.
async fn send(h: &Harness, method: &str, uri: &str, headers: &[(&str, &str)], body: Option<Value>) -> Res {
    let mut b = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let body = match body {
        Some(v) => {
            b = b.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&v).unwrap())
        }
        None => Body::empty(),
    };
    let res = h.router.clone().oneshot(b.body(body).unwrap()).await.unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    Res { status, body, headers }
}

fn header<'a>(r: &'a Res, name: &str) -> Option<&'a str> {
    r.headers.get(name).and_then(|v| v.to_str().ok())
}

#[tokio::test]
async fn manages_local_artifacts_through_the_api_when_enabled() {
    let h = harness();
    let enabled = h.root.path().join("enabled");
    let other = h.root.path().join("enabled-other");
    let disabled = h.root.path().join("disabled");
    write_config(&enabled, json!({ "experimental": { "artifacts": true } }));
    write_config(&other, json!({ "experimental": { "artifacts": true } }));
    write_config(&disabled, json!({}));
    let (enabled, other, disabled) = (enabled.display().to_string(), other.display().to_string(), disabled.display().to_string());

    // A disabled location answers 403.
    let r = h.req_in(Some(&disabled), "GET", "/api/experimental/artifacts", None).await;
    assert_eq!(r.json()["_tag"], "ForbiddenError");

    let r = h.req_in(Some(&enabled), "GET", "/api/experimental/artifacts/providers", None).await;
    assert_eq!(r.json()["data"], json!([{ "id": "local", "name": "Local", "capabilities": ["static"], "default": true }]));

    let r = h.req_in(None, "POST", "/api/session", Some(json!({ "location": { "directory": enabled } }))).await;
    let session = r.json()["data"]["id"].as_str().unwrap().to_string();

    let r = h
        .req_in(
            Some(&enabled),
            "POST",
            "/api/experimental/artifacts",
            Some(json!({
                "sessionID": session,
                "title": " Wait stats ",
                "files": {
                    "index.html": "<!doctype html><h1>v1</h1><script src='./assets/app.js'></script>",
                    "assets/app.js": "window.VERSION = 1",
                },
            })),
        )
        .await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let created = r.json()["data"].clone();
    let id = created["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("art_"));
    assert_eq!(created["provider"], "local");
    assert_eq!(created["sessionID"], json!(session));
    assert_eq!(created["title"], "Wait stats");
    assert_eq!(created["entry"], "index.html");
    assert_eq!(created["url"], json!(format!("/api/experimental/artifacts/{id}/view/")));
    // The files and the KV record.
    let dir = h.app.global.data.join("artifacts").join(&id);
    assert_eq!(std::fs::read_to_string(dir.join("assets/app.js")).unwrap(), "window.VERSION = 1");
    let record = h.app.db.kv_get(&format!("artifact:item:{id}")).unwrap().unwrap();
    assert_eq!(record["directory"], json!(enabled));
    assert_eq!(serde_json::to_string(&record["info"]).unwrap().find("\"id\""), Some(1));

    // `/view` redirects to `/view/` (relative assets resolve); the view needs no location.
    let r = send(&h, "GET", &format!("/api/experimental/artifacts/{id}/view?x=1"), &[], None).await;
    assert_eq!(r.status, 308);
    assert_eq!(header(&r, "location"), Some(format!("/api/experimental/artifacts/{id}/view/?x=1").as_str()));
    let r = send(&h, "GET", &format!("/api/experimental/artifacts/{id}/view/"), &[], None).await;
    assert_eq!(r.status, 200);
    assert_eq!(header(&r, "content-type"), Some("text/html; charset=utf-8"));
    assert_eq!(header(&r, "content-security-policy"), Some("sandbox allow-scripts"));
    assert_eq!(header(&r, "access-control-allow-origin"), Some("*"));
    assert_eq!(header(&r, "cache-control"), Some("private, no-cache"));
    assert_eq!(header(&r, "x-content-type-options"), Some("nosniff"));
    assert!(String::from_utf8_lossy(&r.body).contains("<h1>v1</h1>"));
    let r = send(&h, "GET", &format!("/api/experimental/artifacts/{id}/view/assets/app.js"), &[], None).await;
    assert_eq!((header(&r, "content-type"), r.body.as_ref()), (Some("text/javascript; charset=utf-8"), b"window.VERSION = 1".as_ref()));
    // Dot segments resolve like the WHATWG URL parser (also percent-encoded ones).
    let r = send(&h, "GET", &format!("/api/experimental/artifacts/{id}/view/x/%2E%2E/assets/app.js"), &[], None).await;
    assert_eq!(r.status, 200);
    let r = send(&h, "GET", &format!("/api/experimental/artifacts/{id}/view/missing.html"), &[], None).await;
    assert_eq!(
        (r.status.as_u16(), r.json()),
        (404, json!({ "_tag": "FileNotFoundError", "path": "missing.html", "message": "Artifact file not found: missing.html" }))
    );

    // Another enabled project neither lists nor reads it.
    let r = h.req_in(Some(&other), "GET", "/api/experimental/artifacts", None).await;
    assert_eq!(r.json()["data"], json!([]));
    let r = h.req_in(Some(&other), "GET", &format!("/api/experimental/artifacts/{id}"), None).await;
    assert_eq!(
        (r.status.as_u16(), r.json()),
        (404, json!({ "_tag": "ArtifactNotFoundError", "artifactID": id, "message": format!("Artifact not found: {id}") }))
    );

    // Update replaces the files at the same ID and URL, keeping the entry.
    let r = send(&h, "PATCH", &format!("/api/experimental/artifacts/{id}"), &[], Some(json!({ "files": { "index.html": "<h1>v2</h1>" } }))).await;
    assert_eq!(r.json()["data"]["url"], created["url"]);
    assert!(!dir.join("assets").exists());
    let r = send(&h, "GET", &format!("/api/experimental/artifacts/{id}"), &[], None).await;
    assert_eq!(r.json()["data"]["files"], json!({ "index.html": "<h1>v2</h1>" }));

    // Validation.
    for (body, field, message) in [
        (json!({ "title": " ", "files": { "a": "b" } }), "title", "Artifact title must not be empty"),
        (json!({ "title": "t", "files": {} }), "files", "Artifact must include at least one file"),
        (json!({ "title": "t", "files": { "../escape.html": "nope" } }), "files", "Artifact file path must stay relative: \"../escape.html\""),
        (
            json!({ "title": "t", "files": { "a": "1", "b": "2" } }),
            "entry",
            "Include \"index.html\" or \"index.md\", or specify \"entry\" when publishing multiple files",
        ),
    ] {
        let r = h.req_in(Some(&enabled), "POST", "/api/experimental/artifacts", Some(body)).await;
        assert_eq!(
            (r.status.as_u16(), r.json()),
            (400, json!({ "_tag": "InvalidRequestError", "message": message, "kind": "artifact_invalid", "field": field }))
        );
    }
    let r = h.req_in(Some(&enabled), "POST", "/api/experimental/artifacts", Some(json!({ "title": "t", "files": { "a": "b" }, "provider": "s3" }))).await;
    assert_eq!(
        (r.status.as_u16(), r.json()),
        (404, json!({ "_tag": "ProviderNotFoundError", "providerID": "s3", "message": "Artifact provider not found: s3" }))
    );

    // Delete removes the record and the files.
    let r = h.req_in(Some(&enabled), "DELETE", &format!("/api/experimental/artifacts/{id}"), None).await;
    assert_eq!(r.status, 204);
    assert!(!dir.exists());
    assert!(h.app.db.kv_get(&format!("artifact:item:{id}")).unwrap().is_none());
    let r = h.req_in(Some(&enabled), "GET", &format!("/api/experimental/artifacts/{id}"), None).await;
    assert_eq!(r.json()["_tag"], "ArtifactNotFoundError");
}

// ---- media -------------------------------------------------------------------------------------------------------

const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

fn b64(s: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(s)
}

fn hash(base64: &str) -> String {
    oc_util::hash::sha256(base64::engine::general_purpose::STANDARD.decode(base64).unwrap())
}

fn size(base64: &str) -> usize {
    base64::engine::general_purpose::STANDARD.decode(base64).unwrap().len()
}

const REFS: (&str, &str) = ("x-opencode-capabilities", "other, Experimental-Media-URI");

#[tokio::test]
async fn serves_stored_media_as_refs_only_to_clients_that_opt_in_while_media_blob_is_on() {
    let h = harness();
    write_config(&h.cwd, json!({ "experimental": { "media_blob": true } }));
    let (text, svg, html) = (b64("workspace file contents"), b64("<svg xmlns=\"http://www.w3.org/2000/svg\" onload=\"alert(1)\"/>"), b64("<!doctype html><script>alert(1)</script>"));
    let user = json!({
        "id": "msg_media_user", "type": "user", "text": "See attached",
        "files": [
            { "data": PNG, "mime": "image/png", "source": { "type": "inline" }, "name": "shot.png" },
            { "data": text, "mime": "text/plain", "source": { "type": "uri", "uri": "file:///workspace/readme.txt" }, "name": "readme.txt" },
            { "data": svg, "mime": "image/svg+xml", "source": { "type": "inline" }, "name": "xss.svg" },
            { "data": html, "mime": "text/html", "source": { "type": "inline" }, "name": "xss.html" },
        ],
        "time": { "created": 100 },
    });
    let assistant = json!({
        "id": "msg_media_assistant", "type": "assistant", "agent": "build", "model": { "providerID": "test", "id": "test" },
        "content": [{
            "type": "tool", "id": "call_read_img", "name": "read", "executed": true,
            "state": { "status": "completed", "input": { "path": "shot.png" },
                       "content": [{ "type": "file", "uri": format!("data:image/png;base64,{PNG}"), "mime": "image/png", "name": "shot.png" }] },
            "time": { "created": 110, "ran": 111, "completed": 112 },
        }],
        "finish": "stop", "time": { "created": 110, "completed": 120 },
    });
    let template = h.post("/api/session", json!({ "title": "Media wire" })).await.json()["data"].clone();
    let sid = "ses_0000000000aaaMediaWireTest";
    let mut info = template.clone();
    info["id"] = json!(sid);
    let r = h.post("/api/experimental/session/import", json!({ "info": info, "messages": [user, assistant] })).await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));

    // Without the capability, reads are what was imported.
    let plain = h.get(&format!("/api/session/{sid}/message?order=asc")).await.json();
    assert!(plain["data"].is_array(), "{plain}");
    assert_eq!(plain["data"], json!([user, assistant]));
    let refs = send(&h, "GET", &format!("/api/session/{sid}/message?order=asc"), &[REFS], None).await.json();
    let file = |data: &str, mime: &str, source: Value, name: &str| {
        json!({ "mime": mime, "source": source, "name": name, "uri": format!("media:{}", hash(data)), "bytes": size(data) })
    };
    assert_eq!(
        refs["data"][0]["files"],
        json!([
            file(PNG, "image/png", json!({ "type": "inline" }), "shot.png"),
            file(&text, "text/plain", json!({ "type": "uri", "uri": "file:///workspace/readme.txt" }), "readme.txt"),
            file(&svg, "image/svg+xml", json!({ "type": "inline" }), "xss.svg"),
            file(&html, "text/html", json!({ "type": "inline" }), "xss.html"),
        ])
    );
    assert_eq!(
        refs["data"][1]["content"][0]["state"]["content"],
        json!([{ "type": "file", "uri": format!("media:{}", hash(PNG)), "mime": "image/png", "name": "shot.png" }])
    );
    // Key order follows `Prompt.MediaFileAttachment`.
    assert!(serde_json::to_string(&refs["data"][0]["files"][0]).unwrap().starts_with("{\"mime\":\"image/png\",\"source\""));
    let single = send(&h, "GET", &format!("/api/session/{sid}/message/msg_media_user"), &[REFS], None).await.json();
    assert_eq!(single["data"]["files"][1]["uri"], json!(format!("media:{}", hash(&text))));
    assert_eq!(h.get(&format!("/api/session/{sid}/message/msg_media_user")).await.json()["data"], user);
    let exported = send(&h, "GET", &format!("/api/experimental/session/{sid}/export"), &[REFS], None).await.json();
    assert_eq!(exported["data"]["messages"], json!([user, assistant]));

    // The route serves raw bytes as downloads (2.2.7), with immutable caching and a 304; image types stay usable by <img>.
    let route = |data: &str| format!("/api/experimental/media/{}", hash(data));
    let r = send(&h, "GET", &route(PNG), &[], None).await;
    assert_eq!(r.status, 200);
    assert_eq!(header(&r, "content-type"), Some("image/png"));
    assert_eq!(header(&r, "content-disposition"), Some("attachment"));
    assert_eq!(header(&r, "content-security-policy"), Some("default-src 'none'; img-src 'self'; media-src 'self'; sandbox"));
    assert_eq!(header(&r, "cache-control"), Some("private, max-age=31536000, immutable"));
    assert_eq!(header(&r, "etag"), Some(format!("\"{}\"", hash(PNG)).as_str()));
    for unsafe_ in [&svg, &html] {
        let r = send(&h, "GET", &route(unsafe_), &[], None).await;
        assert_eq!((header(&r, "content-type"), header(&r, "content-disposition")), (Some("application/octet-stream"), Some("attachment")));
    }
    let r = send(&h, "GET", &route(PNG), &[("if-none-match", &format!("W/\"{}\"", hash(PNG)))], None).await;
    assert_eq!((r.status.as_u16(), r.body.len()), (304, 0));

    // A `media:` ref sent back as an attachment; pending input keeps it a ref.
    let r = send(
        &h,
        "POST",
        &format!("/api/session/{sid}/prompt"),
        &[REFS],
        Some(json!({ "text": "Again", "files": [{ "uri": format!("media:{}", hash(PNG)), "name": "again.png" }], "delivery": "queue", "resume": false })),
    )
    .await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let queued = r.json()["data"]["id"].as_str().unwrap().to_string();
    let pending = send(&h, "GET", &format!("/api/session/{sid}/inbox"), &[REFS], None).await.json();
    assert_eq!(pending["data"][0]["id"], json!(queued));
    assert_eq!(pending["data"][0]["payload"]["files"][0]["uri"], json!(format!("media:{}", hash(PNG))));
    assert_eq!(pending["data"][0]["payload"]["files"][0]["bytes"], json!(size(PNG)));
    assert_eq!(pending["data"][0]["payload"]["files"][0]["name"], "again.png");
    let inline = h.get(&format!("/api/session/{sid}/inbox")).await.json();
    assert_eq!(inline["data"][0]["payload"]["files"][0]["data"], PNG);
    let r = send(&h, "DELETE", &format!("/api/session/{sid}/inbox/{queued}"), &[], None).await;
    assert_eq!(r.status, 204);

    // A ref whose blob is gone, or nothing references, reaches opted-in clients as the placeholder.
    let (gone, orphan) = ("e".repeat(64), "d".repeat(64));
    h.app
        .db
        .with(|c| {
            c.execute("INSERT INTO blob (hash, mime, bytes, data, time_created) VALUES (?, 'image/png', 3, ?, 1)", rusqlite::params![orphan, b"abc".to_vec()])?;
            for (id, seq, h) in [("msg_orphan_media", 1001, &orphan), ("msg_gone_media", 1000, &gone)] {
                let data = json!({ "text": "x", "files": [{ "uri": format!("media:{h}"), "mime": "image/png", "name": "gone.png", "source": { "type": "inline" } }], "time": { "created": seq } });
                c.execute(
                    "INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES (?, ?, 'user', ?, ?, ?, ?)",
                    rusqlite::params![id, sid, seq, seq, seq, data.to_string()],
                )?;
            }
            Ok(())
        })
        .unwrap();
    let placeholder = json!({ "data": b64("[media unavailable: gone.png (image/png)]"), "mime": "text/plain", "source": { "type": "inline" }, "name": "gone.png" });
    for id in ["msg_gone_media", "msg_orphan_media"] {
        let r = send(&h, "GET", &format!("/api/session/{sid}/message/{id}"), &[REFS], None).await.json();
        assert_eq!(r["data"]["files"], json!([placeholder]), "{id}");
    }
    // Inline readers hydrate whatever blob is stored (an unreferenced one too); a gone one is the placeholder.
    let r = h.get(&format!("/api/session/{sid}/message/msg_gone_media")).await.json();
    assert_eq!(r["data"]["files"], json!([placeholder]));
    let r = h.get(&format!("/api/session/{sid}/message/msg_orphan_media")).await.json();
    assert_eq!(r["data"]["files"][0]["data"], b64("abc"));
    assert_eq!(send(&h, "GET", &format!("/api/experimental/media/{orphan}"), &[], None).await.status, 404);
}

#[tokio::test]
async fn follows_media_blob_from_the_sessions_project_config() {
    let h = harness();
    let project = h.root.path().join("project");
    write_config(&project, json!({ "experimental": { "media_blob": true } }));
    let project = project.display().to_string();
    let r = h.post("/api/session", json!({ "location": { "directory": project } })).await;
    let sid = r.json()["data"]["id"].as_str().unwrap().to_string();
    let r = send(
        &h,
        "POST",
        &format!("/api/session/{sid}/prompt"),
        &[],
        Some(json!({ "text": "Look", "files": [{ "uri": format!("data:image/png;base64,{PNG}"), "name": "shot.png" }], "delivery": "queue", "resume": false })),
    )
    .await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let refs = send(&h, "GET", &format!("/api/session/{sid}/inbox"), &[("x-opencode-capabilities", "experimental-media-uri")], None).await.json();
    assert_eq!(refs["data"][0]["payload"]["files"][0]["uri"], json!(format!("media:{}", hash(PNG))));
    assert_eq!(h.get(&format!("/api/session/{sid}/inbox")).await.json()["data"][0]["payload"]["files"][0]["data"], PNG);
    assert_eq!(send(&h, "GET", &format!("/api/experimental/media/{}", hash(PNG)), &[], None).await.status, 200);

    // The server cwd keeps media inline: the capability changes nothing there.
    let other = h.session().await;
    send(&h, "POST", &format!("/api/session/{other}/prompt"), &[], Some(json!({ "text": "x", "files": [{ "uri": format!("data:image/png;base64,{PNG}") }], "resume": false })))
        .await;
    let r = send(&h, "GET", &format!("/api/session/{other}/inbox"), &[("x-opencode-capabilities", "experimental-media-uri")], None).await.json();
    assert_eq!(r["data"][0]["payload"]["files"][0]["data"], PNG);

    // A blob no message or inbox item references is unreadable, by route or as a prompt attachment.
    let text = b64("workspace file contents");
    h.app
        .db
        .with(|c| {
            c.execute(
                "INSERT INTO blob (hash, mime, bytes, data, time_created) VALUES (?, 'text/plain', ?, ?, 1)",
                rusqlite::params![hash(&text), size(&text) as i64, base64::engine::general_purpose::STANDARD.decode(&text).unwrap()],
            )
        })
        .unwrap();
    assert_eq!(send(&h, "GET", &format!("/api/experimental/media/{}", hash(&text)), &[], None).await.status, 404);
    let r = send(&h, "POST", &format!("/api/session/{sid}/prompt"), &[], Some(json!({ "text": "Steal", "files": [{ "uri": format!("media:{}", hash(&text)) }], "resume": false }))).await;
    assert_ne!(r.status, 200);
    assert_eq!(h.get(&format!("/api/session/{sid}/inbox")).await.json()["data"].as_array().unwrap().len(), 1);
}
