//! What a credential change announces before the operation goes on (upstream's bus resumes type subscribers
//! inline): at every loaded location, in load order, the Provider State when the available providers changed
//! (`provider.updated`, then `model.updated`), and the policy plugin when the account behind an integration's
//! active credential changed (`provider.updated`, `model.updated`, `websearch.updated` only when its list changed).
//! The sequences match the opencode 2.2.5 binary (`claude_*` parity scenarios: `credential.updated`, two pairs,
//! `credential.switched`; a credential created through `/api/credential` also boots the global config location,
//! which announces its own pairs).

mod common;

use std::time::Duration;

use common::*;
use serde_json::json;
use tokio::sync::broadcast::Receiver;

type Rx = Receiver<oc_core::bus::Event>;

/// The catalog and credential events published so far, as `kind@location` (`kind` for global ones).
fn drain(rx: &mut Rx, h: &Harness) -> Vec<String> {
    let mut out = Vec::new();
    while let Ok(e) = rx.try_recv() {
        if !(e.kind.starts_with("credential.") || matches!(e.kind.as_str(), "provider.updated" | "model.updated" | "websearch.updated")) {
            continue;
        }
        let at = match &e.location {
            Some(l) if l.directory == h.dir() => "@work",
            Some(l) if l.directory == h.app.global.config.display().to_string() => "@config",
            Some(_) => "@other",
            None => "",
        };
        out.push(format!("{}{at}", e.kind));
    }
    out
}

async fn settle() {
    tokio::time::sleep(Duration::from_millis(300)).await;
}

#[tokio::test]
async fn credential_changes_announce_availability_and_accounts_before_switching() {
    let h = harness();
    // Boot the location and build its catalog.
    assert_eq!(h.get("/api/provider").await.status, 200);
    settle().await;
    let mut rx = h.app.bus.subscribe();

    // A first key makes anthropic available, and the account map gains anthropic.
    let r = h.post("/api/integration/anthropic/connect/key", json!({ "key": "sk-1" })).await;
    assert_eq!(r.status, 204, "{}", String::from_utf8_lossy(&r.body));
    settle().await;
    assert_eq!(
        drain(&mut rx, &h),
        [
            "credential.updated",
            "provider.updated@work",
            "model.updated@work",
            "provider.updated@work",
            "model.updated@work",
            "credential.switched",
        ]
    );
    let first = h.get("/api/credential").await.json()["data"][0]["id"].as_str().unwrap().to_string();

    // A second key for the same integration changes neither availability nor accounts.
    let r = h.post("/api/integration/anthropic/connect/key", json!({ "key": "sk-2" })).await;
    assert_eq!(r.status, 204);
    settle().await;
    assert_eq!(drain(&mut rx, &h), ["credential.updated", "credential.switched"]);
    let credentials = h.get("/api/credential").await.json()["data"].clone();
    let second = credentials.as_array().unwrap().iter().find(|c| c["id"] != json!(first)).unwrap()["id"].as_str().unwrap().to_string();

    // Renaming announces only the credential list.
    let r = h.req("PATCH", &format!("/api/credential/{first}"), Some(json!({ "label": "old" }))).await;
    assert_eq!(r.status.as_u16() / 100, 2, "{}", String::from_utf8_lossy(&r.body));
    settle().await;
    assert_eq!(drain(&mut rx, &h), ["credential.updated"]);

    // Removing the inactive one switches nothing; removing the last one makes anthropic unavailable again.
    assert_eq!(h.req("DELETE", &format!("/api/credential/{first}"), None).await.status.as_u16() / 100, 2);
    settle().await;
    assert_eq!(drain(&mut rx, &h), ["credential.updated"]);
    assert_eq!(h.req("DELETE", &format!("/api/credential/{second}"), None).await.status.as_u16() / 100, 2);
    settle().await;
    assert_eq!(
        drain(&mut rx, &h),
        [
            "credential.updated",
            "provider.updated@work",
            "model.updated@work",
            "provider.updated@work",
            "model.updated@work",
            "credential.switched",
        ]
    );
}

#[tokio::test]
async fn credential_creation_announces_at_the_global_config_location_too() {
    let h = harness();
    assert_eq!(h.get("/api/provider").await.status, 200);
    settle().await;
    let mut rx = h.app.bus.subscribe();
    // Credential creation reads `integration.connect` from the global config location, which boots now without
    // waiting for its plugins: only its Provider State's first notify comes before the switch, its boot burst after.
    let r = h.post("/api/credential", json!({ "integrationID": "anthropic", "value": { "type": "key", "key": "sk" } })).await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    settle().await;
    assert_eq!(
        drain(&mut rx, &h),
        [
            "credential.updated",
            "provider.updated@work",
            "model.updated@work",
            "provider.updated@work",
            "model.updated@work",
            "provider.updated@config",
            "model.updated@config",
            "credential.switched",
            "provider.updated@config",
            "model.updated@config",
            "websearch.updated@config",
        ]
    );
    // Booted: both locations announce, and the three OpenAI provider plugins (ChatGPT, OpenAI, context caps) reload
    // on the switch at each.
    let r = h.post("/api/credential", json!({ "integrationID": "openai", "value": { "type": "key", "key": "sk" } })).await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    settle().await;
    let pairs = |at: &str, n: usize| -> Vec<String> {
        (0..n).flat_map(|_| [format!("provider.updated@{at}"), format!("model.updated@{at}")]).collect()
    };
    let mut expected = vec!["credential.updated".to_string()];
    expected.extend(pairs("work", 2));
    expected.extend(pairs("config", 2));
    expected.push("credential.switched".into());
    expected.extend(pairs("work", 3));
    expected.extend(pairs("config", 3));
    assert_eq!(drain(&mut rx, &h), expected);
}

#[tokio::test]
async fn account_changes_reload_web_search_only_when_its_list_changes() {
    let h = harness();
    std::fs::write(
        h.cwd.join("opencode.json"),
        json!({ "policies": [{ "action": "integration.use", "resource": "exa#org_1", "effect": "deny" }] }).to_string(),
    )
    .unwrap();
    let websearch = || async {
        h.get("/api/websearch/provider").await.json()["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["id"].as_str().unwrap().to_string())
            .collect::<Vec<_>>()
    };
    assert!(websearch().await.iter().any(|p| p == "exa"));
    assert_eq!(h.get("/api/provider").await.status, 200);
    settle().await;
    let mut rx = h.app.bus.subscribe();
    // An account without org changes the account map but not the web search list.
    let r = h.post("/api/integration/exa/connect/key", json!({ "key": "k1" })).await;
    assert_eq!(r.status, 204, "{}", String::from_utf8_lossy(&r.body));
    settle().await;
    let events = drain(&mut rx, &h);
    assert!(!events.iter().any(|e| e.starts_with("websearch.updated")), "{events:?}");
    assert!(events.iter().any(|e| e == "provider.updated@work"), "{events:?}");
    // org_1's key withholds exa's web search.
    let id = h.get("/api/credential").await.json()["data"][0]["id"].as_str().unwrap().to_string();
    let r = h.req("DELETE", &format!("/api/credential/{id}"), None).await;
    assert_eq!(r.status.as_u16() / 100, 2);
    settle().await;
    drain(&mut rx, &h);
    let r = h
        .post("/api/integration/exa/connect/key", json!({ "key": "k2" }))
        .await;
    assert_eq!(r.status, 204);
    let id = h.get("/api/credential").await.json()["data"][0]["id"].as_str().unwrap().to_string();
    h.app
        .db
        .with(|c| {
            c.execute(
                "UPDATE credential SET value = json_set(value, '$.metadata', json('{\"orgID\":\"org_1\"}')) WHERE id = ?",
                [&id],
            )
        })
        .unwrap();
    settle().await;
    drain(&mut rx, &h);
    // The account is read when a credential event arrives (a rename publishes one).
    let r = h.req("PATCH", &format!("/api/credential/{id}"), Some(json!({ "label": "work" }))).await;
    assert_eq!(r.status.as_u16() / 100, 2);
    settle().await;
    assert_eq!(
        drain(&mut rx, &h),
        ["credential.updated", "provider.updated@work", "model.updated@work", "websearch.updated@work"]
    );
    assert!(!websearch().await.iter().any(|p| p == "exa"));
}
