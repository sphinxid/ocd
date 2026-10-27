//! Editing saved permissions (`server/test/permission-saved.test.ts`).

mod common;

use common::*;
use serde_json::json;

#[tokio::test]
async fn edits_saved_permissions_through_patch() {
    let h = harness();
    let r = h
        .post(
            "/api/session",
            json!({"permissions": [{"action": "deploy", "resource": "*", "effect": "ask"}]}),
        )
        .await;
    let session = r.json()["data"]["id"].as_str().unwrap().to_string();
    // Saved permissions are only created by answering "always", so save two through the real request flow.
    for resource in ["src/*", "docs/*"] {
        let r = h
            .post(
                &format!("/api/session/{session}/permission"),
                json!({"action": "deploy", "resources": [resource], "save": [resource]}),
            )
            .await;
        let asked = r.json()["data"].clone();
        assert_eq!(asked["effect"], "ask");
        let r = h
            .post(
                &format!(
                    "/api/session/{session}/permission/{}/reply",
                    asked["id"].as_str().unwrap()
                ),
                json!({"decision": "always"}),
            )
            .await;
        assert_eq!(r.status, 204);
    }
    let saved = h.get("/api/permission/saved").await.json();
    let src = saved["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["resource"] == "src/*")
        .unwrap()
        .clone();
    let id = src["id"].as_str().unwrap();

    let r = h
        .req(
            "PATCH",
            &format!("/api/permission/saved/{id}"),
            Some(json!({"resource": "src/allowed.ts"})),
        )
        .await;
    assert_eq!(r.status, 200, "{:?}", String::from_utf8_lossy(&r.body));
    let updated = r.json()["data"].clone();
    assert_eq!(
        (&updated["id"], &updated["action"], &updated["resource"]),
        (&src["id"], &json!("deploy"), &json!("src/allowed.ts"))
    );
    assert!(
        updated["time"]["updated"].as_i64().unwrap() >= src["time"]["updated"].as_i64().unwrap()
    );
    let mut resources: Vec<String> = h.get("/api/permission/saved").await.json()["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["resource"].as_str().unwrap().to_string())
        .collect();
    resources.sort();
    assert_eq!(resources, vec!["docs/*", "src/allowed.ts"]);

    let r = h
        .req(
            "PATCH",
            &format!("/api/permission/saved/{id}"),
            Some(json!({"resource": "docs/*"})),
        )
        .await;
    assert_eq!(r.status, 409);
    assert_eq!(
        r.json(),
        json!({"_tag": "ConflictError", "message": "A saved permission already allows deploy for docs/*", "resource": id})
    );
    let r = h
        .req(
            "PATCH",
            "/api/permission/saved/psv_missing",
            Some(json!({"resource": "x"})),
        )
        .await;
    assert_eq!(r.status, 404);
    assert_eq!(
        r.json(),
        json!({"_tag": "PermissionSavedNotFoundError", "id": "psv_missing", "message": "Saved permission not found: psv_missing"})
    );
}
