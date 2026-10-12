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
