//! Trace propagation headers and the per-call HTTP hook (`StreamOptions.http`) against a local
//! HTTP server.

use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use oc_ai::protocols::openai_chat as oc;
use oc_ai::route::auth;
use oc_ai::route::transport::{HttpHook, WireRequest, WireResponse};
use oc_ai::*;
use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const SSE: &str = "data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Say ORIGINAL\"}}]}\n\n\
data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";

/// Serves `count` requests with `status` and `body`; returns the raw request texts.
async fn serve(count: usize, status: u16, body: &'static str) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move {
        let mut seen = vec![];
        for _ in 0..count {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = socket.read(&mut chunk).await.unwrap();
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf).to_string();
                if let Some(end) = text.find("\r\n\r\n") {
                    let length = text[..end]
                        .lines()
                        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap()))
                        .unwrap_or(0);
                    if buf.len() >= end + 4 + length {
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            seen.push(String::from_utf8_lossy(&buf).to_string());
            let response = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.ok();
        }
        seen
    });
    (base, handle)
}

fn request(base: &str) -> LLMRequest {
    let model = oc::compatible_route().provider("mock").base_url(Some(base.into())).auth(auth::none()).model("m1").unwrap();
    LLMRequest::new(model).prompt("hi")
}

fn header<'a>(raw: &'a str, name: &str) -> Option<&'a str> {
    raw.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

async fn text(stream: LLMStream) -> Result<String, AIError> {
    let mut out = String::new();
    let mut stream = stream;
    while let Some(event) = stream.next().await {
        if let LLMEvent::TextDelta { text, .. } = event? {
            out.push_str(&text);
        }
    }
    Ok(out)
}

#[tokio::test]
async fn requests_carry_trace_headers_of_one_process_trace() {
    let (base, server) = serve(2, 200, SSE).await;
    let client = Client::new();
    for _ in 0..2 {
        let stream = client.stream(request(&base), CancellationToken::new()).await.unwrap();
        assert_eq!(text(stream).await.unwrap(), "Say ORIGINAL");
    }
    let seen = server.await.unwrap();
    let trace = oc_util::trace::trace_id();
    let mut spans = vec![];
    for raw in &seen {
        let b3 = header(raw, "b3").expect("b3 header");
        let parts: Vec<&str> = b3.split('-').collect();
        assert_eq!(parts.len(), 4, "{b3}");
        assert_eq!(parts[0], trace);
        assert_eq!(parts[2], "1");
        assert_eq!(header(raw, "traceparent").unwrap(), format!("00-{trace}-{}-01", parts[1]));
        spans.push(parts[1].to_string());
        spans.push(parts[3].to_string());
    }
    spans.sort();
    spans.dedup();
    assert_eq!(spans.len(), 4, "every request has a fresh span and parent span");
}

#[derive(Default)]
struct Recorder {
    requests: Mutex<Vec<WireRequest>>,
    responses: Mutex<Vec<WireResponse>>,
    fail: bool,
}

#[async_trait]
impl HttpHook for Recorder {
    async fn request(&self, mut request: WireRequest) -> Result<WireRequest, String> {
        if self.fail {
            return Err("hook exploded".into());
        }
        self.requests.lock().push(request.clone());
        request.headers.insert("x-hooked".into(), "yes".into());
        request.headers.insert("b3".into(), "plugin-value".into());
        let mut body: serde_json::Value = serde_json::from_str(request.body.as_deref().unwrap()).unwrap();
        body["user"] = "hook".into();
        request.body = Some(body.to_string());
        Ok(request)
    }

    async fn response(&self, request: &WireRequest, mut response: WireResponse) -> Result<WireResponse, String> {
        assert_eq!(request.headers.get("x-hooked").map(String::as_str), Some("yes"));
        self.responses.lock().push(response.clone());
        response.body = response.body.map(|b| b.replace("ORIGINAL", "HOOKED"));
        Ok(response)
    }
}

#[tokio::test]
async fn hook_rewrites_request_and_response() {
    let (base, server) = serve(1, 200, SSE).await;
    let hook = Arc::new(Recorder::default());
    let stream = Client::new().stream_with(request(&base), CancellationToken::new(), Some(hook.clone())).await.unwrap();
    assert_eq!(text(stream).await.unwrap(), "Say HOOKED");
    let raw = server.await.unwrap().remove(0);
    assert_eq!(header(&raw, "x-hooked"), Some("yes"));
    // Trace headers are set after the hook (Effect's traced client), replacing a hook's value.
    assert!(header(&raw, "b3").unwrap().starts_with(oc_util::trace::trace_id()));
    assert!(raw.contains("\"user\":\"hook\""));
    let seen = hook.requests.lock()[0].clone();
    assert_eq!(seen.method, "POST");
    assert!(seen.url.ends_with("/v1/chat/completions"));
    assert_eq!(seen.headers.get("content-type").map(String::as_str), Some("application/json"));
    assert_eq!(seen.headers.get("content-length").cloned(), seen.body.as_ref().map(|b| b.len().to_string()));
    assert!(!seen.headers.contains_key("b3"));
    let response = hook.responses.lock()[0].clone();
    assert_eq!(response.status, 200);
    assert_eq!(response.status_text, "");
    assert_eq!(response.headers.get("content-type").map(String::as_str), Some("text/event-stream"));
}

#[tokio::test]
async fn hook_failure_is_a_transport_error_and_status_is_checked_after_the_hook() {
    let hook = Arc::new(Recorder { fail: true, ..Default::default() });
    let err = Client::new()
        .stream_with(request("http://127.0.0.1:9/v1"), CancellationToken::new(), Some(hook))
        .await
        .err()
        .expect("hook failure");
    assert_eq!(err.message, "hook exploded");
    assert!(matches!(err.kind, ErrorKind::Transport { .. }));

    let (base, server) = serve(1, 500, "{\"error\":{\"message\":\"boom\"}}").await;
    let hook = Arc::new(Recorder::default());
    let err = Client::new().stream_with(request(&base), CancellationToken::new(), Some(hook.clone())).await.err().expect("status error");
    server.await.unwrap();
    assert_eq!(hook.responses.lock()[0].status, 500);
    assert_eq!(err.message, "boom");
}
