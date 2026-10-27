//! `POST /api/debug/heap-dump`: ocd has no JavaScript heap, so the route fails the way upstream reports a snapshot it
//! could not write. The CLI side is tested in `crates/ocd/tests/heap_dump.rs`.

mod common;

use common::*;
use serde_json::json;

#[tokio::test]
async fn heap_dump_is_not_supported() {
    let h = harness();
    let r = h
        .post("/api/debug/heap-dump?directory=relative/dir", json!(null))
        .await;
    assert_eq!(r.status, 400);
    assert_eq!(
        r.json(),
        json!({"_tag": "InvalidRequestError", "message": "directory must be an absolute path", "field": "directory"})
    );
    for uri in [
        "/api/debug/heap-dump",
        "/api/debug/heap-dump?directory=%2Ftmp%2Fsnapshots",
    ] {
        let r = h.req("POST", uri, None).await;
        assert_eq!(r.status, 500);
        let body = r.json();
        assert_eq!(body["_tag"], "UnknownError");
        assert!(
            body["message"]
                .as_str()
                .unwrap()
                .starts_with("Failed to write heap snapshot: "),
            "{body}"
        );
        assert!(body["ref"].as_str().unwrap().starts_with("err_"));
    }
}
