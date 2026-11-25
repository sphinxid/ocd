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
