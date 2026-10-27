//! Routing, envelopes, status codes, error shapes, auth, CORS, readiness, raw endpoints.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine;
use sha2::Digest;
use common::*;
use serde_json::json;

// ---- routing & envelopes -------------------------------------------------------------------

#[tokio::test]
async fn location_envelope_from_query_header_and_cwd() {
    let h = harness(Some(PASSWORD));
    let other = tempfile::tempdir().unwrap();
    let other_dir = other.path().to_str().unwrap();

    let r = h.get(&format!("/api/agent?location%5Bdirectory%5D={}", enc(other_dir))).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.header("content-type"), Some("application/json"));
    let v = r.json();
    assert_eq!(v["location"], json!({ "directory": other_dir }));
    assert_eq!(v["data"][0]["id"], "build");
    // key order: location first
    assert!(String::from_utf8_lossy(&r.body).starts_with("{\"location\":"));

    let req = Request::get("/api/agent")
        .header("authorization", basic(PASSWORD))
        .header("x-opencode-directory", enc(other_dir))
        .body(Body::empty())
        .unwrap();
    assert_eq!(h.send(req).await.json()["location"]["directory"], other_dir);

    let r = h.get("/api/agent").await;
    assert_eq!(r.json()["location"]["directory"], h.dir());
}

#[tokio::test]
async fn missing_location_is_404() {
    let h = harness(Some(PASSWORD));
    let r = h.get("/api/agent?location[directory]=/definitely/not/here").await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(
        r.json(),
        json!({"_tag":"LocationNotFoundError","location":{"directory":"/definitely/not/here"},"message":"Location not found: /definitely/not/here"})
    );
}

#[tokio::test]
async fn typed_not_found_errors() {
    let h = harness(Some(PASSWORD));
    let r = h.get("/api/agent/plan").await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(r.json(), json!({"_tag":"AgentNotFoundError","agentID":"plan","message":"Agent not found: plan"}));
    let r = h.get("/api/session/ses_nope").await;
    assert_eq!(
        String::from_utf8_lossy(&r.body),
        r#"{"_tag":"SessionNotFoundError","sessionID":"ses_nope","message":"Session not found: ses_nope"}"#
    );
}

#[tokio::test]
async fn data_and_bare_envelopes() {
    let h = harness(Some(PASSWORD));
    let r = h.get("/api/session/ses_a").await;
    assert_eq!(r.status, StatusCode::OK);
    let v = r.json();
    assert_eq!(v["data"]["id"], "ses_a");
    assert!(v.get("location").is_none());
    // integral numbers stay integral
    assert!(String::from_utf8_lossy(&r.body).contains("\"created\":1760000000000,"));

    let r = h.get("/api/project").await;
    assert!(r.json().is_array());
    let r = h.get("/api/credential").await;
    assert_eq!(r.json()["data"][0]["value"]["type"], "key");
    let r = h.req("POST", "/api/session/ses_a/interrupt?resume=true", None).await;
    assert_eq!(r.json(), json!({"interrupted": true}));
}

#[tokio::test]
async fn no_content_is_empty_204() {
    let h = harness(Some(PASSWORD));
    let r = h.req("DELETE", "/api/session/ses_a", None).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert!(r.body.is_empty());
    assert!(r.header("content-type").is_none());
    let r = h.req("POST", "/api/vcs/init?provider=git", None).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert!(h.mock.last_call().2.contains("provider: Some(\"git\")"));
}

#[tokio::test]
async fn unknown_routes_and_methods_are_empty_404() {
    let h = harness(Some(PASSWORD));
    let r = h.get("/api/nope").await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert!(r.body.is_empty());
    let r = h.req("PUT", "/api/info", None).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert!(r.body.is_empty());
    // unauthenticated unknown route → 401 (auth runs before routing)
    let r = h.send(Request::get("/api/nope").body(Body::empty()).unwrap()).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn unimplemented_backend_method_is_500_empty() {
    let h = harness(Some(PASSWORD));
    let r = h.get(&format!("/api/model?location[directory]={}", enc(&h.dir()))).await;
    assert_eq!(r.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(r.body.is_empty());
}

// ---- request decoding ------------------------------------------------------------------------

#[tokio::test]
async fn path_param_pattern_is_params_error() {
    let h = harness(Some(PASSWORD));
    let r = h.get("/api/session/bogus").await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let v = r.json();
    assert_eq!(v["_tag"], "InvalidRequestError");
    assert_eq!(v["kind"], "Params");
}

#[tokio::test]
async fn session_scope_invalid_id() {
    let h = harness(Some(PASSWORD));
    let r = h.req("POST", "/api/session/bogus/prompt", Some(json!({"text": "hi"}))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.json(), json!({"_tag":"InvalidRequestError","message":"Invalid session ID","field":"sessionID"}));
}

#[tokio::test]
async fn body_decoding() {
    let h = harness(Some(PASSWORD));
    let r = h.req("POST", "/api/session/ses_a/prompt", Some(json!({"text": "hello", "extra": 1}))).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json()["data"]["payload"]["text"], "hello");

    let r = h.req("POST", "/api/session/ses_a/prompt", Some(json!({"nope": 1}))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.json()["kind"], "Payload");

    let r = h.req("POST", "/api/session/ses_a/prompt", None).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.json()["kind"], "Payload");

    let req = Request::post("/api/session/ses_a/prompt")
        .header("authorization", basic(PASSWORD))
        .header("content-type", "text/plain")
        .body(Body::from("hi"))
        .unwrap();
    let r = h.send(req).await;
    assert_eq!(r.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(String::from_utf8_lossy(&r.body), "Unsupported content-type: text/plain");

    // a missing content-type defaults to application/json
    let req = Request::post("/api/session/ses_a/prompt")
        .header("authorization", basic(PASSWORD))
        .body(Body::from(r#"{"text":"x"}"#))
        .unwrap();
    assert_eq!(h.send(req).await.status, StatusCode::OK);

    let r = h.req("POST", "/api/session/ses_missing/prompt", Some(json!({"text": "x"}))).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn query_booleans_and_numbers() {
    let h = harness(Some(PASSWORD));
    let r = h.req("POST", "/api/session/ses_a/interrupt?resume=yes", None).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.json()["kind"], "Query");
    let r = h.req("POST", "/api/session/ses_a/interrupt?resume=false", None).await;
    assert_eq!(r.json(), json!({"interrupted": false}));
    let r = h.get("/api/session?limit=0").await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let r = h.get("/api/session?limit=abc").await;
    assert_eq!(r.json()["kind"], "Query");
    let r = h.get("/api/session/ses_a/message?limit=201").await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn octet_stream_body() {
    let h = harness(Some(PASSWORD));
    let req = Request::post(format!("/api/experimental/fs/write?path=a.txt&location[directory]={}", enc(&h.dir())))
        .header("authorization", basic(PASSWORD))
        .header("content-type", "application/octet-stream")
        .body(Body::from("raw bytes"))
        .unwrap();
    let r = h.send(req).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json()["data"]["path"], format!("{}/a.txt", h.dir()));
    assert!(h.mock.last_call().2.contains("raw bytes") || h.mock.last_call().2.contains("b\"raw bytes\""));
    // required query param
    let req = Request::post("/api/experimental/fs/write")
        .header("authorization", basic(PASSWORD))
        .header("content-type", "application/octet-stream")
        .body(Body::from("x"))
        .unwrap();
    let r = h.send(req).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.json()["kind"], "Query");
}

#[tokio::test]
async fn form_global_session_uses_request_location() {
    let h = harness(Some(PASSWORD));
    let r = h.get("/api/session/global/form").await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json(), json!({"data": []}));
    let r = h.get("/api/session/global/form?location[directory]=/nope/x").await;
    assert_eq!(r.json()["_tag"], "LocationNotFoundError");
    let r = h.get("/api/session/xyz/form").await;
    assert_eq!(r.json()["message"], "Invalid session ID");
}

// ---- pagination cursors ----------------------------------------------------------------------

#[tokio::test]
async fn session_list_cursors() {
    let h = harness(Some(PASSWORD));
    let r = h.get("/api/session?order=desc&directory=/tmp&limit=2").await;
    assert_eq!(r.status, StatusCode::OK);
    let v = r.json();
    assert_eq!(v["data"].as_array().unwrap().len(), 2);
    let next = v["cursor"]["next"].as_str().unwrap().to_string();
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(&next).unwrap();
    assert_eq!(
        String::from_utf8(decoded).unwrap(),
        r#"{"order":"desc","directory":"/tmp","anchor":{"id":"ses_a","time":1,"direction":"next"}}"#
    );
    // paging with the cursor passes the embedded query + anchor to the backend
    let r = h.get(&format!("/api/session?cursor={next}&limit=1&order=asc")).await;
    assert_eq!(r.status, StatusCode::OK);
    let call = h.mock.last_call().2;
    assert!(call.contains("limit: 1"), "{call}");
    assert!(call.contains("Desc") && call.contains("anchor: Some"), "{call}");

    let r = h.get("/api/session?search=none").await;
    assert_eq!(r.json(), json!({"data": [], "cursor": {"previous": null, "next": null}}));

    let r = h.get("/api/session?cursor=garbage").await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.json(), json!({"_tag":"InvalidCursorError","message":"Invalid cursor"}));

    let r = h.get("/api/session?parentID=null").await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(h.mock.last_call().2.contains("Root"));
    let r = h.get("/api/session?parentID=abc").await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn message_list_cursors() {
    let h = harness(Some(PASSWORD));
    let r = h.get("/api/session/ses_a/message").await;
    let v = r.json();
    let prev = v["cursor"]["previous"].as_str().unwrap();
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(prev).unwrap();
    assert_eq!(String::from_utf8(decoded).unwrap(), r#"{"id":"msg_1","order":"desc","direction":"previous"}"#);
    let r = h.get(&format!("/api/session/ses_a/message?cursor={prev}&order=asc")).await;
    assert_eq!(r.json(), json!({"_tag":"InvalidCursorError","message":"Cursor cannot be combined with order"}));
    let r = h.get(&format!("/api/session/ses_a/message?cursor={prev}")).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(h.mock.last_call().2.contains("Previous"));
    let r = h.get("/api/session/ses_a/message?cursor=xx").await;
    assert_eq!(r.json()["_tag"], "InvalidCursorError");
}

// ---- auth -------------------------------------------------------------------------------------

#[tokio::test]
async fn unauthorized_shape_and_challenge() {
    let h = harness(Some(PASSWORD));
    let r = h.send(Request::get("/api/project").body(Body::empty()).unwrap()).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.json(), json!({"_tag":"UnauthorizedError","message":"Authentication required"}));
    assert_eq!(r.header("www-authenticate"), Some("Basic realm=\"Secure Area\""));

    let r = h.send(Request::get("/api/project").header("sec-fetch-mode", "cors").body(Body::empty()).unwrap()).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert!(r.header("www-authenticate").is_none());

    let r = h.send(Request::get("/api/project").header("authorization", basic("wrong")).body(Body::empty()).unwrap()).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);

    let token = base64::engine::general_purpose::STANDARD.encode("opencode:secret");
    let r = h.send(Request::get(format!("/api/project?auth_token={}", enc(&token))).body(Body::empty()).unwrap()).await;
    assert_eq!(r.status, StatusCode::OK);

    // openapi.json requires auth too
    let r = h.send(Request::get("/openapi.json").body(Body::empty()).unwrap()).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    let r = h.get("/openapi.json").await;
    assert_eq!(r.json()["openapi"], "3.1.0");
}

#[tokio::test]
async fn unauthenticated_mode() {
    let h = harness(None);
    let r = h.send(Request::get("/api/project").body(Body::empty()).unwrap()).await;
    assert_eq!(r.status, StatusCode::OK);
}

#[tokio::test]
async fn pairing_flow() {
    let h = harness(Some(PASSWORD));
    let r = h.req("POST", "/api/pair", None).await;
    assert_eq!(r.status, StatusCode::OK);
    let code = r.json()["code"].as_str().unwrap().to_string();
    assert_eq!(r.json()["expires_in"], 300);
    assert_eq!(code.len(), 22);

    // JSON client (no auth needed for the connect link)
    let r = h.send(Request::get(format!("/auth/connect/{code}")).body(Body::empty()).unwrap()).await;
    assert_eq!(r.status, StatusCode::OK);
    let token = r.json()["token"].as_str().unwrap().to_string();
    let r = h.send(Request::get("/api/project").header("authorization", basic(&token)).body(Body::empty()).unwrap()).await;
    assert_eq!(r.status, StatusCode::OK);

    // single use
    let r = h.send(Request::get(format!("/auth/connect/{code}")).body(Body::empty()).unwrap()).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.json(), json!({"_tag":"UnauthorizedError","message":"Pairing link expired or already used"}));

    // browser
    let r = h.req("POST", "/api/pair", None).await;
    let code = r.json()["code"].as_str().unwrap().to_string();
    let r = h
        .send(
            Request::get(format!("/auth/connect/{code}"))
                .header("accept", "text/html,application/xhtml+xml")
                .header("host", "127.0.0.1:4096")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, StatusCode::FOUND);
    assert_eq!(r.header("location"), Some("/"));
    let cookie = r.header("set-cookie").unwrap().to_string();
    assert!(cookie.starts_with("opencode_session_4096="));
    assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Lax") && cookie.contains("Max-Age=2592000"));
    let pair = cookie.split(';').next().unwrap().to_string();
    let r = h
        .send(Request::get("/api/project").header("host", "127.0.0.1:4096").header("cookie", &pair).body(Body::empty()).unwrap())
        .await;
    assert_eq!(r.status, StatusCode::OK);
    let r = h
        .send(
            Request::get("/api/project")
                .header("host", "127.0.0.1:4096")
                .header("origin", "http://localhost:3000")
                .header("cookie", &pair)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);

    let r = h
        .send(Request::get("/auth/connect/unknown").header("accept", "text/html").body(Body::empty()).unwrap())
        .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        String::from_utf8_lossy(&r.body),
        "This pairing link expired or was already used. Run `opencode pair` to get a new one."
    );
}

// ---- CORS -----------------------------------------------------------------------------------

#[tokio::test]
async fn cors_preflight_and_headers() {
    let h = harness(Some(PASSWORD));
    let r = h
        .send(
            Request::options("/api/session")
                .header("origin", "http://localhost:3000")
                .header("access-control-request-headers", "authorization,content-type")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert_eq!(r.header("access-control-allow-origin"), Some("http://localhost:3000"));
    assert_eq!(r.header("access-control-allow-methods"), Some("GET, HEAD, PUT, PATCH, POST, DELETE"));
    assert_eq!(r.header("access-control-allow-headers"), Some("authorization,content-type"));
    assert_eq!(r.header("access-control-max-age"), Some("86400"));
    assert_eq!(r.header("vary"), Some("Origin, Access-Control-Request-Headers"));
    assert!(r.header("access-control-allow-credentials").is_none());

    // on a 401, allowed origin is echoed
    let r = h.send(Request::get("/api/project").header("origin", "https://app.opencode.ai").body(Body::empty()).unwrap()).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.header("access-control-allow-origin"), Some("https://app.opencode.ai"));

    // extra --cors origin
    let r = h.send(Request::get("/api/project").header("origin", "https://extra.example").body(Body::empty()).unwrap()).await;
    assert_eq!(r.header("access-control-allow-origin"), Some("https://extra.example"));

    // disallowed: not blocked, but no allow-origin
    let r = h
        .send(
            Request::get("/api/project")
                .header("origin", "https://evil.example")
                .header("authorization", basic(PASSWORD))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.header("access-control-allow-origin").is_none());
    assert!(r.header("vary").unwrap().contains("Origin"));
}

// ---- readiness --------------------------------------------------------------------------------

#[tokio::test]
async fn readiness_gate() {
    let dir = tempfile::tempdir().unwrap();
    let mut opts = options(Some(PASSWORD), dir.path().to_str().unwrap());
    opts.ready = false;
    let (router, handle) = oc_server::router(Mock::new(), &opts).unwrap();
    let h = Harness { router, handle: handle.clone(), mock: Mock::new(), dir };

    let r = h.send(Request::get("/api/info").body(Body::empty()).unwrap()).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);

    let r = h.get("/api/info").await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(r.header("retry-after"), Some("1"));
    let v = r.json();
    assert_eq!(v["version"], "1.2.3");
    assert_eq!(v["paths"]["tmp"], "/tmp/oc");
    assert!(v.get("capabilities").is_none());

    let r = h.get("/api/project").await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(r.json(), json!({"code": "service_starting"}));
    assert_eq!(r.header("retry-after"), Some("1"));
    let r = h.send(Request::get("/api/project").body(Body::empty()).unwrap()).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);

    handle.set_failed();
    let r = h.get("/api/info").await;
    assert_eq!(r.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(r.header("retry-after").is_none());
    let r = h.get("/api/project").await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(r.json()["code"], "service_failed");
    assert_eq!(r.json()["action"], "Run `opencode service restart` after checking the service logs.");

    handle.set_stopping();
    assert_eq!(h.get("/api/project").await.json(), json!({"code": "service_stopping"}));

    handle.set_ready();
    let r = h.get("/api/info").await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json()["capabilities"], json!({"persistentPty": false}));
}

// ---- rpc --------------------------------------------------------------------------------------

#[tokio::test]
async fn rpc_call_mapping() {
    let h = harness(Some(PASSWORD));
    let r = h.req("POST", "/api/rpc/plug/echo", Some(json!({"input": {"a": 1}}))).await;
    assert_eq!(r.json(), json!({"output": {"a": 1}}));
    let r = h.req("POST", "/api/rpc/plug/none", Some(json!({}))).await;
    assert_eq!(r.json(), json!({}));
    let r = h.req("POST", "/api/rpc/plug/bad", Some(json!({}))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.json(), json!({"_tag":"RpcError","type":"rpc.bad_input","message":"bad input","data":{"x":1}}));
    let r = h.req("POST", "/api/rpc/plug/die", Some(json!({}))).await;
    assert_eq!(r.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(r.json(), json!({"_tag":"RpcInternalError","type":"rpc.internal","message":"RPC call failed"}));
}

// ---- fs.read ------------------------------------------------------------------------------------

#[tokio::test]
async fn fs_read_validators_and_ranges() {
    let h = harness(Some(PASSWORD));
    let r = h.get("/api/fs/read/hello.txt").await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(&r.body[..], b"0123456789");
    let etag = r.header("etag").unwrap().to_string();
    assert_eq!(etag, format!("W/\"a-{:x}\"", 1_760_000_000_000i64));
    assert_eq!(r.header("cache-control"), Some("no-cache, no-transform"));
    assert_eq!(r.header("accept-ranges"), Some("bytes"));
    assert_eq!(r.header("content-type"), Some("text/plain"));
    assert_eq!(r.header("last-modified"), Some("Thu, 09 Oct 2025 08:53:20 GMT"));

    let send = |hdr: &'static str, val: String| {
        Request::get("/api/fs/read/hello.txt").header("authorization", basic(PASSWORD)).header(hdr, val).body(Body::empty()).unwrap()
    };
    let r = h.send(send("if-none-match", etag.clone())).await;
    assert_eq!(r.status, StatusCode::NOT_MODIFIED);
    assert!(r.body.is_empty());
    assert_eq!(r.header("etag"), Some(etag.as_str()));

    let r = h.send(send("range", "bytes=2-4".into())).await;
    assert_eq!(r.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(&r.body[..], b"234");
    assert_eq!(r.header("content-range"), Some("bytes 2-4/10"));
    assert_eq!(r.header("content-length"), Some("3"));

    let r = h.send(send("range", "bytes=20-".into())).await;
    assert_eq!(r.status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(r.header("content-range"), Some("bytes */10"));

    let r = h.get("/api/fs/read/missing%20file.txt").await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(r.json(), json!({"_tag":"FileNotFoundError","path":"missing file.txt","message":"File not found: missing file.txt"}));
    let r = h.get("/api/fs/read/bad%zz").await;
    assert_eq!(r.json()["path"], "bad%zz");
}

// ---- PTY tickets -----------------------------------------------------------------------------

#[tokio::test]
async fn pty_connect_token_gate() {
    let h = harness(Some(PASSWORD));
    let r = h.req("POST", "/api/pty/pty_1/connect-token", None).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(r.json(), json!({"_tag":"ForbiddenError","message":"Invalid PTY connect token request"}));

    let mk = |origin: Option<&str>, id: &str| {
        let mut b = Request::post(format!("/api/pty/{id}/connect-token"))
            .header("authorization", basic(PASSWORD))
            .header("x-opencode-ticket", "1")
            .header("host", "127.0.0.1:4096");
        if let Some(o) = origin {
            b = b.header("origin", o);
        }
        b.body(Body::empty()).unwrap()
    };
    let r = h.send(mk(None, "pty_1")).await;
    assert_eq!(r.status, StatusCode::OK);
    let v = r.json();
    assert_eq!(v["location"]["directory"], h.dir());
    assert_eq!(v["data"]["expires_in"], 60);
    assert_eq!(v["data"]["ticket"].as_str().unwrap().len(), 36);

    assert_eq!(h.send(mk(Some("https://evil.example"), "pty_1")).await.status, StatusCode::FORBIDDEN);
    assert_eq!(h.send(mk(Some("http://127.0.0.1:4096"), "pty_1")).await.status, StatusCode::OK);
    let r = h.send(mk(None, "pty_2")).await;
    assert_eq!(r.json()["_tag"], "PtyNotFoundError");

    let r = h
        .send(
            Request::post("/api/experimental/persistent-pty/pty_p/connect-token")
                .header("authorization", basic(PASSWORD))
                .header("x-opencode-ticket", "1")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.json()["data"]["ticket"].is_string());
    assert!(r.json().get("location").is_none());
}

// ---- web UI + compression ------------------------------------------------------------------

fn br(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    brotli::BrotliCompress(&mut &data[..], &mut out, &Default::default()).unwrap();
    out
}

fn web_ui_harness(source: oc_server::webui::WebUiSource, dir: tempfile::TempDir) -> Harness {
    let mut opts = options(Some(PASSWORD), dir.path().to_str().unwrap());
    opts.web_ui = Some(source);
    let (router, handle) = oc_server::router(Mock::new(), &opts).unwrap();
    Harness { router, handle, mock: Mock::new(), dir }
}

fn get(path: &str, encoding: Option<&str>) -> Request<Body> {
    let mut b = Request::get(path);
    if let Some(e) = encoding {
        b = b.header("accept-encoding", e);
    }
    b.body(Body::empty()).unwrap()
}

const THEME_HTML: &[u8] = b"<html><script id=\"oc-theme-preload-script\">theme()</script><body>embedded</body></html>";

#[tokio::test]
async fn web_ui_archive_serving() {
    let dir = tempfile::tempdir().unwrap();
    let index = br(THEME_HTML);
    let js = br(b"console.log(1)");
    let big = "console.log('compressed');".repeat(100);
    let big_br = br(big.as_bytes());
    let archive = oc_server::webui::WebUi::encode_archive(&[
        ("index.html", &index),
        ("_assets/app.js", &js),
        ("_assets/big.js", &big_br),
        ("sw.js", &js),
        ("registerSW.js", &js),
    ]);
    let path = dir.path().join("app.bin");
    std::fs::write(&path, archive).unwrap();
    let h = web_ui_harness(path.into(), dir);

    // SPA fallback (deep routes, dotted names, legacy /assets), before auth
    for route in ["/", "/index.html", "/workspace/x", "/workspace/example.js", "/assets/missing.js", "/settings?x=1"] {
        let r = h.send(get(route, None)).await;
        assert_eq!(r.status, StatusCode::OK, "{route}");
        assert_eq!(&r.body[..], THEME_HTML, "{route}");
        assert_eq!(r.header("content-type"), Some("text/html"));
        assert_eq!(r.header("cache-control"), Some("no-cache"));
        let theme = base64::engine::general_purpose::STANDARD.encode(sha2::Sha256::digest(b"theme()"));
        assert_eq!(
            r.header("content-security-policy"),
            Some(format!("default-src 'self'; script-src 'self' 'wasm-unsafe-eval' 'sha256-{theme}'; style-src 'self' 'unsafe-inline'; img-src 'self' data: https: blob:; font-src 'self' data:; media-src 'self' data:; connect-src * data: blob:; worker-src 'self' blob:").as_str())
        );
        assert_eq!(r.header("x-content-type-options"), Some("nosniff"));
        // Node's compression appends Accept-Encoding after CORS's Origin
        assert_eq!(r.header("vary"), Some("Origin, Accept-Encoding"));
        assert!(r.header("www-authenticate").is_none());
    }
    // the HTML is never sent pre-compressed (its CSP hashes the inline script); it is compressed on the fly
    let r = h.send(get("/", Some("br"))).await;
    assert_eq!(r.header("content-encoding"), Some("br"));
    assert_ne!(&r.body[..], &index[..]);

    // brotli assets as stored, to browsers that take them
    let r = h.send(get("/_assets/app.js", Some("gzip, deflate, br"))).await;
    assert_eq!(r.header("content-encoding"), Some("br"));
    assert_eq!(r.header("vary"), Some("accept-encoding, Origin"));
    assert_eq!(r.header("content-type"), Some("text/javascript"));
    assert_eq!(r.header("cache-control"), Some("public, max-age=31536000, immutable"));
    assert!(!r.header("content-security-policy").unwrap().contains("sha256"));
    assert_eq!(&r.body[..], &js[..]);

    for encoding in [None, Some("br;q=0"), Some("br; q=0.0")] {
        let r = h.send(get("/_assets/app.js", encoding)).await;
        assert!(r.header("content-encoding").is_none(), "{encoding:?}");
        assert_eq!(r.header("vary"), Some("accept-encoding, Origin"));
        assert_eq!(&r.body[..], b"console.log(1)");
    }
    // gzip-only clients get on-the-fly gzip even for tiny assets (no 1 KiB threshold, like upstream)
    let r = h.send(get("/_assets/app.js", Some("gzip"))).await;
    assert_eq!(r.header("content-encoding"), Some("gzip"));
    let r = h.send(get("/_assets/big.js", Some("gzip"))).await;
    assert_eq!(r.header("content-encoding"), Some("gzip"));

    for revalidate in ["/sw.js", "/registerSW.js"] {
        assert_eq!(h.send(get(revalidate, None)).await.header("cache-control"), Some("no-cache"));
    }

    // dot segments resolve like `new URL(...)`
    let r = h.send(get("/x/../_assets/app.js", None)).await;
    assert_eq!(&r.body[..], b"console.log(1)");

    // missing hashed assets and icons: 404 no-store, empty, whatever the method
    for (method, route) in [("GET", "/_assets/missing.js"), ("GET", "/icons/x.png"), ("POST", "/_assets/old.css")] {
        let r = h.send(Request::builder().method(method).uri(route).body(Body::empty()).unwrap()).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{route}");
        assert_eq!(r.header("cache-control"), Some("no-store"));
        assert_eq!(r.header("vary"), Some("Origin"));
        assert!(r.body.is_empty());
    }

    // other methods: 405 empty
    for method in ["POST", "PUT", "DELETE", "PATCH"] {
        let r = h.send(Request::builder().method(method).uri("/x").body(Body::empty()).unwrap()).await;
        assert_eq!(r.status, StatusCode::METHOD_NOT_ALLOWED, "{method}");
        assert!(r.body.is_empty());
        assert!(r.header("content-type").is_none());
    }

    // HEAD: headers only, no Vary: Accept-Encoding (nothing to compress)
    let r = h.send(Request::head("/deep/route").body(Body::empty()).unwrap()).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.is_empty());
    assert_eq!(r.header("content-type"), Some("text/html"));
    assert!(r.header("content-security-policy").unwrap().contains("'sha256-"));
    assert_eq!(r.header("vary"), Some("Origin"));
    let r = h.send(Request::head("/_assets/app.js").header("accept-encoding", "br").body(Body::empty()).unwrap()).await;
    assert_eq!(r.header("content-encoding"), Some("br"));
    assert!(r.body.is_empty());

    // API paths still need auth, and unknown API routes never fall back to the SPA
    for route in ["/api", "/api/project", "/api/missing", "/openapi.json", "/x/../api/project"] {
        let r = h.send(get(route, None)).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{route}");
        assert_eq!(r.header("www-authenticate"), Some("Basic realm=\"Secure Area\""));
    }
    let r = h.send(Request::get("/api/missing").header("authorization", basic(PASSWORD)).body(Body::empty()).unwrap()).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert!(!String::from_utf8_lossy(&r.body).contains("embedded"));
}

#[tokio::test]
async fn web_ui_from_a_directory_uses_the_mime_lookup() {
    let dir = tempfile::tempdir().unwrap();
    let dist = dir.path().join("dist");
    std::fs::create_dir_all(dist.join("_assets")).unwrap();
    std::fs::write(dist.join("index.html"), THEME_HTML).unwrap();
    std::fs::write(dist.join("_assets/app.js"), "console.log(1)").unwrap();
    std::fs::write(dist.join("_assets/app.js.map"), "{}").unwrap();
    std::fs::write(dist.join("font.woff2"), [0u8, 1, 2, 255]).unwrap();
    let ui = oc_server::webui::WebUi::load(&dist).unwrap().with_mime(|name| {
        if name.ends_with(".woff2") { "font/woff2".into() } else if name.ends_with(".js") { "text/javascript".into() } else { "text/html".into() }
    });
    assert_eq!(ui.len(), 3);
    let h = web_ui_harness(ui.into(), dir);

    // no stored brotli: plain bytes, no Vary from the web UI itself
    let r = h.send(get("/_assets/app.js", Some("br"))).await;
    assert_eq!(r.header("content-type"), Some("text/javascript"));
    assert_eq!(r.header("content-encoding"), Some("br")); // compressed on the fly
    assert_eq!(r.header("vary"), Some("Origin, Accept-Encoding"));
    let r = h.send(get("/font.woff2", Some("br"))).await;
    assert_eq!(r.header("content-type"), Some("font/woff2"));
    assert!(r.header("content-encoding").is_none());
    assert_eq!(r.header("vary"), Some("Origin"));
    assert_eq!(&r.body[..], &[0u8, 1, 2, 255]);
    // source maps are not served
    let r = h.send(get("/_assets/app.js.map", None)).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(r.header("cache-control"), Some("no-store"));
}

#[tokio::test]
async fn empty_web_ui_answers_404_before_auth() {
    // An ocd built without the archive serves like an upstream `--skip-web-ui` build.
    let dir = tempfile::tempdir().unwrap();
    let h = web_ui_harness(oc_server::webui::WebUi::empty().into(), dir);
    for (method, route, no_store) in [
        ("GET", "/", false),
        ("GET", "/index.html", false),
        ("GET", "/deep/route", false),
        ("POST", "/", false),
        ("HEAD", "/", false),
        ("GET", "/_assets/app.js", true),
        ("GET", "/icons/a.png", true),
    ] {
        let r = h.send(Request::builder().method(method).uri(route).body(Body::empty()).unwrap()).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{method} {route}");
        assert!(r.body.is_empty());
        assert!(r.header("www-authenticate").is_none());
        assert_eq!(r.header("cache-control"), if no_store { Some("no-store") } else { None });
        assert_eq!(r.header("vary"), Some("Origin"));
    }
    assert_eq!(h.send(get("/api/info", None)).await.status, StatusCode::UNAUTHORIZED);
    // the archive the upstream build embeds for `--skip-web-ui` decodes to the same thing
    let skip = oc_server::webui::WebUi::from_archive(oc_server::webui::WebUi::encode_archive(&[]).into()).unwrap();
    assert!(skip.is_empty());
}

#[tokio::test]
async fn web_ui_bodies_are_unsized_on_the_wire() {
    // Upstream's Node server writes web UI responses chunked (HEAD: no length); so does ocd.
    let dir = tempfile::tempdir().unwrap();
    let archive = oc_server::webui::WebUi::encode_archive(&[("index.html", &br(THEME_HTML))]);
    let mut opts = options(Some(PASSWORD), dir.path().to_str().unwrap());
    opts.hostname = "127.0.0.1".into();
    opts.port = Some(0);
    opts.web_ui = Some(oc_server::webui::WebUi::from_archive(archive.into()).unwrap().into());
    let server = oc_server::serve(Mock::new(), opts).await.unwrap();
    let client = reqwest::Client::builder().no_gzip().no_brotli().no_deflate().build().unwrap();
    let r = client.get(format!("{}/deep", server.url())).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers().get("transfer-encoding").map(|v| v.to_str().unwrap()), Some("chunked"));
    assert!(r.headers().get("content-length").is_none());
    assert_eq!(&r.bytes().await.unwrap()[..], THEME_HTML);
    let r = client.head(format!("{}/deep", server.url())).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.headers().get("content-length").is_none());
    let r = client.get(format!("{}/_assets/gone.js", server.url())).send().await.unwrap();
    assert_eq!(r.status(), 404);
    assert!(r.headers().get("content-length").is_none());
    server.shutdown().await;
}

#[tokio::test]
async fn api_vary_matches_node_compression() {
    let h = harness(Some(PASSWORD));
    // compressible JSON: `Origin, Accept-Encoding`, whatever the size, compressed or not
    let r = h.send(Request::get("/api/project").header("authorization", basic(PASSWORD)).body(Body::empty()).unwrap()).await;
    assert_eq!(r.header("vary"), Some("Origin, Accept-Encoding"));
    let r = h
        .send(Request::get("/openapi.json").header("authorization", basic(PASSWORD)).header("accept-encoding", "br").body(Body::empty()).unwrap())
        .await;
    assert_eq!(r.header("content-encoding"), Some("br"));
    assert_eq!(r.header("vary"), Some("Origin, Accept-Encoding"));
    let r = h.send(get("/api/info", None)).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.header("vary"), Some("Origin, Accept-Encoding"));
}

#[tokio::test]
async fn json_responses_are_compressed() {
    let h = harness(Some(PASSWORD));
    let r = h
        .send(
            Request::get("/openapi.json")
                .header("authorization", basic(PASSWORD))
                .header("accept-encoding", "br")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(r.header("content-encoding"), Some("br"));
    // small bodies are not
    let r = h
        .send(
            Request::get("/api/project")
                .header("authorization", basic(PASSWORD))
                .header("accept-encoding", "br")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert!(r.header("content-encoding").is_none());
}

#[test]
fn every_operation_is_routed_once() {
    assert_eq!(oc_server::OPS.len(), 149);
    let mut seen = std::collections::HashSet::new();
    for op in oc_server::OPS {
        assert!(seen.insert((op.method, op.path)), "{} {}", op.method, op.path);
    }
}
