mod common;

use common::*;
use serde_json::{Value, json};

#[tokio::test]
async fn location_get_and_debug_locations() {
    let h = harness();
    let r = h.get("/api/location").await;
    assert_eq!(r.status, 200, "{:?}", String::from_utf8_lossy(&r.body));
    let v = r.json();
    assert_eq!(v["directory"], json!(h.dir()));
    assert_eq!(v["project"]["directory"], json!(h.dir()));
    assert_eq!(v["project"]["canonical"], json!(h.dir()));
    assert_eq!(v["project"]["id"].as_str().unwrap(), oc_util::hash::fast(format!("directory:{}", h.dir())));

    let r = h.get("/api/debug/location").await;
    assert_eq!(r.json(), json!([{"directory": h.dir()}]));
    let r = h.req("DELETE", "/api/debug/location", None).await;
    assert_eq!(r.status, 204);
    assert_eq!(h.get("/api/debug/location").await.json(), json!([]));

    // Unknown directory → LocationNotFoundError.
    let r = h.get(&format!("/api/agent?location[directory]={}", enc("/definitely/missing"))).await;
    assert_eq!(r.status, 404);
    assert_eq!(r.json()["_tag"], "LocationNotFoundError");
}

#[tokio::test]
async fn agents() {
    let h = harness();
    let r = h.get("/api/agent").await;
    assert_eq!(r.status, 200);
    let v = r.json();
    assert_eq!(v["location"], json!({"directory": h.dir()}));
    let ids: Vec<&str> = v["data"].as_array().unwrap().iter().map(|a| a["id"].as_str().unwrap()).collect();
    assert_eq!(ids[0], "build");
    assert!(ids.contains(&"plan") && ids.contains(&"explore"));

    let r = h.get("/api/agent/plan").await;
    assert_eq!(r.json()["data"]["name"], "Plan");
    let r = h.get("/api/agent/nope").await;
    assert_eq!(r.status, 404);
    assert_eq!(r.json(), json!({"_tag":"AgentNotFoundError","agentID":"nope","message":"Agent not found: nope"}));
}

#[tokio::test]
async fn session_crud_and_list_paging() {
    let h = harness();
    let mut ids = Vec::new();
    for _ in 0..3 {
        ids.push(h.session().await);
        tokio::time::sleep(std::time::Duration::from_millis(3)).await;
    }
    let r = h.get(&format!("/api/session/{}", ids[0])).await;
    let s = r.json()["data"].clone();
    assert_eq!(s["id"], json!(ids[0]));
    assert_eq!(s["location"], json!({"directory": h.dir()}));
    assert!(s.get("subpath").is_none());

    // Newest first, two per page.
    let r = h.get("/api/session?limit=2").await;
    assert_eq!(r.status, 200);
    let page = r.json();
    let got: Vec<&str> = page["data"].as_array().unwrap().iter().map(|s| s["id"].as_str().unwrap()).collect();
    assert_eq!(got, vec![ids[2].as_str(), ids[1].as_str()]);
    let next = page["cursor"]["next"].as_str().unwrap().to_string();
    let r = h.get(&format!("/api/session?limit=2&cursor={next}")).await;
    let page2 = r.json();
    let got: Vec<&str> = page2["data"].as_array().unwrap().iter().map(|s| s["id"].as_str().unwrap()).collect();
    assert_eq!(got, vec![ids[0].as_str()]);
    let prev = page2["cursor"]["previous"].as_str().unwrap().to_string();
    let r = h.get(&format!("/api/session?limit=2&cursor={prev}")).await;
    let got: Vec<String> = r.json()["data"].as_array().unwrap().iter().map(|s| s["id"].as_str().unwrap().to_string()).collect();
    assert_eq!(got, vec![ids[2].clone(), ids[1].clone()]);
    // Empty page → null cursors.
    let r = h.get(&format!("/api/session?parentID={}", ids[0])).await;
    assert_eq!(r.json(), json!({"data": [], "cursor": {"previous": null, "next": null}}));

    // Ascending order + directory filter.
    let r = h.get(&format!("/api/session?order=asc&directory={}", enc(&h.dir()))).await;
    let got: Vec<String> = r.json()["data"].as_array().unwrap().iter().map(|s| s["id"].as_str().unwrap().to_string()).collect();
    assert_eq!(got.len(), 3);
    assert_eq!(got[0], ids[0]);

    // Update title + metadata.
    let r = h.req("PATCH", &format!("/api/session/{}", ids[1]), Some(json!({"title": "Hello", "metadata": {"a": 1}}))).await;
    assert_eq!(r.status, 204);
    let s = h.get(&format!("/api/session/{}", ids[1])).await.json();
    assert_eq!(s["data"]["title"], "Hello");
    assert_eq!(s["data"]["metadata"], json!({"a": 1}));
    let r = h.get("/api/session?search=hell").await;
    assert_eq!(r.json()["data"].as_array().unwrap().len(), 1);

    // Child session inherits location; removing the parent removes the child.
    let r = h.post("/api/session", json!({"parentID": ids[0], "title": "child"})).await;
    let child = r.json()["data"]["id"].as_str().unwrap().to_string();
    assert_eq!(r.json()["data"]["parentID"], json!(ids[0]));
    let r = h.get("/api/session?parentID=null").await;
    assert_eq!(r.json()["data"].as_array().unwrap().len(), 3);
    let r = h.req("DELETE", &format!("/api/session/{}", ids[0]), None).await;
    assert_eq!(r.status, 204);
    assert_eq!(h.get(&format!("/api/session/{child}")).await.status, 404);
    let r = h.get(&format!("/api/session/{}", ids[0])).await;
    assert_eq!(r.status, 404);
    assert_eq!(r.json()["message"], json!(format!("Session not found: {}", ids[0])));

    // Missing parent → 404.
    let r = h.post("/api/session", json!({"parentID": "ses_missing"})).await;
    assert_eq!(r.status, 404);
    assert_eq!(r.json()["_tag"], "SessionNotFoundError");
}

#[tokio::test]
async fn prompt_inbox_and_messages() {
    let h = harness();
    let id = h.session().await;
    let r = h.post(&format!("/api/session/{id}/prompt"), json!({"text": "hello", "delivery": "queue", "resume": false})).await;
    assert_eq!(r.status, 200, "{:?}", String::from_utf8_lossy(&r.body));
    let item = r.json()["data"].clone();
    assert_eq!(item["type"], "user");
    assert_eq!(item["delivery"], "queue");
    assert_eq!(item["payload"]["text"], "hello");
    let pid = item["id"].as_str().unwrap().to_string();

    // Idempotent re-admission with the same id.
    let again = h.post(&format!("/api/session/{id}/prompt"), json!({"id": pid, "text": "hello"})).await;
    assert_eq!(again.status, 200);
    assert_eq!(again.json()["data"]["id"], json!(pid));

    let list = h.get(&format!("/api/session/{id}/inbox")).await.json();
    assert_eq!(list["data"].as_array().unwrap().len(), 1);

    // Steer it, then queue it back.
    let r = h.req("PATCH", &format!("/api/session/{id}/inbox/{pid}"), Some(json!({"delivery": "steer"}))).await;
    assert_eq!(r.status, 204);
    let r = h.req("PATCH", &format!("/api/session/{id}/inbox/{pid}"), Some(json!({"delivery": "steer"}))).await;
    assert_eq!(r.status, 409);
    assert_eq!(r.json(), json!({"_tag":"ConflictError","message":format!("Pending input cannot change to steer: {pid}"),"resource":pid}));

    // Synthetic + compaction admission.
    let r = h.post(&format!("/api/session/{id}/synthetic"), json!({"text": "note", "description": "d", "resume": false})).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["data"]["type"], "synthetic");
    let r = h.post(&format!("/api/session/{id}/compact"), json!({"delivery": "queue"})).await;
    assert_eq!(r.status, 200);
    let compaction = r.json()["data"].clone();
    assert_eq!(compaction["type"], "compaction");
    assert_eq!(compaction["payload"], json!({}));
    let cid = compaction["id"].as_str().unwrap().to_string();
    let r = h.req("DELETE", &format!("/api/session/{id}/inbox/{cid}"), None).await;
    assert_eq!(r.status, 204);
    // Cancelling again is a no-op.
    let r = h.req("DELETE", &format!("/api/session/{id}/inbox/{cid}"), None).await;
    assert_eq!(r.status, 204);

    // Deliver: messages become visible.
    h.promote(&id).await;
    assert!(h.get(&format!("/api/session/{id}/inbox")).await.json()["data"].as_array().unwrap().is_empty());
    let r = h.get(&format!("/api/session/{id}/message")).await;
    assert_eq!(r.status, 200, "{:?}", String::from_utf8_lossy(&r.body));
    let page = r.json();
    let types: Vec<&str> = page["data"].as_array().unwrap().iter().map(|m| m["type"].as_str().unwrap()).collect();
    assert_eq!(types, vec!["synthetic", "user"]);
    let next = page["cursor"]["next"].as_str().unwrap();
    let r = h.get(&format!("/api/session/{id}/message?cursor={next}")).await;
    assert!(r.json()["data"].as_array().unwrap().is_empty());
    let r = h.get(&format!("/api/session/{id}/message?order=asc&limit=1&type=user")).await;
    assert_eq!(r.json()["data"][0]["id"], json!(pid));
    assert_eq!(r.json()["data"][0]["text"], "hello");

    let r = h.get(&format!("/api/session/{id}/message/{pid}")).await;
    assert_eq!(r.json()["data"]["text"], "hello");
    let r = h.get(&format!("/api/session/{id}/message/msg_missing")).await;
    assert_eq!(r.status, 404);
    assert_eq!(r.json()["_tag"], "MessageNotFoundError");

    let r = h.get(&format!("/api/session/{id}/context")).await;
    assert_eq!(r.json()["data"].as_array().unwrap().len(), 2);

    // A user message id cannot be re-used by a synthetic input.
    let r = h.post(&format!("/api/session/{id}/synthetic"), json!({"id": pid, "text": "x"})).await;
    assert_eq!(r.status, 409);
    assert_eq!(r.json()["message"], json!(format!("Synthetic input ID conflicts with an existing durable record: {pid}")));

    // Fork the whole history.
    let r = h.post(&format!("/api/session/{id}/fork"), json!({})).await;
    assert_eq!(r.status, 200, "{:?}", String::from_utf8_lossy(&r.body));
    let fork = r.json()["data"].clone();
    let fid = fork["id"].as_str().unwrap();
    assert_eq!(fork["fork"]["sessionID"], json!(id));
    let msgs = h.get(&format!("/api/session/{fid}/message")).await.json();
    assert_eq!(msgs["data"].as_array().unwrap().len(), 2);
    let r = h.post(&format!("/api/session/{id}/fork"), json!({"before": "msg_missing"})).await;
    assert_eq!(r.status, 404);
    let empty = h.session().await;
    let r = h.post(&format!("/api/session/{empty}/fork"), json!({})).await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["kind"], "empty_session");
}

#[tokio::test]
async fn prompt_attachments_and_skills() {
    let h = harness();
    let id = h.session().await;
    std::fs::write(h.cwd.join("a.txt"), "line1\nline2\nline3\n").unwrap();
    let uri = format!("file://{}?start=2&end=2", h.cwd.join("a.txt").display());
    let r = h
        .post(
            &format!("/api/session/{id}/prompt"),
            json!({"text": "see", "files": [{"uri": uri}], "skills": [{"id": "opencode"}], "resume": false}),
        )
        .await;
    assert_eq!(r.status, 200, "{:?}", String::from_utf8_lossy(&r.body));
    let payload = r.json()["data"]["payload"].clone();
    assert_eq!(payload["files"][0]["mime"], "text/plain");
    assert_eq!(payload["files"][0]["name"], "a.txt");
    assert_eq!(payload["files"][0]["data"], json!(base64_encode(b"line2")));
    assert_eq!(payload["skills"][0]["name"], "OpenCode");
    assert!(payload["skills"][0]["text"].as_str().unwrap().starts_with("<skill_content name=\"OpenCode\">"));

    let r = h.post(&format!("/api/session/{id}/prompt"), json!({"text": "x", "files": [{"uri": "http://x/y"}]})).await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["field"], "files");
    let r = h.post(&format!("/api/session/{id}/prompt"), json!({"text": "x", "skills": [{"id": "nope"}]})).await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json(), json!({"_tag":"InvalidRequestError","message":"Skill not found: nope","field":"skills"}));
}

fn base64_encode(b: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(b)
}

#[tokio::test]
async fn skills_and_commands() {
    let h = harness();
    let skills = h.get("/api/skill").await.json();
    let ids: Vec<&str> = skills["data"].as_array().unwrap().iter().map(|s| s["id"].as_str().unwrap()).collect();
    assert_eq!(&ids[..2], &["opencode", "report"]);
    let commands = h.get("/api/command").await.json();
    assert_eq!(commands["data"][0], json!({"name": "init", "description": "guided AGENTS.md setup"}));
    assert_eq!(commands["data"][1]["name"], "review");

    let id = h.session().await;
    let r = h.post(&format!("/api/session/{id}/command"), json!({"name": "nope", "text": ""})).await;
    assert_eq!(r.status, 404);
    assert_eq!(r.json(), json!({"_tag":"CommandNotFoundError","command":"nope","message":"Command not found: nope"}));
    let r = h.post(&format!("/api/session/{id}/command"), json!({"name": "init", "text": "extra"})).await;
    assert_eq!(r.status, 204, "{:?}", String::from_utf8_lossy(&r.body));
    let inbox = h.get(&format!("/api/session/{id}/inbox")).await.json();
    let text = inbox["data"][0]["payload"]["text"].as_str().unwrap();
    assert!(text.contains("\nextra\n"), "{text}");
    assert!(!text.contains("$ARGUMENTS"));

    let r = h.post(&format!("/api/experimental/session/{id}/skill"), json!({"id": "opencode", "resume": false})).await;
    assert_eq!(r.status, 204);
    let r = h.post(&format!("/api/experimental/session/{id}/skill"), json!({"id": "nope"})).await;
    assert_eq!(r.status, 404);
    assert_eq!(r.json()["_tag"], "SkillNotFoundError");
}

#[tokio::test]
async fn permissions_flow() {
    let h = harness();
    let id = h.session().await;
    let r = h.post(&format!("/api/session/{id}/permission"), json!({"action": "read", "resources": ["x.env"]})).await;
    assert_eq!(r.status, 200, "{:?}", String::from_utf8_lossy(&r.body));
    let data = r.json()["data"].clone();
    assert_eq!(data["effect"], "ask");
    let rid = data["id"].as_str().unwrap().to_string();
    let r = h.post(&format!("/api/session/{id}/permission"), json!({"action": "read", "resources": ["src/a.rs"]})).await;
    assert_eq!(r.json()["data"]["effect"], "allow");

    let list = h.get(&format!("/api/session/{id}/permission")).await.json();
    assert_eq!(list["data"][0]["id"], json!(rid));
    let loc = h.get("/api/permission/request").await.json();
    assert_eq!(loc["data"].as_array().unwrap().len(), 1);
    let r = h.get(&format!("/api/session/{id}/permission/{rid}")).await;
    assert_eq!(r.json()["data"]["action"], "read");
    let r = h.post(&format!("/api/session/{id}/permission/{rid}/reply"), json!({"decision": "once"})).await;
    assert_eq!(r.status, 204);
    let r = h.get(&format!("/api/session/{id}/permission/{rid}")).await;
    assert_eq!(r.status, 404);
    assert_eq!(r.json()["message"], json!(format!("Permission request not found: {rid}")));
    let r = h.get("/api/permission/saved").await;
    assert_eq!(r.json()["data"], json!([]));
}

#[tokio::test]
async fn filesystem() {
    let h = harness();
    std::fs::create_dir_all(h.cwd.join("src/nested")).unwrap();
    std::fs::write(h.cwd.join("src/main.rs"), "fn main() {}").unwrap();
    std::fs::write(h.cwd.join("README.md"), "# hi").unwrap();
    let r = h.get("/api/fs/list").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["data"], json!([{"path":"src/","type":"directory"},{"path":"README.md","type":"file"}]));
    let r = h.get("/api/fs/list?path=src").await;
    assert_eq!(r.json()["data"], json!([{"path":"src/nested/","type":"directory"},{"path":"src/main.rs","type":"file"}]));
    let r = h.get("/api/fs/list?path=missing").await;
    assert_eq!(r.status, 404);
    assert_eq!(r.json(), json!({"_tag":"FileNotFoundError","path":"missing","message":"Directory not found: missing"}));

    let r = h.get("/api/fs/find?query=main").await;
    assert_eq!(r.json()["data"][0], json!({"path":"src/main.rs","type":"file"}));
    let r = h.get("/api/fs/find?query=src&type=directory").await;
    assert_eq!(r.json()["data"][0], json!({"path":"src/","type":"directory"}));

    let r = h.get("/api/fs/read/src/main.rs").await;
    assert_eq!(r.status, 200);
    assert_eq!(&r.body[..], b"fn main() {}");
    let r = h.get("/api/fs/read/..%2F..%2Fetc%2Fpasswd").await;
    assert_eq!(r.status, 404);

    let r = h
        .router
        .clone();
    let _ = r;
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/api/experimental/fs/write?path=out%2Fnew.txt")
        .header("content-type", "application/octet-stream")
        .body(axum::body::Body::from("data"))
        .unwrap();
    use tower::ServiceExt;
    let res = h.router.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(std::fs::read_to_string(h.cwd.join("out/new.txt")).unwrap(), "data");
}

#[tokio::test]
async fn projects_config_and_credentials() {
    let h = harness();
    h.get("/api/location").await;
    let r = h.get("/api/project").await;
    let projects = r.json();
    let pid = projects[0]["id"].as_str().unwrap().to_string();
    let r = h.req("PATCH", &format!("/api/project/{pid}"), Some(json!({"name": "Demo", "commands": {"start": "make"}}))).await;
    assert_eq!(r.status, 200, "{:?}", String::from_utf8_lossy(&r.body));
    assert_eq!(r.json()["name"], "Demo");
    assert_eq!(r.json()["commands"], json!({"start": "make"}));
    let r = h.req("PATCH", "/api/project/missing", Some(json!({"name": "x"}))).await;
    assert_eq!(r.status, 404);
    assert_eq!(r.json()["_tag"], "ProjectNotFoundError");

    let r = h.get("/api/worktree?projectID=missing").await;
    assert_eq!(r.status, 404);
    let r = h.get(&format!("/api/worktree?projectID={pid}")).await;
    assert_eq!(r.json(), json!([]));

    let r = h.get("/api/config").await;
    assert_eq!(r.status, 200);
    assert!(r.json().is_array());

    let r = h.post("/api/credential", json!({"integrationID": "anthropic", "value": {"type": "key", "key": "sk-1"}})).await;
    assert_eq!(r.status, 200, "{:?}", String::from_utf8_lossy(&r.body));
    let c1 = r.json()["data"].clone();
    assert_eq!(c1["active"], true);
    assert_eq!(c1["label"], "default");
    let r = h
        .post("/api/credential", json!({"integrationID": "anthropic", "label": "two", "value": {"type": "key", "key": "sk-2"}, "activate": false}))
        .await;
    assert_eq!(r.json()["data"]["active"], false);
    let c2 = r.json()["data"]["id"].as_str().unwrap().to_string();
    let r = h.post("/api/credential", json!({"id": c2, "integrationID": "x", "value": {"type": "key", "key": "k"}})).await;
    assert_eq!(r.status, 409);
    let r = h.post(&format!("/api/credential/{c2}/activate"), json!({})).await;
    assert_eq!(r.status, 204);
    let list = h.get("/api/credential").await.json();
    let active: Vec<(String, bool)> =
        list["data"].as_array().unwrap().iter().map(|c| (c["id"].as_str().unwrap().to_string(), c["active"].as_bool().unwrap())).collect();
    assert!(active.contains(&(c2.clone(), true)));
    let r = h.req("DELETE", &format!("/api/credential/{c2}"), None).await;
    assert_eq!(r.status, 204);
    let list = h.get("/api/credential").await.json();
    assert_eq!(list["data"].as_array().unwrap().len(), 1);
    assert_eq!(list["data"][0]["active"], true);

    let r = h.get("/api/experimental/migration/v1").await;
    assert_eq!(r.json(), json!({"status": "completed"}));
}

#[tokio::test]
async fn instruction_entries_and_log() {
    let h = harness();
    let id = h.session().await;
    let base = format!("/api/experimental/session/{id}/instructions/entries");
    let r = h.req("PUT", &format!("{base}/b.key"), Some(json!({"value": {"x": 1}}))).await;
    assert_eq!(r.status, 204);
    h.req("PUT", &format!("{base}/a"), Some(json!({"value": "text"}))).await;
    let r = h.get(&base).await;
    assert_eq!(r.json()["data"], json!([{"key":"a","value":"text"},{"key":"b.key","value":{"x":1}}]));
    h.req("DELETE", &format!("{base}/a"), None).await;
    assert_eq!(h.get(&base).await.json()["data"].as_array().unwrap().len(), 1);
    let big = "x".repeat(300 * 1024);
    let r = h.req("PUT", &format!("{base}/big"), Some(json!({"value": big}))).await;
    assert_eq!(r.status, 413);
    assert_eq!(r.json()["_tag"], "InstructionEntryValueTooLargeError");

    let r = h.get(&format!("/api/experimental/session/{id}/log")).await;
    assert_eq!(r.status, 200);
    let text = String::from_utf8_lossy(&r.body).to_string();
    assert!(text.contains("\"type\":\"log.synced\""), "{text}");
    assert!(text.contains(&format!("\"aggregateID\":\"{id}\"")));
}

#[tokio::test]
async fn interrupt_active_wait_view_without_runner() {
    let h = harness();
    let id = h.session().await;
    let r = h.post(&format!("/api/session/{id}/interrupt"), json!({})).await;
    assert_eq!(r.json(), json!({"interrupted": false}));
    let r = h.get("/api/session/active").await;
    assert_eq!(r.json(), json!({"data": {}}));
    let r = h.post(&format!("/api/experimental/session/{id}/wait"), json!({})).await;
    assert_eq!(r.status, 204);
    let r = h.post(&format!("/api/session/{id}/view"), json!({"idle": 1})).await;
    assert_eq!(r.status, 204);
    let r = h.req("PUT", &format!("/api/session/{id}/environment"), Some(json!({"variables": {"A": "1"}}))).await;
    assert_eq!(r.status, 204);
    let r = h.post(&format!("/api/session/{id}/agent"), json!({"agent": "plan"})).await;
    assert_eq!(r.status, 204);
    let r = h.post(&format!("/api/session/{id}/model"), json!({"model": {"id": "m", "providerID": "p"}})).await;
    assert_eq!(r.status, 204);
    let s = h.get(&format!("/api/session/{id}")).await.json()["data"].clone();
    assert_eq!(s["agent"], "plan");
    assert_eq!(s["model"]["providerID"], "p");
    let r = h.post(&format!("/api/session/{id}/revert/stage"), json!({"messageID": "msg_missing"})).await;
    assert_eq!(r.status, 404, "{:?}", String::from_utf8_lossy(&r.body));
    assert_eq!(r.json()["_tag"], "MessageNotFoundError");
    let r = h.req("DELETE", &format!("/api/session/{id}/revert"), None).await;
    assert_eq!(r.status, 204);
    let r = h.post(&format!("/api/session/{id}/revert/commit"), json!({})).await;
    assert_eq!(r.status, 204);
}

#[tokio::test]
async fn session_move() {
    let h = harness();
    let id = h.session().await;
    let other = h.cwd.parent().unwrap().join("other");
    std::fs::create_dir_all(&other).unwrap();
    let r = h.post(&format!("/api/session/{id}/move"), json!({"directory": "/definitely/missing"})).await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["message"], "Directory does not exist: /definitely/missing");
    let r = h.post(&format!("/api/session/{id}/move"), json!({"directory": other.display().to_string()})).await;
    assert_eq!(r.status, 204, "{:?}", String::from_utf8_lossy(&r.body));
    let inbox = h.get(&format!("/api/session/{id}/inbox")).await.json();
    assert_eq!(inbox["data"][0]["type"], "move");
    assert_eq!(inbox["data"][0]["payload"]["location"]["directory"], json!(other.display().to_string()));
}

#[tokio::test]
async fn vcs_on_git_repository() {
    if !git_available() {
        return;
    }
    let h = harness();
    git(&h.cwd, &["init", "-q"]);
    std::fs::write(h.cwd.join("a.txt"), "one\n").unwrap();
    git(&h.cwd, &["add", "."]);
    git(&h.cwd, &["commit", "-qm", "init"]);
    std::fs::write(h.cwd.join("a.txt"), "two\n").unwrap();
    let r = h.get("/api/vcs").await;
    assert_eq!(r.status, 200, "{:?}", String::from_utf8_lossy(&r.body));
    assert_eq!(r.json()["data"]["provider"], "git");
    assert_eq!(r.json()["data"]["branch"]["current"], "main");
    let r = h.get("/api/vcs/status").await;
    assert_eq!(r.json()["data"][0]["file"], "a.txt");
    let r = h.get("/api/vcs/diff?mode=working").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["data"][0]["file"], "a.txt");
    let r = h.get("/api/vcs/branch").await;
    assert_eq!(r.json()["data"], json!(["main"]));
    let r = h.post("/api/vcs/init", json!({})).await;
    assert_eq!(r.status, 409);
}

#[tokio::test]
async fn vcs_init_on_plain_directory() {
    if !git_available() {
        return;
    }
    let h = harness();
    let r = h.get("/api/vcs").await;
    assert_eq!(r.json()["data"], json!({"branch": {}}));
    let r = h.post("/api/vcs/init", json!({})).await;
    assert_eq!(r.status, 204, "{:?}", String::from_utf8_lossy(&r.body));
    assert!(h.cwd.join(".git").exists());
    let r = h.post("/api/vcs/init?provider=svn", json!({})).await;
    assert!(r.status == 400 || r.status == 409);
}

#[tokio::test]
async fn event_feed_sees_session_events() {
    use oc_server::{Backend, EventSubscription, RequestContext};
    let h = harness();
    let engine = oc_backend::Engine::new(h.app.clone());
    let ctx = RequestContext::for_test(&h.dir());
    let sub = engine.event_subscribe(&ctx, Default::default()).await.unwrap();
    let EventSubscription::Stream(mut stream) = sub else { panic!("stream expected") };
    let id = h.session().await;
    use futures::StreamExt;
    let mut types = Vec::new();
    while let Ok(Some(ev)) = tokio::time::timeout(std::time::Duration::from_millis(500), stream.next()).await {
        types.push(ev["type"].as_str().unwrap_or_default().to_string());
        if ev["type"] == "session.created" {
            assert_eq!(ev["data"]["sessionID"], json!(id));
            assert_eq!(ev["durable"]["seq"], json!(0));
            break;
        }
    }
    assert!(types.contains(&"session.created".to_string()), "{types:?}");
    let _: Value = json!(null);
}

#[tokio::test]
async fn models_providers_and_integrations() {
    let h = harness();
    let r = h.get("/api/integration").await;
    assert_eq!(r.status, 200, "{:?}", String::from_utf8_lossy(&r.body));
    let ids: Vec<String> = r.json()["data"].as_array().unwrap().iter().map(|i| i["id"].as_str().unwrap().to_string()).collect();
    assert!(ids.contains(&"anthropic".to_string()), "{ids:?}");
    let r = h.get("/api/integration/nope").await;
    assert_eq!(r.status, 404);
    assert_eq!(r.json(), json!({"_tag":"IntegrationNotFoundError","integrationID":"nope","message":"Integration not found: nope"}));
    let r = h.get("/api/integration/anthropic/connect/oauth/missing").await;
    assert_eq!(r.status, 404);
    assert_eq!(r.json()["message"], "OAuth attempt not found: missing");

    // Connecting a key makes the provider and its models available.
    let r = h.post("/api/integration/anthropic/connect/key", json!({"key": "sk-test"})).await;
    assert_eq!(r.status, 204, "{:?}", String::from_utf8_lossy(&r.body));
    let creds = h.get("/api/credential").await.json();
    assert_eq!(creds["data"][0]["integrationID"], "anthropic");
    assert_eq!(creds["data"][0]["active"], true);

    let r = h.get("/api/provider").await;
    assert_eq!(r.status, 200, "{:?}", String::from_utf8_lossy(&r.body));
    let providers: Vec<String> = r.json()["data"].as_array().unwrap().iter().map(|p| p["id"].as_str().unwrap().to_string()).collect();
    assert!(providers.contains(&"anthropic".to_string()), "{providers:?}");
    let r = h.get("/api/provider/anthropic").await;
    assert_eq!(r.json()["data"]["id"], "anthropic");
    let r = h.get("/api/provider/nope").await;
    assert_eq!(r.status, 404);
    assert_eq!(r.json()["message"], "Provider not found: nope");

    let r = h.get("/api/model").await;
    assert_eq!(r.status, 200, "{:?}", String::from_utf8_lossy(&r.body));
    assert!(r.json()["data"].as_array().unwrap().iter().any(|m| m["providerID"] == "anthropic"));
    let r = h.get("/api/model/default").await;
    assert_eq!(r.status, 200);
    assert!(r.json()["data"].is_object());
}

#[tokio::test]
async fn forms_flow() {
    let h = harness();
    let id = h.session().await;
    let other = h.session().await;
    let base = format!("/api/session/{id}/form");
    let field = json!({"key": "name", "type": "string", "label": "Name", "required": true});
    let r = h.post(&base, json!({"id": "frm_1", "title": "Who?", "fields": [field]})).await;
    assert_eq!(r.status, 200, "{:?}", String::from_utf8_lossy(&r.body));
    assert_eq!(r.json()["data"]["sessionID"], json!(id));
    let r = h.post(&base, json!({"id": "frm_1", "title": "Again", "fields": [field]})).await;
    assert_eq!(r.status, 409);
    assert_eq!(r.json(), json!({"_tag":"ConflictError","message":"Form already exists: frm_1","resource":"frm_1"}));
    let r = h.post(&base, json!({"title": "Empty", "fields": []})).await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["field"], "fields");

    assert_eq!(h.get(&base).await.json()["data"].as_array().unwrap().len(), 1);
    assert_eq!(h.get("/api/form").await.json()["data"].as_array().unwrap().len(), 1);
    let r = h.get(&format!("{base}/frm_1")).await;
    assert_eq!(r.status, 200, "{:?}", String::from_utf8_lossy(&r.body));
    assert_eq!(r.json()["data"]["state"]["status"], "pending");
    // Owned by another session → 404.
    let r = h.get(&format!("/api/session/{other}/form/frm_1")).await;
    assert_eq!(r.status, 404);
    assert_eq!(r.json(), json!({"_tag":"FormNotFoundError","id":"frm_1","message":"Form not found: frm_1"}));

    let r = h.post(&format!("{base}/frm_1/reply"), json!({"answer": {"name": ""}})).await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["_tag"], "FormInvalidAnswerError");
    // A form someone waits on (the question tool's `ask`), answered over HTTP.
    let forms = h.app.clone();
    let sid = id.clone();
    let field2 = field.clone();
    let asked = tokio::spawn(async move {
        forms.forms.ask(&json!({"id": "frm_2", "sessionID": sid, "title": "Q", "fields": [field2]})).await.unwrap()
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let r = h.post(&format!("{base}/frm_2/reply"), json!({"answer": {"name": "Ada"}})).await;
    assert_eq!(r.status, 204, "{:?}", String::from_utf8_lossy(&r.body));
    assert_eq!(asked.await.unwrap().to_json()["answer"], json!({"name": "Ada"}));
    let r = h.get(&format!("{base}/frm_2")).await;
    assert_eq!(r.json()["data"]["state"]["status"], "answered");
    let r = h.req("DELETE", &format!("{base}/frm_2"), None).await;
    assert_eq!(r.status, 409);
    assert_eq!(r.json(), json!({"_tag":"FormAlreadySettledError","id":"frm_2","message":"Form already settled: frm_2"}));
    let r = h.req("DELETE", &format!("{base}/frm_1?message=later"), None).await;
    assert_eq!(r.status, 204);

    // `global` forms live at the request location.
    let r = h.post("/api/session/global/form", json!({"title": "MCP", "fields": [field]})).await;
    assert_eq!(r.status, 200, "{:?}", String::from_utf8_lossy(&r.body));
    assert!(h.get("/api/form").await.json()["data"].as_array().unwrap().iter().any(|f| f["sessionID"] == "global"));

    // Background with no jobs is a no-op.
    let r = h.post(&format!("/api/session/{id}/background"), json!({})).await;
    assert_eq!(r.status, 204);
}

#[tokio::test]
async fn transfer_generate_and_mcp_without_runner() {
    let h = harness();
    let id = h.session().await;
    h.post(&format!("/api/session/{id}/prompt"), json!({"text": "hello", "resume": false})).await;
    h.promote(&id).await;
    let r = h.get(&format!("/api/experimental/session/{id}/export")).await;
    assert_eq!(r.status, 200, "{:?}", String::from_utf8_lossy(&r.body));
    let data = r.json()["data"].clone();
    assert_eq!(data["info"]["id"], json!(id));
    assert_eq!(data["messages"][0]["text"], "hello");
    let r = h.get("/api/experimental/session/ses_missing/export").await;
    assert_eq!(r.status, 404);

    let r = h.post("/api/experimental/session/import", json!({"info": data["info"], "messages": data["messages"]})).await;
    assert_eq!(r.status, 409);
    assert_eq!(r.json(), json!({"_tag":"ConflictError","message":format!("Session already exists: {id}"),"resource":id}));
    // Re-import after deleting the original.
    assert_eq!(h.req("DELETE", &format!("/api/session/{id}"), None).await.status, 204);
    let r = h.post("/api/experimental/session/import", json!({"info": data["info"], "messages": data["messages"]})).await;
    assert_eq!(r.status, 200, "{:?}", String::from_utf8_lossy(&r.body));
    assert_eq!(r.json()["data"]["id"], json!(id));
    let msgs = h.get(&format!("/api/session/{id}/message")).await.json();
    assert_eq!(msgs["data"].as_array().unwrap().len(), 1);

    let r = h.post(&format!("/api/session/{id}/generate"), json!({"prompt": "x"})).await;
    assert_eq!(r.status, 503);
    assert_eq!(r.json()["service"], "session generation");
    let r = h.post("/api/experimental/generate", json!({"prompt": "x"})).await;
    assert_eq!(r.status, 503);
    assert_eq!(h.get("/api/mcp").await.json()["data"], json!([]));
    let r = h.post("/api/experimental/mcp/x/connect", json!({})).await;
    assert_eq!(r.status, 404);
    assert_eq!(r.json(), json!({"_tag":"McpServerNotFoundError","server":"x","message":"MCP server not found: x"}));
}

#[tokio::test]
async fn pty_lifecycle_and_attach() {
    use futures::StreamExt;
    use oc_server::{Backend, RequestContext};
    let h = harness();
    let r = h.post("/api/pty", json!({"command": "/bin/sh", "title": "t"})).await;
    assert_eq!(r.status, 200, "{:?}", String::from_utf8_lossy(&r.body));
    let pty = r.json()["data"].clone();
    let id = pty["id"].as_str().unwrap().to_string();
    assert_eq!(pty["status"], "running");
    assert_eq!(pty["cwd"], json!(h.dir()));
    assert_eq!(h.get("/api/pty").await.json()["data"].as_array().unwrap().len(), 1);
    let r = h.req("PUT", &format!("/api/pty/{id}"), Some(json!({"title": "renamed", "size": {"rows": 30, "cols": 100}}))).await;
    assert_eq!(r.json()["data"]["title"], "renamed");

    // Another location does not see it.
    let other = h.cwd.parent().unwrap().join("elsewhere");
    std::fs::create_dir_all(&other).unwrap();
    let r = h.get(&format!("/api/pty/{id}?location[directory]={}", enc(&other.display().to_string()))).await;
    assert_eq!(r.status, 404);
    assert_eq!(r.json()["message"], json!(format!("PTY session not found: {id}")));

    // Attach through the backend (oc-server does the WebSocket framing).
    let engine = oc_backend::Engine::new(h.app.clone());
    engine.pty_owner.lock().insert(id.clone(), h.dir());
    let ctx = RequestContext::for_test(&h.dir());
    let att = engine.pty_connect(&ctx, oc_server::PtyAttachRequest { pty_id: id.clone(), cursor: None }).await.unwrap();
    att.control.write("echo marker-$((40+2))\n");
    let mut out = att.replay.clone();
    let mut stream = att.output;
    while !out.contains("marker-42") {
        match tokio::time::timeout(std::time::Duration::from_secs(5), stream.next()).await {
            Ok(Some(chunk)) => out.push_str(&chunk),
            _ => break,
        }
    }
    assert!(out.contains("marker-42"), "{out:?}");
    att.control.detach();

    let r = h.req("DELETE", &format!("/api/pty/{id}"), None).await;
    assert_eq!(r.status, 204);
    assert_eq!(h.get(&format!("/api/pty/{id}")).await.status, 404);
}

#[tokio::test]
async fn session_stats() {
    let h = harness();
    let id = h.session().await;
    h.post(&format!("/api/session/{id}/prompt"), json!({"text": "hello", "resume": false})).await;
    h.promote(&id).await;
    let now = oc_schema::id::now_ms() as i64;
    let data = json!({
        "time": {"created": now},
        "model": {"providerID": "anthropic", "id": "claude", "variant": "high"},
        "tokens": {"input": 10, "output": 5, "reasoning": 1, "cache": {"read": 2, "write": 0}},
        "cost": 0.25,
        "content": [
            {"type": "tool", "name": "read", "state": {"status": "completed"}, "time": {"created": now, "completed": now + 40}},
            {"type": "tool", "name": "read", "state": {"status": "error"}},
            {"type": "text", "text": "hi"}
        ]
    });
    h.app
        .db
        .with(|c| {
            c.execute(
                "INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES (?, ?, 'assistant', 1000, ?3, ?3, ?4)",
                rusqlite::params![oc_schema::id::message(), id, now, data.to_string()],
            )
        })
        .unwrap();
    let r = h.get(&format!("/api/experimental/session/stats?to={}&tools=detail", now + 1)).await;
    assert_eq!(r.status, 200, "{:?}", String::from_utf8_lossy(&r.body));
    let s = r.json()["data"].clone();
    assert_eq!(s["sessions"], 1);
    assert_eq!(s["prompts"], 1);
    assert_eq!(s["steps"], 1);
    assert_eq!(s["tokens"]["input"], 10);
    assert_eq!(s["cost"], 0.25);
    assert_eq!(s["activeDays"], 1);
    assert_eq!(s["streak"], 1);
    assert_eq!(s["models"][0]["model"], json!({"id": "claude", "providerID": "anthropic", "variant": "high"}));
    assert_eq!(s["tools"]["totals"], json!({"calls": 2, "succeeded": 1, "failed": 1, "unfinished": 0}));
    assert_eq!(s["tools"]["usage"][0]["name"], "read");
    assert_eq!(s["tools"]["usage"][0]["durationP50"], 40);
    let r = h.get("/api/experimental/session/stats?tools=summary&timezone=Europe%2FBerlin").await;
    assert_eq!(r.json()["data"]["tools"], json!({"mode": "summary", "totals": {"calls": 2, "succeeded": 1, "failed": 1, "unfinished": 0}}));
    let r = h.get("/api/experimental/session/stats?timezone=Mars%2FBase").await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["message"], "Invalid time zone: Mars/Base");
    let r = h.get("/api/experimental/session/stats?from=10&to=5").await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["message"], "Stats range must end after it starts");
}

#[tokio::test]
async fn shells_websearch_references() {
    let h = harness();
    let r = h.get("/api/config/shell").await;
    assert_eq!(r.status, 200, "{:?}", String::from_utf8_lossy(&r.body));
    for s in r.json().as_array().unwrap() {
        assert!(std::path::Path::new(s["path"].as_str().unwrap()).is_file());
    }
    let r = h.get("/api/websearch/provider").await;
    assert_eq!(r.status, 200, "{:?}", String::from_utf8_lossy(&r.body));
    let ids: Vec<String> = r.json()["data"].as_array().unwrap().iter().map(|p| p["id"].as_str().unwrap().to_string()).collect();
    assert_eq!(ids.len(), 5, "{ids:?}");
    let r = h.post("/api/websearch", json!({"query": "x", "providerID": "nope"})).await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["kind"], "websearch_provider_not_found");
    let r = h.get("/api/reference").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["data"], json!([]));
    let id = h.session().await;
    let r = h.req("PUT", &format!("/api/session/{id}/environment"), Some(json!({"variables": {"A": "1"}}))).await;
    assert_eq!(r.status, 204);
    assert_eq!(h.app.environments.lock().get(&id).unwrap().get("A").map(String::as_str), Some("1"));
}

#[tokio::test]
async fn worktrees() {
    if !git_available() {
        return;
    }
    let h = harness();
    git(&h.cwd, &["init", "-q"]);
    std::fs::write(h.cwd.join("a.txt"), "one\n").unwrap();
    git(&h.cwd, &["add", "."]);
    git(&h.cwd, &["commit", "-qm", "init"]);
    let loc = h.get("/api/location").await.json();
    let pid = loc["project"]["id"].as_str().unwrap().to_string();
    let r = h.post("/api/worktree", json!({"projectID": "missing"})).await;
    assert_eq!(r.status, 404);
    let target = h.cwd.parent().unwrap().join("wt-one");
    let r = h.post("/api/worktree", json!({"projectID": pid, "directory": target.display().to_string(), "name": "one"})).await;
    assert_eq!(r.status, 200, "{:?}", String::from_utf8_lossy(&r.body));
    let dir = r.json()["directory"].as_str().unwrap().to_string();
    assert!(std::path::Path::new(&dir).join("a.txt").exists());
    let list = h.get(&format!("/api/worktree?projectID={pid}")).await.json();
    assert!(list.as_array().unwrap().iter().any(|w| w["directory"] == json!(dir)), "{list}");
    let r = h.req("DELETE", "/api/worktree", Some(json!({"projectID": pid, "directory": "/not/a/worktree", "force": false}))).await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["name"], "WorktreeError");
    let r = h.post("/api/worktree/refresh", json!({"projectID": pid})).await;
    assert_eq!(r.status, 204, "{:?}", String::from_utf8_lossy(&r.body));
    let r = h.post("/api/worktree/refresh", json!({"projectID": "missing"})).await;
    assert_eq!(r.status, 404);
    assert_eq!(r.json()["_tag"], "ProjectNotFoundError");
    let r = h.req("DELETE", "/api/worktree", Some(json!({"projectID": pid, "directory": dir, "force": true}))).await;
    assert_eq!(r.status, 204, "{:?}", String::from_utf8_lossy(&r.body));
}

#[tokio::test]
async fn persistent_pty_without_daemon() {
    let h = harness();
    let r = h.get("/api/info").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["capabilities"]["persistentPty"], json!(cfg!(unix)));
    let id = h.session().await;
    let r = h.get(&format!("/api/experimental/session/{id}/terminal")).await;
    assert_eq!(r.status, 200, "{:?}", String::from_utf8_lossy(&r.body));
    assert_eq!(r.json(), json!({"data": []}));
    let r = h.get(&format!("/api/experimental/session/{id}/terminal/read")).await;
    assert_eq!(r.json(), json!({"data": null}));
    let r = h.get("/api/experimental/persistent-pty/pty_missing").await;
    assert!(r.status == 404 || r.status == 503, "{}", r.status);
    let tag = r.json()["_tag"].as_str().unwrap().to_string();
    assert!(tag == "PtyNotFoundError" || tag == "ServiceUnavailableError");
}

/// Needs the real `opencode-pty` daemon (`OPENCODE_PTY_BIN` or a download):
/// `cargo test -p oc-backend --test api -- --ignored persistent_pty_real_daemon`.
#[tokio::test]
#[ignore]
async fn persistent_pty_real_daemon() {
    use futures::StreamExt;
    use oc_server::{Backend, PersistentPtyAttachRequest, PersistentPtyEvent, PtyRole, RequestContext};
    let h = harness();
    let id = h.session().await;
    let r = h
        .post(
            &format!("/api/experimental/session/{id}/terminal"),
            json!({"command": "/bin/sh", "args": [], "title": "t", "env": {}, "size": {"cols": 80, "rows": 24}}),
        )
        .await;
    assert_eq!(r.status, 200, "{:?}", String::from_utf8_lossy(&r.body));
    let pty = r.json()["data"]["id"].as_str().unwrap().to_string();
    let engine = h.engine.clone();
    let ctx = RequestContext::for_test(&h.dir());
    let att = engine
        .persistent_pty_connect(
            &ctx,
            PersistentPtyAttachRequest { pty_id: pty.clone(), cursor: 0, attachment_id: "a1".into(), role: PtyRole::Controller, takeover: true },
        )
        .await
        .unwrap();
    att.control.input(80, 24, bytes::Bytes::from_static(b"echo marker-$((40+2))\n")).await;
    let mut out = String::from_utf8_lossy(&att.replay.data).to_string();
    let mut events = att.events;
    while !out.contains("marker-42") {
        match tokio::time::timeout(std::time::Duration::from_secs(10), events.next()).await {
            Ok(Some(PersistentPtyEvent::Output(b))) => out.push_str(&String::from_utf8_lossy(&b)),
            Ok(Some(_)) => {}
            _ => break,
        }
    }
    assert!(out.contains("marker-42"), "{out:?}");
    att.control.detach();
    let r = h.req("DELETE", &format!("/api/experimental/persistent-pty/{pty}"), None).await;
    assert_eq!(r.status, 204);
    engine.persistent().await.shutdown().await.unwrap();
}

#[tokio::test]
async fn shell_commands() {
    let h = harness();
    let mut events = h.app.bus.subscribe();
    let r = h.post("/api/shell", json!({"command": "echo shell-$((20+1))", "metadata": {"source": "test"}})).await;
    assert_eq!(r.status, 200, "{:?}", String::from_utf8_lossy(&r.body));
    let info = r.json()["data"].clone();
    let id = info["id"].as_str().unwrap().to_string();
    assert_eq!(info["cwd"], json!(h.dir()));
    let created = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let e = events.recv().await.unwrap();
            if e.kind == "shell.created" {
                return e;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(created.data["info"]["id"], json!(id));
    // Wait for exit, then page the output.
    let mut status = String::new();
    for _ in 0..100 {
        status = h.get(&format!("/api/shell/{id}")).await.json()["data"]["status"].as_str().unwrap_or_default().to_string();
        if status != "running" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_ne!(status, "running");
    let r = h.get(&format!("/api/shell/{id}/output?cursor=0&limit=100")).await;
    assert_eq!(r.status, 200, "{:?}", String::from_utf8_lossy(&r.body));
    assert!(r.json()["data"]["output"].as_str().unwrap().contains("shell-21"), "{}", r.json());
    let r = h.get(&format!("/api/shell/{id}/output?cursor=3&limit=2")).await;
    assert_eq!(r.json()["data"]["cursor"], 5);
    // Exited commands are not listed.
    assert!(h.get("/api/shell").await.json()["data"].as_array().unwrap().iter().all(|s| s["id"] != json!(id)));
    // Another location does not see it.
    let other = h.cwd.parent().unwrap().join("elsewhere-shell");
    std::fs::create_dir_all(&other).unwrap();
    let r = h.get(&format!("/api/shell/{id}?location[directory]={}", enc(&other.display().to_string()))).await;
    assert_eq!(r.status, 404);
    assert_eq!(r.json(), json!({"_tag":"ShellNotFoundError","id":id,"message":format!("Shell command not found: {id}")}));
    assert_eq!(h.req("DELETE", &format!("/api/shell/{id}"), None).await.status, 204);
    assert_eq!(h.get(&format!("/api/shell/{id}")).await.status, 404);
    assert_eq!(h.req("DELETE", &format!("/api/shell/{}", oc_schema::id::shell()), None).await.status, 204);
}

#[tokio::test]
async fn config_update_and_reload_eviction() {
    let h = harness();
    h.get("/api/location").await;
    assert_eq!(h.get("/api/debug/location").await.json().as_array().unwrap().len(), 1);
    // A core reload announcing `config.updated` drops the cached location services.
    h.app.bus.publish(
        "config.updated",
        json!({}),
        Some(oc_schema::types::LocationRef { directory: h.dir(), workspace_id: None }),
    );
    let mut empty = false;
    for _ in 0..50 {
        if h.get("/api/debug/location").await.json().as_array().unwrap().is_empty() {
            empty = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(empty);
    // Patching the global config writes the shell and reloads loaded locations.
    h.get("/api/location").await;
    let r = h.req("PATCH", "/api/experimental/config", Some(json!({"shell": "/bin/sh"}))).await;
    assert_eq!(r.status, 204, "{:?}", String::from_utf8_lossy(&r.body));
    let config = h.app.global.config.clone();
    let written = ["opencode.jsonc", "opencode.json"].iter().filter_map(|n| std::fs::read_to_string(config.join(n)).ok()).next().unwrap();
    assert!(written.contains("/bin/sh"), "{written}");
    let entries = h.get("/api/config").await.json();
    assert!(entries.to_string().contains("/bin/sh"), "{entries}");
}

#[tokio::test]
async fn wellknown_add() {
    let h = harness();
    // A tiny origin serving `/.well-known/opencode`.
    let app = axum::Router::new().route(
        "/.well-known/opencode",
        axum::routing::get(|| async { axum::Json(json!({"auth": {"command": ["echo", "token"], "env": "WK_TOKEN"}})) }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let r = h.post("/api/experimental/integration/wellknown", json!({"url": format!("{origin}/")})).await;
    assert_eq!(r.status, 204, "{:?}", String::from_utf8_lossy(&r.body));
    let r = h.get(&format!("/api/integration/{}", enc(&origin))).await;
    assert_eq!(r.status, 200, "{:?}", String::from_utf8_lossy(&r.body));
    assert!(r.json()["data"]["methods"].to_string().contains("login"), "{}", r.json());

    let r = h.post("/api/experimental/integration/wellknown", json!({"url": "http://127.0.0.1:1"})).await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["kind"], "well_known_discovery");
    assert_eq!(r.json()["message"], "Failed to load wellknown manifest from http://127.0.0.1:1/.well-known/opencode");
}

#[tokio::test]
async fn prompt_images_are_normalized() {
    use base64::Engine;
    let h = harness();
    // `media.image` limits from the location config.
    std::fs::write(h.cwd.join("opencode.json"), r#"{"media": {"image": {"max_width": 50, "max_height": 50}}}"#).unwrap();
    let id = h.session().await;
    let img = image::RgbImage::from_fn(200, 100, |x, y| image::Rgb([(x % 256) as u8, (y % 256) as u8, 128]));
    let mut png = Vec::new();
    image::DynamicImage::ImageRgb8(img).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
    let uri = format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(&png));
    let r = h.post(&format!("/api/session/{id}/prompt"), json!({"text": "img", "files": [{"uri": uri}], "resume": false})).await;
    assert_eq!(r.status, 200, "{:?}", String::from_utf8_lossy(&r.body));
    let file = r.json()["data"]["payload"]["files"][0].clone();
    let data = base64::engine::general_purpose::STANDARD.decode(file["data"].as_str().unwrap()).unwrap();
    let decoded = image::load_from_memory(&data).unwrap();
    assert!(decoded.width() <= 50 && decoded.height() <= 50, "{}x{}", decoded.width(), decoded.height());

    // A PNG signature with an undecodable body is rejected.
    let mut bad = vec![0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
    bad.extend_from_slice(b"not really a png");
    let uri = format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(&bad));
    let r = h.post(&format!("/api/session/{id}/prompt"), json!({"text": "img", "files": [{"uri": uri}], "resume": false})).await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json(), json!({"_tag":"InvalidRequestError","message":"Image could not be decoded: inline attachment","field":"files"}));
}
