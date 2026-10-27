//! The AI SDK bridge against the real Bun sidecar: the fixture provider package
//! (`tests/fixtures/aisdk-provider`, a dependency-free LanguageModelV3) loaded from a `file://`
//! URL, the internal dynamic provider plugin, plugin `aisdk.*` hooks, the HTTP hook middleware,
//! errors and cancellation. No network.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use oc_plugin::{
    AisdkError, AisdkHttp, AisdkStreamRequest, ApiError, ApiRequest, HostOptions, InternalPlugins, LoadRequest, LocationRef,
    Operation, PluginApi, PluginHost, StorageScan,
};
use parking_lot::Mutex;
use serde_json::{Value, json};

fn bun() -> Option<PathBuf> {
    static BUN: OnceLock<Option<PathBuf>> = OnceLock::new();
    BUN.get_or_init(|| {
        let path = std::env::var_os("OCD_BUN_PATH")
            .or_else(|| std::env::var_os("OCD_TEST_BUN"))
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("OCD_SCRATCH").map(|d| PathBuf::from(d).join("bun144/bun-linux-x64/bun")).filter(|p| p.exists()))?;
        // SAFETY: set once, before any sidecar is spawned, to the same value in every test.
        unsafe { std::env::set_var("OCD_BUN_PATH", &path) };
        Some(path)
    })
    .clone()
}

macro_rules! require_bun {
    () => {
        if bun().is_none() {
            eprintln!("skipping: set OCD_BUN_PATH or OCD_TEST_BUN to a bun 1.4.4 binary");
            return;
        }
    };
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

fn provider_url() -> String {
    format!("file://{}", fixture("aisdk-provider").join("index.js").display())
}

struct NoApi;

#[async_trait]
impl PluginApi for NoApi {
    async fn call(&self, request: ApiRequest) -> Result<Value, ApiError> {
        Err(ApiError::unsupported(&request.method))
    }
    async fn invoke(&self, _location: &LocationRef, _handle: &str, _args: Vec<Value>) -> Result<Value, ApiError> {
        Err(ApiError::new("no handles"))
    }
    async fn storage_get(&self, _key: &str) -> Result<Option<Value>, ApiError> {
        Ok(None)
    }
    async fn storage_set(&self, _key: &str, _value: Value) -> Result<(), ApiError> {
        Ok(())
    }
    async fn storage_remove(&self, _key: &str) -> Result<(), ApiError> {
        Ok(())
    }
    async fn storage_scan(&self, _prefix: &str, _after: Option<&str>, _limit: Option<u64>) -> Result<StorageScan, ApiError> {
        Ok(StorageScan { entries: vec![], next: None })
    }
}

struct Setup {
    host: PluginHost,
    location: LocationRef,
    _dir: tempfile::TempDir,
}

fn setup() -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let options = HostOptions {
        bin: dir.path().join("bin"),
        cache: dir.path().join("cache"),
        locks: dir.path().join("locks"),
        cwd: dir.path().to_path_buf(),
        app: json!({ "name": "opencode", "version": "2.0.26", "channel": "local" }),
    };
    let host = PluginHost::new(options, Arc::new(NoApi));
    let location = LocationRef::new(dir.path().to_string_lossy().into_owned());
    Setup { host, location, _dir: dir }
}

fn model(provider: &str, package: &str, settings: Value) -> Value {
    json!({
        "id": "m1", "modelID": "m1", "providerID": provider, "name": "M1",
        "package": format!("aisdk:{package}"), "settings": settings,
        "capabilities": { "tools": true, "input": ["text"], "output": ["text"] }, "variants": [],
        "time": { "released": 0 }, "cost": [], "status": "active", "enabled": true,
        "limit": { "context": 1000, "output": 100 },
    })
}

fn options() -> Value {
    json!({ "prompt": [{ "role": "user", "content": [{ "type": "text", "text": "hi" }] }], "tools": [] })
}

async fn collect(host: &PluginHost, request: AisdkStreamRequest) -> (Vec<Value>, Option<AisdkError>) {
    let mut parts = vec![];
    let mut stream = match host.aisdk_stream(request).await {
        Ok(stream) => stream,
        Err(error) => return (parts, Some(error)),
    };
    while let Some(item) = tokio::time::timeout(Duration::from_secs(20), stream.next()).await.expect("stream stalled") {
        match item {
            Ok(part) => parts.push(part),
            Err(error) => return (parts, Some(error)),
        }
    }
    (parts, None)
}

fn stream_request(s: &Setup, model: Value) -> AisdkStreamRequest {
    AisdkStreamRequest { location: s.location.clone(), model, options: options(), http: None, enabled: None }
}

#[tokio::test]
async fn scripted_stream_through_the_dynamic_plugin() {
    require_bun!();
    let s = setup();
    let script = json!([
        { "type": "stream-start", "warnings": [] },
        { "type": "reasoning-start", "id": "r" },
        { "type": "reasoning-delta", "id": "r", "delta": "hmm" },
        { "type": "reasoning-end", "id": "r" },
        { "type": "text-start", "id": "t" },
        { "type": "text-delta", "id": "t", "delta": "hello" },
        { "type": "text-end", "id": "t" },
        { "type": "tool-call", "toolCallId": "c1", "toolName": "read", "input": "{\"path\":\"a\"}" },
        { "type": "finish", "finishReason": { "unified": "tool-calls", "raw": "tool_calls" },
          "usage": { "inputTokens": { "total": 3 }, "outputTokens": { "total": 2 } } },
    ]);
    let m = model("fx", &provider_url(), json!({ "script": script }));
    // No plugin was ever loaded: the bridge starts the sidecar on its own.
    assert!(!s.host.is_running());
    s.host.aisdk_load(&s.location, m.clone(), None).await.unwrap();
    assert!(s.host.is_running());
    let (parts, error) = collect(&s.host, stream_request(&s, m)).await;
    assert_eq!(error, None);
    let types: Vec<&str> = parts.iter().map(|p| p["type"].as_str().unwrap()).collect();
    // stream-start is dropped in the sidecar, like upstream's streamPartEvents.
    assert_eq!(
        types,
        ["reasoning-start", "reasoning-delta", "reasoning-end", "text-start", "text-delta", "text-end", "tool-call", "finish"]
    );
    assert_eq!(parts[7]["usage"]["inputTokens"]["total"], 3);
}

#[tokio::test]
async fn errors_and_initialization_failures() {
    require_bun!();
    let s = setup();
    // A streamed error part ends the stream with its error shape.
    let m = model(
        "fx",
        &provider_url(),
        json!({ "script": [
            { "type": "text-delta", "id": "t", "delta": "x" },
            { "type": "error", "error": { "error": { "message": "quota gone", "code": "insufficient_quota" } } },
        ] }),
    );
    let (parts, error) = collect(&s.host, stream_request(&s, m)).await;
    assert_eq!(parts.len(), 1);
    let Some(AisdkError::Provider { operation, shape }) = error else { panic!("{error:?}") };
    assert_eq!(operation, "read");
    assert_eq!(shape["kind"], "unknown");
    assert_eq!(shape["message"], "quota gone");

    // A stream that errors while reading.
    let m = model("fx", &provider_url(), json!({ "script": [{ "type": "throw", "message": "terminated" }] }));
    let (_, error) = collect(&s.host, stream_request(&s, m)).await;
    let Some(AisdkError::Provider { shape, .. }) = error else { panic!("{error:?}") };
    assert_eq!(shape["kind"], "network");

    // Package without a `create*` export.
    let bad = format!("file://{}", fixture("aisdk-provider").join("package.json").display());
    let error = s.host.aisdk_load(&s.location, model("fx", &bad, json!({})), None).await.unwrap_err();
    assert!(matches!(&error, AisdkError::Init { detail: Some(d) } if d.contains("Cannot") || d.contains("no provider factory") || d.contains("JSON")), "{error:?}");

    // The dynamic plugin removed by configuration: nothing returns an SDK.
    let error = s
        .host
        .aisdk_load(&s.location, model("fy", &provider_url(), json!({})), Some(vec!["opencode.provider.cohere".into()]))
        .await
        .unwrap_err();
    assert_eq!(error, AisdkError::Init { detail: Some("No AISDK provider plugin returned an SDK".into()) });
}

struct Recorder {
    seen: Mutex<Vec<Value>>,
    response: bool,
}

#[async_trait]
impl AisdkHttp for Recorder {
    async fn request(&self, mut request: Value) -> Result<Value, String> {
        self.seen.lock().push(request.clone());
        request["headers"]["x-hooked"] = json!("1");
        Ok(request)
    }
    async fn response(&self, _request: Value, mut response: Value) -> Result<Value, String> {
        let body = response["body"].as_str().unwrap_or_default().replace("ORIGINAL", "REWRITTEN");
        response["body"] = json!(body);
        Ok(response)
    }
    fn wants_response(&self) -> bool {
        self.response
    }
}

/// A one-shot OpenAI-compatible SSE server; returns its base URL and the raw request.
async fn sse_server(text: &'static str) -> (String, tokio::task::JoinHandle<String>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut raw = vec![];
        let mut buf = [0u8; 8192];
        loop {
            let n = socket.read(&mut buf).await.unwrap();
            raw.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&raw).to_string();
            if let Some(end) = text.find("\r\n\r\n") {
                let length: usize = text
                    .lines()
                    .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0)))
                    .unwrap_or(0);
                if raw.len() >= end + 4 + length {
                    break;
                }
            }
            if n == 0 {
                break;
            }
        }
        let chunk = |v: Value| format!("data: {v}\n\n");
        let body = [
            chunk(json!({ "id": "x", "choices": [{ "index": 0, "delta": { "content": text } }] })),
            chunk(json!({ "id": "x", "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }], "usage": { "prompt_tokens": 4, "completion_tokens": 1 } })),
            "data: [DONE]\n\n".to_string(),
        ]
        .concat();
        let response = format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
        socket.write_all(response.as_bytes()).await.unwrap();
        socket.shutdown().await.ok();
        String::from_utf8_lossy(&raw).to_string()
    });
    (url, handle)
}

#[tokio::test]
async fn plugin_hooks_and_http_middleware() {
    require_bun!();
    let s = setup();
    let result = s
        .host
        .load(LoadRequest {
            location: s.location.clone(),
            info: json!({ "directory": s.location.directory, "project": { "id": "global", "directory": s.location.directory, "canonical": s.location.directory } }),
            operations: vec![Operation::Add { target: fixture("aisdk-plugin").to_string_lossy().into_owned(), options: Default::default(), mtime: None }],
            internal: InternalPlugins { pre: vec![], post: vec![] },
            blocked: None,
        })
        .await;
    assert_eq!(result.resolution.enabled_ids(), ["fixture.aisdk"]);
    let (url, server) = sse_server("ORIGINAL text").await;
    let m = model("fixture", &provider_url(), json!({ "baseURL": url, "apiKey": "k" }));
    let recorder = Arc::new(Recorder { seen: Mutex::new(vec![]), response: true });
    let mut request = stream_request(&s, m);
    request.http = Some(recorder.clone());
    let (parts, error) = collect(&s.host, request).await;
    assert_eq!(error, None);
    let raw = server.await.unwrap();
    // The language hook added a header naming what the sdk hook saw; the sdk hook renamed the model.
    let seen = recorder.seen.lock().clone();
    assert_eq!(seen.len(), 1);
    let headers = &seen[0]["headers"];
    assert_eq!(headers["x-fixture-aisdk"], format!("{}|fixture/m1|function|fixture", provider_url()));
    assert_eq!(headers["authorization"], "Bearer k");
    assert!(seen[0]["body"].as_str().unwrap().contains("\"model\":\"m1-wrapped\""));
    // The request hook's header reached the server; the response hook rewrote the body.
    assert!(raw.to_ascii_lowercase().contains("x-hooked: 1"), "{raw}");
    let text: String = parts.iter().filter(|p| p["type"] == "text-delta").map(|p| p["delta"].as_str().unwrap()).collect();
    assert_eq!(text, "REWRITTEN text");
    let finish = parts.iter().find(|p| p["type"] == "finish").unwrap();
    assert_eq!(finish["usage"]["inputTokens"]["total"], 4);
}

#[tokio::test]
async fn dropping_the_stream_cancels_it() {
    require_bun!();
    let s = setup();
    // A server that accepts and never answers.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let accepted = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        // Wait until the client gives up (the connection closes).
        let mut buf = [0u8; 4096];
        loop {
            match tokio::time::timeout(Duration::from_secs(20), socket.readable()).await {
                Ok(Ok(())) => match socket.try_read(&mut buf) {
                    Ok(0) => return true,
                    Ok(_) => continue,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                    Err(_) => return true,
                },
                _ => return false,
            }
        }
    });
    let m = model("fx", &provider_url(), json!({ "baseURL": url }));
    let stream = s.host.aisdk_stream(stream_request(&s, m)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    drop(stream);
    // The fixture does not pass an abort signal (upstream sends none either), so the request itself
    // stays open until the server answers; the sidecar call is cancelled and its reader released.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(s.host.is_running());
    accepted.abort();
}
