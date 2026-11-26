//! opencode 2.4.1 API behaviour in a release build with the `machines` flag off (ocd has no machines yet): the
//! machine routes decode their input and then answer that machines are disabled, and the link answers an empty 404.
//! Bodies were checked against the 2.4.1 binary.

mod common;

use common::*;
use serde_json::{Value, json};

const MACHINES_OFF: &str = "Machines are disabled; enable experimental.machines";

fn res(r: &Res) -> (u16, Value) {
    (r.status.as_u16(), r.json())
}

fn forbidden() -> (u16, Value) {
    (403, json!({ "_tag": "ForbiddenError", "message": MACHINES_OFF }))
}

#[tokio::test]
async fn the_machine_routes_answer_that_machines_are_disabled() {
    let h = harness();
    let route = "/api/experimental/machine";
    assert_eq!(res(&h.get(route).await), forbidden());
    assert_eq!(
        res(&h.post(route, json!({})).await),
        (400, json!({ "_tag": "InvalidRequestError", "message": "Missing key\n  at [\"name\"]", "kind": "Payload" }))
    );
    assert_eq!(res(&h.post(route, json!({ "name": "gpu", "reach": [] })).await), forbidden());
    assert_eq!(res(&h.req("DELETE", "/api/experimental/machine/gpu", None).await), forbidden());
    let link = h.get("/api/experimental/machine/link").await;
    assert_eq!(link.status.as_u16(), 404);
    assert!(link.body.is_empty());
}

/// The link with a server password: without credentials it is checked like any route (401, even for a WebSocket
/// upgrade, since the route only skips the password while machines are on), with them an empty 404; another method
/// is an unknown route, without axum's `Allow`.
#[tokio::test]
async fn the_machine_link_answers_like_an_unknown_route() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;
    let h = harness();
    let opts = oc_server::ServerOptions { version: "1.2.3".into(), ready: true, password: Some("pw".into()), ..Default::default() };
    let (router, _handle) = oc_server::router(h.engine.clone() as std::sync::Arc<dyn oc_server::Backend>, &opts).unwrap();
    let send = |method: &str, auth: bool, upgrade: bool| {
        let mut b = Request::builder().method(method).uri("/api/experimental/machine/link");
        if auth {
            b = b.header("authorization", "Basic b3BlbmNvZGU6cHc=");
        }
        if upgrade {
            b = b
                .header("upgrade", "websocket")
                .header("connection", "Upgrade")
                .header("sec-websocket-version", "13")
                .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==");
        }
        let router = router.clone();
        let request = b.body(Body::empty()).unwrap();
        async move {
            let res = router.oneshot(request).await.unwrap();
            let allow = res.headers().get("allow").is_some();
            let status = res.status().as_u16();
            let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
            (status, String::from_utf8_lossy(&body).to_string(), allow)
        }
    };
    let unauthorized = (401, r#"{"_tag":"UnauthorizedError","message":"Authentication required"}"#.to_string(), false);
    assert_eq!(send("GET", false, false).await, unauthorized);
    assert_eq!(send("GET", false, true).await, unauthorized);
    let empty = (404, String::new(), false);
    assert_eq!(send("GET", true, false).await, empty);
    assert_eq!(send("GET", true, true).await, empty);
    assert_eq!(send("POST", true, false).await, empty);
    assert_eq!(send("DELETE", true, false).await, empty);
}
