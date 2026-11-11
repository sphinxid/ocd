//! The `fetch` extension (core `codemode/web.ts`) over a mock HTTP client, plus execute-tool progress rows.

use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use oc_codemode::codemode::{CodeMode, ExecutionLimits};
use oc_codemode::execute_tool::{format_result, CallStatus, ProgressRows};
use oc_codemode::host::{
    CallResult, ExtensionInvocation, Host, HostValue, HttpClient, HttpRequest, HttpResponse, Json, ToolCallError,
};
use serde_json::json;

struct Mock {
    seen: Mutex<Vec<HttpRequest>>,
}

impl HttpClient for Mock {
    fn send(&self, request: HttpRequest) -> BoxFuture<'static, Result<HttpResponse, String>> {
        self.seen.lock().unwrap().push(request.clone());
        Box::pin(async move {
            if request.url.contains("down") {
                return Err("Unable to connect. Is the computer able to access the url?".into());
            }
            Ok(HttpResponse {
                url: request.url.clone(),
                status: 201,
                status_text: "Created".into(),
                redirected: false,
                headers: vec![("Content-Type".into(), "application/json".into())],
                body: br#"{"ok":true,"n":[1,2]}"#.to_vec(),
            })
        })
    }
}

#[derive(Default)]
struct Progress {
    rows: ProgressRows,
}

impl Host for Progress {
    fn call(&self, _path: &str, _input: Option<Json>) -> BoxFuture<'static, Result<Option<Json>, ToolCallError>> {
        Box::pin(async { Ok(None) })
    }
    fn extension_before(&self, call: &ExtensionInvocation) -> BoxFuture<'static, Result<(), HostValue>> {
        self.rows.extension_started(call);
        Box::pin(async { Ok(()) })
    }
    fn extension_after(&self, call: &ExtensionInvocation, result: &CallResult) {
        self.rows.settled(call.id, result);
    }
}

#[tokio::test]
async fn fetch_returns_the_buffered_response_shape() {
    let client = Arc::new(Mock { seen: Mutex::new(Vec::new()) });
    let host = Arc::new(Progress::default());
    let runtime = CodeMode::new(
        Vec::new(),
        host.clone(),
        vec![oc_codemode::web::extension(client.clone())],
        ExecutionLimits::default(),
    )
    .unwrap();
    let result = runtime
        .execute(
            r#"
            const r = await fetch("https://api.test/items", { method: "post", headers: { "x-a": "1" }, body: "hi" })
            return [r.status, r.ok, r.statusText, r.url, r.redirected, r.headers.get("CONTENT-TYPE"), r.headers.has("x"),
                    r.headers.entries(), await r.text(), await r.json(), (await r.bytes()).length, typeof r.json]
            "#,
        )
        .await;
    assert_eq!(
        result.to_json(),
        json!({"ok": true, "value": [201, true, "Created", "https://api.test/items", false, "application/json", false,
            [["content-type", "application/json"]], "{\"ok\":true,\"n\":[1,2]}", {"ok": true, "n": [1, 2]}, 21, "function"],
            "toolCalls": []})
    );
    let seen = client.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].method, "POST");
    assert_eq!(seen[0].body.as_deref(), Some(&b"hi"[..]));
    assert!(seen[0].headers.contains(&("x-a".to_string(), "1".to_string())));
    let rows = host.rows.rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].tool, "fetch");
    assert_eq!(rows[0].status, CallStatus::Completed);
    assert_eq!(serde_json::to_value(&rows[0].input).unwrap(), json!({"method": "POST", "url": "https://api.test/items"}));

    let failed = runtime.execute(r#"try { await fetch("https://down.test/") } catch (e) { return [e.name, e.message] }"#).await;
    assert_eq!(
        failed.to_json()["value"],
        json!(["TypeError", "Unable to connect. Is the computer able to access the url?"])
    );
    let uncaught = runtime.execute(r#"await fetch("https://down.test/")"#).await;
    assert_eq!(format_result(&uncaught), "TypeError: Unable to connect. Is the computer able to access the url?");
    assert_eq!(host.rows.rows()[2].status, CallStatus::Error);
}

/// Answers the URLs in `routes` with `(status, location)` redirects, and anything else with 200 `ok`.
struct Redirects {
    routes: Vec<(&'static str, u16, &'static str)>,
    seen: Mutex<Vec<HttpRequest>>,
}

impl HttpClient for Redirects {
    fn send(&self, request: HttpRequest) -> BoxFuture<'static, Result<HttpResponse, String>> {
        self.seen.lock().unwrap().push(request.clone());
        let route = self.routes.iter().find(|(url, _, _)| *url == request.url).map(|(_, status, location)| (*status, *location));
        Box::pin(async move {
            Ok(HttpResponse {
                url: request.url.clone(),
                status: route.map(|r| r.0).unwrap_or(200),
                status_text: String::new(),
                redirected: false,
                headers: route.map(|r| vec![("Location".to_string(), r.1.to_string())]).unwrap_or_default(),
                body: b"ok".to_vec(),
            })
        })
    }
}

type Checked = Arc<Mutex<Vec<(String, String, String)>>>;

fn checked_runtime(client: Arc<Redirects>, deny: Option<&'static str>) -> (CodeMode, Arc<Mutex<Vec<String>>>, Checked) {
    let authorized: Arc<Mutex<Vec<String>>> = Arc::default();
    let checked: Checked = Arc::default();
    let (a, c) = (authorized.clone(), checked.clone());
    let authorize: oc_codemode::web::Authorize = Arc::new(move |url, _args| {
        a.lock().unwrap().push(url);
        Box::pin(async { Ok(()) })
    });
    let check: oc_codemode::web::CheckRedirect = Arc::new(move |next: String, args: Vec<HostValue>, from: String| {
        let method = oc_codemode::execute_tool::fetch_display(&args)["method"].as_str().unwrap_or_default().to_string();
        c.lock().unwrap().push((next.clone(), from, method));
        Box::pin(async move {
            match deny {
                Some(denied) if next.contains(denied) => Err(HostValue::error("Error", "Permission denied: webfetch")),
                _ => Ok(()),
            }
        })
    });
    let extension = oc_codemode::web::extension_checked(client, Some(authorize), Some(check));
    let runtime = CodeMode::new(Vec::new(), Arc::new(Progress::default()), vec![extension], ExecutionLimits::default()).unwrap();
    (runtime, authorized, checked)
}

fn redirects(routes: Vec<(&'static str, u16, &'static str)>) -> Arc<Redirects> {
    Arc::new(Redirects { routes, seen: Mutex::new(Vec::new()) })
}

/// tool-execute.test.ts "fetch matches rules without the fragment and rejects credentials in the URL" (#61701).
#[tokio::test]
async fn fetch_drops_the_fragment_and_rejects_credentials() {
    let client = redirects(vec![]);
    let (runtime, authorized, _) = checked_runtime(client.clone(), None);
    let result = runtime
        .execute(
            r#"const r = await fetch("https://api.test/allowed#top"); const url = r.url
            try { await fetch("https://user@api.test/") } catch (e) { return [url, r.redirected, e.name, e.message] }"#,
        )
        .await;
    assert_eq!(result.to_json()["value"], json!(["https://api.test/allowed", false, "TypeError", "fetch cannot be used with credentials in the URL."]));
    assert_eq!(*authorized.lock().unwrap(), ["https://api.test/allowed"]);
    let seen: Vec<String> = client.seen.lock().unwrap().iter().map(|r| r.url.clone()).collect();
    assert_eq!(seen, ["https://api.test/allowed"]);
}

/// fetch.ts redirects (#62373): every hop is checked against the rules with the call's method and the URL the
/// program fetched (without its fragment), and a denied hop is never requested.
#[tokio::test]
async fn fetch_checks_every_redirect() {
    let client = redirects(vec![
        ("https://a.test/start", 302, "https://b.test/next#frag"),
        ("https://b.test/next", 307, "/end"),
        ("https://a.test/denied", 302, "https://a.test/blocked"),
        ("https://a.test/ftp", 301, "ftp://a.test/x"),
        ("https://a.test/creds", 301, "https://u:p@a.test/x"),
        ("https://a.test/loop", 308, "/loop"),
    ]);
    let (runtime, _, checked) = checked_runtime(client.clone(), Some("blocked"));
    let result = runtime
        .execute(r#"const r = await fetch("https://a.test/start#x", { method: "post", body: "hi" }); return [r.url, r.redirected, r.status, await r.text()]"#)
        .await;
    assert_eq!(result.to_json()["value"], json!(["https://b.test/end", true, 200, "ok"]));
    let seen = client.seen.lock().unwrap().clone();
    // 302 after a POST becomes a GET without a body; 307 keeps the method.
    let requests: Vec<(&str, &str, bool)> = seen.iter().map(|r| (r.url.as_str(), r.method.as_str(), r.body.is_some())).collect();
    assert_eq!(requests, [("https://a.test/start", "POST", true), ("https://b.test/next", "GET", false), ("https://b.test/end", "GET", false)]);
    let from = "https://a.test/start".to_string();
    assert_eq!(
        *checked.lock().unwrap(),
        [("https://b.test/next".to_string(), from.clone(), "POST".to_string()), ("https://b.test/end".to_string(), from, "POST".to_string())]
    );

    client.seen.lock().unwrap().clear();
    let denied = runtime.execute(r#"try { await fetch("https://a.test/denied") } catch (e) { return e.message }"#).await;
    assert_eq!(denied.to_json()["value"], json!("Permission denied: webfetch"));
    assert_eq!(client.seen.lock().unwrap().len(), 1);

    for (url, message) in [
        ("https://a.test/ftp", "fetch cannot follow a redirect to ftp: URLs."),
        ("https://a.test/creds", "fetch cannot follow a redirect with credentials in the URL."),
        ("https://a.test/loop", "Too many redirects (more than 20)"),
    ] {
        client.seen.lock().unwrap().clear();
        let result = runtime.execute(&format!(r#"try {{ await fetch("{url}") }} catch (e) {{ return [e.name, e.message] }}"#)).await;
        assert_eq!(result.to_json()["value"], json!(["Error", message]), "{url}");
        if url.ends_with("loop") {
            assert_eq!(client.seen.lock().unwrap().len(), 21);
        }
    }
}

/// A 303 turns any method but HEAD into a GET without a body or content headers, and a hop to another origin
/// drops the credential headers.
#[tokio::test]
async fn fetch_rewrites_redirected_requests_like_a_browser() {
    let client = redirects(vec![("https://a.test/see", 303, "https://b.test/x"), ("https://a.test/keep", 307, "/x")]);
    let (runtime, _, _) = checked_runtime(client.clone(), None);
    runtime
        .execute(r#"await fetch("https://a.test/see", { method: "PUT", body: "x", headers: [["Content-Type", "text/plain"], ["Authorization", "t"], ["Cookie", "c"], ["X-Keep", "1"]] })"#)
        .await;
    let seen = client.seen.lock().unwrap().clone();
    assert_eq!((seen[1].method.as_str(), seen[1].body.is_none()), ("GET", true));
    assert_eq!(seen[1].headers, [("X-Keep".to_string(), "1".to_string())]);
    client.seen.lock().unwrap().clear();
    runtime.execute(r#"await fetch("https://a.test/keep", { method: "PUT", body: "x", headers: { authorization: "t" } })"#).await;
    let seen = client.seen.lock().unwrap().clone();
    assert_eq!((seen[1].url.as_str(), seen[1].method.as_str(), seen[1].body.as_deref()), ("https://a.test/x", "PUT", Some(&b"x"[..])));
    assert!(seen[1].headers.contains(&("authorization".to_string(), "t".to_string())));
}
