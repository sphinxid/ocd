//! Responses validate against the normative OpenAPI schemas.

mod common;

use axum::body::Body;
use axum::http::Request;
use common::*;
use serde_json::{Value, json};

fn spec() -> Value {
    serde_json::from_str(oc_server::OPENAPI_JSON).unwrap()
}

/// The JSON schema of `operationId`'s response for `status`, with components attached so
/// `#/components/schemas/...` refs resolve.
fn response_schema(op_id: &str, status: &str) -> Value {
    let spec = spec();
    for (_, item) in spec["paths"].as_object().unwrap() {
        for (_, op) in item.as_object().unwrap() {
            if op.get("operationId").and_then(Value::as_str) == Some(op_id) {
                let schema = op["responses"][status]["content"]["application/json"]["schema"].clone();
                assert!(!schema.is_null(), "{op_id} has no {status} JSON schema");
                return json!({
                    "$schema": "https://json-schema.org/draft/2020-12/schema",
                    "allOf": [schema],
                    "components": spec["components"],
                });
            }
        }
    }
    panic!("unknown operation {op_id}");
}

fn assert_valid(op_id: &str, status: &str, instance: &Value) {
    let schema = response_schema(op_id, status);
    let validator = jsonschema::validator_for(&schema).expect("schema compiles");
    let errors: Vec<String> = validator.iter_errors(instance).map(|e| format!("{} at {}", e, e.instance_path)).collect();
    assert!(errors.is_empty(), "{op_id} {status}: {errors:#?}\ninstance: {instance}");
}

fn assert_invalid(op_id: &str, status: &str, instance: &Value) {
    let schema = response_schema(op_id, status);
    let validator = jsonschema::validator_for(&schema).expect("schema compiles");
    assert!(!validator.is_valid(instance), "{op_id} {status} unexpectedly accepted {instance}");
}

#[tokio::test]
async fn success_bodies_validate() {
    let h = harness(Some(PASSWORD));
    let d = enc(&h.dir());
    let cases: Vec<(&str, &str, Option<Value>, &str)> = vec![
        ("GET", "/api/info", None, "server.info"),
        ("POST", "/api/pair", None, "server.pair"),
        ("GET", "/api/location", None, "location.get"),
        ("GET", "/api/agent", None, "agent.list"),
        ("GET", "/api/agent/build", None, "agent.get"),
        ("GET", "/api/session?limit=2", None, "session.list"),
        ("GET", "/api/session?search=none", None, "session.list"),
        ("GET", "/api/session/ses_a", None, "session.get"),
        ("POST", "/api/session/ses_a/prompt", Some(json!({"text": "hi"})), "session.prompt"),
        ("POST", "/api/session/ses_a/interrupt", None, "session.interrupt"),
        ("GET", "/api/session/ses_a/message", None, "session.message.list"),
        ("GET", "/api/session/global/form", None, "session.form.list"),
        ("GET", "/api/project", None, "project.list"),
        ("GET", "/api/credential", None, "credential.list"),
        ("POST", "/api/rpc/a/echo", Some(json!({"input": [1, 2]})), "rpc.call"),
        ("POST", "/api/rpc/a/none", Some(json!({})), "rpc.call"),
    ];
    for (method, uri, body, op) in cases {
        let r = h.req(method, uri, body).await;
        assert_eq!(r.status, 200, "{method} {uri}: {:?}", String::from_utf8_lossy(&r.body));
        assert_valid(op, "200", &r.json());
    }
    // ticket envelope (needs the ticket header)
    let r = h
        .send(
            Request::post(format!("/api/pty/pty_1/connect-token?location[directory]={d}"))
                .header("authorization", basic(PASSWORD))
                .header("x-opencode-ticket", "1")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_valid("pty.connect.token", "200", &r.json());
    let r = h.send(Request::get(format!("/auth/connect/{}", {
        let code = h.req("POST", "/api/pair", None).await.json()["code"].as_str().unwrap().to_string();
        code
    })).body(Body::empty()).unwrap()).await;
    assert_valid("server.connect", "200", &r.json());
}

#[tokio::test]
async fn error_bodies_validate() {
    let h = harness(Some(PASSWORD));
    let cases: Vec<(&str, &str, Option<Value>, &str, &str)> = vec![
        ("GET", "/api/agent/zzz", None, "agent.get", "404"),
        ("GET", "/api/agent?location[directory]=/no/such/dir", None, "agent.list", "404"),
        ("GET", "/api/session/ses_zzz", None, "session.get", "404"),
        ("GET", "/api/session/zzz", None, "session.get", "400"),
        ("POST", "/api/session/zzz/prompt", Some(json!({"text": "x"})), "session.prompt", "400"),
        ("POST", "/api/session/ses_a/prompt", Some(json!({})), "session.prompt", "400"),
        ("GET", "/api/session?cursor=bad", None, "session.list", "400"),
        ("POST", "/api/rpc/a/bad", Some(json!({})), "rpc.call", "400"),
        ("POST", "/api/rpc/a/die", Some(json!({})), "rpc.call", "500"),
        ("POST", "/api/pty/pty_1/connect-token", None, "pty.connect.token", "403"),
        ("GET", "/api/fs/read/nope.txt", None, "fs.read", "404"),
    ];
    for (method, uri, body, op, status) in cases {
        let r = h.req(method, uri, body).await;
        assert_eq!(r.status.as_str(), status, "{method} {uri}");
        assert_valid(op, status, &r.json());
    }
    let r = h.send(Request::get("/api/project").body(Body::empty()).unwrap()).await;
    assert_valid("project.list", "401", &r.json());
}

#[test]
fn generated_types_round_trip_and_validate() {
    // Serializing generated types yields schema-valid JSON in schema key order.
    let session: oc_server::generated::SessionInfo = from(session("ses_1", 5));
    let out = serde_json::to_value(&session).unwrap();
    assert_valid("session.get", "200", &json!({ "data": out }));
    let keys: Vec<&String> = out.as_object().unwrap().keys().collect();
    assert_eq!(keys, vec!["id", "projectID", "cost", "tokens", "time", "location"]);

    let msg: oc_server::generated::SessionMessageInfo = from(message("msg_9"));
    assert!(matches!(msg, oc_server::generated::SessionMessageInfo::User(_)));

    // schema sanity: the validator does reject bad shapes
    assert_invalid("session.get", "200", &json!({"data": {"id": "nope"}}));
}

#[test]
fn every_error_component_has_a_status() {
    let spec = spec();
    let tags: Vec<String> = spec["components"]["schemas"]
        .as_object()
        .unwrap()
        .iter()
        .filter(|(k, _)| k.ends_with("ErrorEncoded"))
        .map(|(_, v)| v["properties"]["_tag"]["enum"][0].as_str().unwrap().to_string())
        .collect();
    assert_eq!(tags.len(), 39);
    for t in tags {
        assert!(oc_server::generated::ERROR_STATUS.iter().any(|(tag, _)| *tag == t), "{t}");
    }
}

#[test]
fn union_discrimination() {
    use oc_server::generated::*;
    let v = json!({"type": "key", "key": "sk"});
    let c: CredentialValue = from(v.clone());
    assert!(matches!(c, CredentialValue::Key(_)));
    assert_eq!(serde_json::to_value(&c).unwrap(), v);
    let s: FormState = from(json!({"status": "pending"}));
    assert!(matches!(s, FormState::Pending(_)));
    let n: FormValue = from(json!("Infinity"));
    assert!(matches!(n, FormValue::String(_)));
    let n: FormValue = from(json!(3));
    assert!(matches!(n, FormValue::Number(_)));
}
