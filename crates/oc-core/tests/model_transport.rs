//! `SessionModelTransport` + the Open Responses channel against a local WebSocket/HTTP mock: one socket
//! per session with continuation frames, error frames, dropped sockets, refused upgrades (HTTP
//! fallback), 1009 rejections, idle timeouts and closing a session.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use oc_ai::error::ErrorKind;
use oc_ai::{AIError, Client, LLMRequest, LLMResponse, Message, StreamOptions};
use oc_core::model_transport::{IdleTimeout, Interceptor, SessionModelTransport};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
enum Action {
    /// Answer with these events (text frames, or SSE over HTTP).
    Respond(Vec<Value>),
    /// Send one frame (an error event) and keep the socket.
    Frame(Value),
    /// Send these events, then drop the TCP connection without a close frame.
    Drop(Vec<Value>),
    /// Close the socket with this code before answering.
    Close(u16),
    /// Never answer.
    Silent,
    /// Refuse the next upgrade with this HTTP status.
    Reject(u16),
}

#[derive(Debug, Clone, PartialEq)]
enum Log {
    Connect { conn: usize, headers: Vec<(String, String)> },
    Frame { conn: usize, body: Value },
    Closed { conn: usize, code: Option<u16> },
    Http { body: Value },
}

struct Mock {
    base: String,
    actions: Arc<Mutex<VecDeque<Action>>>,
    log: Arc<Mutex<Vec<Log>>>,
}

impl Mock {
    async fn start(actions: Vec<Action>) -> Mock {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let actions = Arc::new(Mutex::new(VecDeque::from(actions)));
        let log: Arc<Mutex<Vec<Log>>> = Arc::default();
        let (a, l) = (actions.clone(), log.clone());
        tokio::spawn(async move {
            let mut conn = 0;
            loop {
                let Ok((stream, _)) = listener.accept().await else { return };
                conn += 1;
                tokio::spawn(handle(conn, stream, a.clone(), l.clone()));
            }
        });
        Mock { base, actions, log }
    }

    fn log(&self) -> Vec<Log> {
        self.log.lock().clone()
    }

    fn frames(&self) -> Vec<(usize, Value)> {
        self.log().into_iter().filter_map(|l| if let Log::Frame { conn, body } = l { Some((conn, body)) } else { None }).collect()
    }

    fn connections(&self) -> usize {
        self.log().iter().filter(|l| matches!(l, Log::Connect { .. })).count()
    }

    fn remaining(&self) -> usize {
        self.actions.lock().len()
    }
}

async fn handle(conn: usize, stream: tokio::net::TcpStream, actions: Arc<Mutex<VecDeque<Action>>>, log: Arc<Mutex<Vec<Log>>>) {
    let mut head = vec![0u8; 8192];
    let head = loop {
        let n = stream.peek(&mut head).await.unwrap();
        let text = String::from_utf8_lossy(&head[..n]).to_string();
        if text.contains("\r\n\r\n") || n == head.len() {
            break text;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    if head.to_ascii_lowercase().contains("upgrade: websocket") {
        let reject = {
            let mut a = actions.lock();
            match a.front() {
                Some(Action::Reject(status)) => {
                    let status = *status;
                    a.pop_front();
                    Some(status)
                }
                _ => None,
            }
        };
        let mut stream = stream;
        if let Some(status) = reject {
            log.lock().push(Log::Connect { conn, headers: vec![] });
            let _ = stream.write_all(format!("HTTP/1.1 {status} Refused\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").as_bytes()).await;
            let _ = stream.shutdown().await;
            return;
        }
        let seen: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
        let capture = seen.clone();
        let callback = move |request: &tokio_tungstenite::tungstenite::handshake::server::Request, response| {
            *capture.lock() = request.headers().iter().map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string())).collect();
            Ok(response)
        };
        let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(stream, callback).await else { return };
        log.lock().push(Log::Connect { conn, headers: seen.lock().clone() });
        while let Some(message) = ws.next().await {
            match message {
                Ok(WsMessage::Text(text)) => {
                    log.lock().push(Log::Frame { conn, body: serde_json::from_str(&text).unwrap() });
                    let action = actions.lock().pop_front().unwrap_or(Action::Silent);
                    match action {
                        Action::Respond(events) => {
                            for e in events {
                                ws.send(WsMessage::Text(e.to_string().into())).await.unwrap();
                            }
                        }
                        Action::Frame(frame) => ws.send(WsMessage::Text(frame.to_string().into())).await.unwrap(),
                        Action::Drop(events) => {
                            for e in events {
                                ws.send(WsMessage::Text(e.to_string().into())).await.unwrap();
                            }
                            ws.flush().await.ok();
                            log.lock().push(Log::Closed { conn, code: None });
                            return;
                        }
                        Action::Close(code) => {
                            let frame = tokio_tungstenite::tungstenite::protocol::CloseFrame { code: code.into(), reason: "closing".into() };
                            ws.close(Some(frame)).await.ok();
                            log.lock().push(Log::Closed { conn, code: Some(code) });
                            return;
                        }
                        Action::Silent | Action::Reject(_) => {}
                    }
                }
                Ok(WsMessage::Close(frame)) => {
                    log.lock().push(Log::Closed { conn, code: frame.map(|f| u16::from(f.code)) });
                    return;
                }
                Ok(_) => {}
                Err(_) => {
                    log.lock().push(Log::Closed { conn, code: None });
                    return;
                }
            }
        }
        return;
    }
    // Plain HTTP (the fallback): read the request, answer the next action as SSE.
    let mut stream = stream;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let body = loop {
        let n = stream.read(&mut chunk).await.unwrap();
        buf.extend_from_slice(&chunk[..n]);
        let text = String::from_utf8_lossy(&buf).to_string();
        if let Some(end) = text.find("\r\n\r\n") {
            let length = text[..end]
                .lines()
                .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap()))
                .unwrap_or(0);
            if buf.len() >= end + 4 + length {
                break text[end + 4..].to_string();
            }
        }
        if n == 0 {
            break String::new();
        }
    };
    log.lock().push(Log::Http { body: serde_json::from_str(&body).unwrap_or(Value::Null) });
    let events = match actions.lock().pop_front() {
        Some(Action::Respond(events)) => events,
        _ => vec![],
    };
    let sse: String = events.iter().map(|e| format!("data: {e}\n\n")).collect();
    let response = format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{sse}", sse.len());
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// A complete Responses answer with one text item.
fn answer(id: &str, text: &str) -> Vec<Value> {
    let item = json!({"id": format!("msg_{id}"), "type": "message", "role": "assistant", "status": "completed",
        "content": [{"type": "output_text", "text": text, "annotations": []}]});
    vec![
        json!({"type": "response.created", "response": {"id": id, "output": []}}),
        json!({"type": "response.output_item.added", "output_index": 0, "item": {"id": format!("msg_{id}"), "type": "message", "role": "assistant", "content": []}}),
        json!({"type": "response.output_text.delta", "item_id": format!("msg_{id}"), "output_index": 0, "content_index": 0, "delta": text}),
        json!({"type": "response.output_item.done", "output_index": 0, "item": item}),
        json!({"type": "response.completed", "response": {"id": id, "status": "completed", "output": [item],
            "usage": {"input_tokens": 3, "output_tokens": 2, "total_tokens": 5}}}),
    ]
}

fn model(base: &str) -> oc_ai::LanguageModel {
    let settings = json!({"baseURL": base, "apiKey": "test"});
    oc_ai::providers::language_model("@opencode/ai/providers/openai", "m1", settings.as_object().unwrap()).unwrap()
}

async fn run(base: &str, messages: Vec<Message>, executor: Arc<dyn oc_ai::route::websocket::ChannelExecutor>) -> Result<LLMResponse, AIError> {
    let mut request = LLMRequest::new(model(base));
    request.messages = messages;
    let options = StreamOptions { http: None, web_socket: Some(executor) };
    Client::new().generate_opts(request, CancellationToken::new(), options).await
}

fn text(response: &LLMResponse) -> String {
    response.message.content.iter().filter_map(|p| if let oc_ai::ContentPart::Text(t) = p { Some(t.text.clone()) } else { None }).collect()
}

fn transport_fields(error: &AIError) -> (Option<String>, Option<String>, Option<String>, Option<String>) {
    match &error.kind {
        ErrorKind::Transport { code, phase, delivery, recovery, .. } => (code.clone(), phase.clone(), delivery.clone(), recovery.clone()),
        other => panic!("not a transport error: {other:?}"),
    }
}

async fn settle() {
    tokio::time::sleep(Duration::from_millis(150)).await;
}

#[tokio::test]
async fn steps_continue_over_one_socket() {
    let mock = Mock::start(vec![Action::Respond(answer("resp_1", "One.")), Action::Respond(answer("resp_2", "Two."))]).await;
    let transport = Arc::new(SessionModelTransport::default());
    let executor = transport.bind("ses_1", None, IdleTimeout::Default);
    let first = run(&mock.base, vec![Message::user_text("hi")], executor.clone()).await.unwrap();
    assert_eq!(text(&first), "One.");
    let history = vec![Message::user_text("hi"), first.message.clone(), Message::user_text("again")];
    let second = run(&mock.base, history, executor.clone()).await.unwrap();
    assert_eq!(text(&second), "Two.");

    assert_eq!(mock.connections(), 1);
    let frames = mock.frames();
    assert_eq!(frames.len(), 2);
    let (conn, full) = &frames[0];
    assert_eq!(*conn, 1);
    assert_eq!(full["type"], "response.create");
    assert!(full.get("previous_response_id").is_none());
    assert!(full.get("stream").is_none());
    let (conn, incremental) = &frames[1];
    assert_eq!(*conn, 1);
    assert_eq!(incremental["previous_response_id"], "resp_1");
    assert_eq!(incremental["input"], json!([{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "again"}]}]));
    // The handshake carries the request headers and the Responses WebSocket opt-in, no tracing headers.
    let Log::Connect { headers, .. } = &mock.log()[0] else { panic!() };
    let header = |name: &str| headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
    assert_eq!(header("authorization").as_deref(), Some("Bearer test"));
    assert_eq!(header("openai-beta").as_deref(), Some("responses_websockets=2026-02-06"));
    assert_eq!(header("traceparent"), None);
    assert_eq!(header("content-type"), None);
    assert!(transport.connected("ses_1"));

    // Removing the session closes its socket normally.
    transport.close("ses_1");
    settle().await;
    assert!(!transport.connected("ses_1"));
    assert_eq!(mock.log().last(), Some(&Log::Closed { conn: 1, code: Some(1000) }));
}

#[tokio::test]
async fn rejected_continuation_closes_the_socket_and_the_retry_goes_full() {
    let gone = json!({"type": "error", "status": 400, "error": {"type": "invalid_request_error", "code": "previous_response_not_found", "message": "gone"}});
    let mock = Mock::start(vec![
        Action::Respond(answer("resp_1", "One.")),
        Action::Frame(gone.clone()),
        Action::Respond(answer("resp_2", "Two.")),
    ])
    .await;
    let transport = Arc::new(SessionModelTransport::default());
    let executor = transport.bind("ses_1", None, IdleTimeout::Default);
    let first = run(&mock.base, vec![Message::user_text("hi")], executor.clone()).await.unwrap();
    let history = vec![Message::user_text("hi"), first.message.clone(), Message::user_text("again")];
    let error = run(&mock.base, history.clone(), executor.clone()).await.unwrap_err();
    assert_eq!(error.message, "previous_response_not_found: gone");
    assert_eq!(error.body.as_deref(), Some(gone.to_string().as_str()));
    let (_, phase, delivery, recovery) = transport_fields(&error);
    assert_eq!((phase.as_deref(), delivery.as_deref(), recovery.as_deref()), (Some("receive"), Some("rejected"), Some("retry-full")));
    assert!(!error.is_retryable());
    settle().await;
    assert!(!transport.connected("ses_1"));

    let retried = run(&mock.base, history, executor).await.unwrap();
    assert_eq!(text(&retried), "Two.");
    let frames = mock.frames();
    assert_eq!(frames.iter().map(|(c, _)| *c).collect::<Vec<_>>(), vec![1, 1, 2]);
    assert!(frames[2].1.get("previous_response_id").is_none());
    assert_eq!(frames[2].1["input"].as_array().unwrap().len(), 3);
    assert!(mock.log().contains(&Log::Closed { conn: 1, code: Some(1000) }));
}

#[tokio::test]
async fn provider_error_frames_and_dropped_sockets() {
    let busy = json!({"type": "error", "error": {"type": "server_error", "code": "server_error", "message": "busy"}});
    let partial: Vec<Value> = answer("resp_2", "Partial")[..3].to_vec();
    let mock = Mock::start(vec![
        Action::Frame(busy.clone()),
        Action::Drop(partial),
        Action::Close(1011),
        Action::Respond(answer("resp_3", "Done.")),
    ])
    .await;
    let transport = Arc::new(SessionModelTransport::default());
    let executor = transport.bind("ses_1", None, IdleTimeout::Default);
    let history = vec![Message::user_text("hi")];

    // An error frame is a classified provider failure; the socket is dropped after it.
    let error = run(&mock.base, history.clone(), executor.clone()).await.unwrap_err();
    assert_eq!(error.message, "server_error: busy");
    assert_eq!(error.tag(), "ProviderInternal");
    assert_eq!(error.body.as_deref(), Some(busy.to_string().as_str()));
    settle().await;
    assert!(!transport.connected("ses_1"));

    // A socket lost after the provider answered: abnormal closure, delivery accepted, retryable.
    let error = run(&mock.base, history.clone(), executor.clone()).await.unwrap_err();
    assert_eq!(error.message, "WebSocket closed with code 1006");
    assert_eq!(error.body.as_deref(), Some(""));
    let (code, phase, delivery, _) = transport_fields(&error);
    assert_eq!((code.as_deref(), phase.as_deref(), delivery.as_deref()), (Some("1006"), Some("close"), Some("accepted")));
    assert!(error.is_retryable());

    // A close frame before any output: ambiguous delivery, still retryable.
    let error = run(&mock.base, history.clone(), executor.clone()).await.unwrap_err();
    assert_eq!(error.message, "WebSocket closed with code 1011");
    assert_eq!(error.body.as_deref(), Some("closing"));
    let (code, phase, delivery, _) = transport_fields(&error);
    assert_eq!((code.as_deref(), phase.as_deref(), delivery.as_deref()), (Some("1011"), Some("close"), Some("ambiguous")));

    let done = run(&mock.base, history, executor).await.unwrap();
    assert_eq!(text(&done), "Done.");
    assert_eq!(mock.connections(), 4);
    assert_eq!(mock.remaining(), 0);
}

#[tokio::test]
async fn refused_upgrade_pins_the_session_to_http() {
    let mock = Mock::start(vec![
        Action::Reject(403),
        Action::Respond(answer("resp_1", "Over HTTP.")),
        Action::Respond(answer("resp_2", "Still HTTP.")),
        Action::Respond(answer("resp_3", "Socket.")),
    ])
    .await;
    let transport = Arc::new(SessionModelTransport::default());
    let executor = transport.bind("ses_1", None, IdleTimeout::Default);
    let first = run(&mock.base, vec![Message::user_text("hi")], executor.clone()).await.unwrap();
    assert_eq!(text(&first), "Over HTTP.");
    let second = run(&mock.base, vec![Message::user_text("hi")], executor).await.unwrap();
    assert_eq!(text(&second), "Still HTTP.");
    let log = mock.log();
    assert_eq!(log.iter().filter(|l| matches!(l, Log::Http { .. })).count(), 2);
    let Log::Http { body } = &log[1] else { panic!("{log:?}") };
    assert_eq!(body["stream"], true);
    assert_eq!(mock.connections(), 1);
    // Another session tries its own socket.
    let other = transport.bind("ses_2", None, IdleTimeout::Default);
    let third = run(&mock.base, vec![Message::user_text("hi")], other).await.unwrap();
    assert_eq!(text(&third), "Socket.");
    assert_eq!(mock.connections(), 2);
}

#[tokio::test]
async fn message_too_large_falls_back_to_http() {
    let mock = Mock::start(vec![Action::Close(1009), Action::Respond(answer("resp_1", "Over HTTP."))]).await;
    let transport = Arc::new(SessionModelTransport::default());
    let executor = transport.bind("ses_1", None, IdleTimeout::Default);
    let response = run(&mock.base, vec![Message::user_text("hi")], executor).await.unwrap();
    assert_eq!(text(&response), "Over HTTP.");
    assert!(mock.log().iter().any(|l| matches!(l, Log::Http { .. })));
}

#[tokio::test]
async fn idle_timeout_and_interceptors() {
    struct Tap(Mutex<Vec<String>>);
    #[async_trait::async_trait]
    impl Interceptor for Tap {
        async fn handshake(&self, url: String, mut headers: oc_ai::route::Headers) -> Result<(String, oc_ai::route::Headers), String> {
            headers.insert("x-tap".into(), "1".into());
            Ok((url, headers))
        }
        async fn send(&self, frame: String) -> Result<String, String> {
            self.0.lock().push(frame.clone());
            let mut value: Value = serde_json::from_str(&frame).unwrap();
            value["user"] = json!("tapped");
            Ok(value.to_string())
        }
        async fn receive(&self, frame: String) -> Result<String, String> {
            Ok(frame.replace("One.", "Uno."))
        }
    }
    let mock = Mock::start(vec![Action::Respond(answer("resp_1", "One.")), Action::Silent]).await;
    let transport = Arc::new(SessionModelTransport::default());
    let tap = Arc::new(Tap(Mutex::default()));
    let executor = transport.bind("ses_1", Some(tap.clone()), IdleTimeout::After(Duration::from_millis(300)));
    let first = run(&mock.base, vec![Message::user_text("hi")], executor.clone()).await.unwrap();
    assert_eq!(text(&first), "Uno.");
    assert_eq!(mock.frames()[0].1["user"], "tapped");
    let Log::Connect { headers, .. } = &mock.log()[0] else { panic!() };
    assert!(headers.iter().any(|(k, v)| k == "x-tap" && v == "1"));
    assert_eq!(tap.0.lock().len(), 1);

    let error = run(&mock.base, vec![Message::user_text("hi")], executor).await.unwrap_err();
    assert_eq!(error.message, "Timed out waiting for WebSocket data");
    let (code, _, delivery, _) = transport_fields(&error);
    assert_eq!((code.as_deref(), delivery.as_deref()), (Some("idle-timeout"), Some("ambiguous")));
    settle().await;
    assert!(!transport.connected("ses_1"));
}

#[tokio::test]
async fn routes_without_a_channel_and_disabled_endpoints_stay_on_http() {
    // Azure only offers the socket on `*.openai.azure.com`; a custom host uses HTTP (warning logged).
    let mock = Mock::start(vec![Action::Respond(answer("resp_1", "HTTP."))]).await;
    let settings = json!({"baseURL": mock.base, "apiKey": "k"});
    let model = oc_ai::providers::language_model("@opencode/ai/providers/azure", "m1", settings.as_object().unwrap()).unwrap();
    assert!(model.route.channel.is_some());
    let transport = Arc::new(SessionModelTransport::default());
    let options = StreamOptions { http: None, web_socket: Some(transport.bind("ses_1", None, IdleTimeout::Default)) };
    let response = Client::new().generate_opts(LLMRequest::new(model).prompt("hi"), CancellationToken::new(), options).await.unwrap();
    assert_eq!(text(&response), "HTTP.");
    assert_eq!(mock.connections(), 0);
}

#[tokio::test]
async fn changed_handshake_rotates_the_socket_and_drops_the_continuation() {
    struct Header(&'static str);
    #[async_trait::async_trait]
    impl Interceptor for Header {
        async fn handshake(&self, url: String, mut headers: oc_ai::route::Headers) -> Result<(String, oc_ai::route::Headers), String> {
            headers.insert("x-account".into(), self.0.into());
            Ok((url, headers))
        }
    }
    let mock = Mock::start(vec![
        Action::Respond(answer("resp_1", "One.")),
        Action::Respond(answer("resp_2", "Two.")),
        Action::Respond(answer("resp_3", "Three.")),
    ])
    .await;
    let transport = Arc::new(SessionModelTransport::default());
    let first = run(&mock.base, vec![Message::user_text("hi")], transport.bind("ses_1", Some(Arc::new(Header("a"))), IdleTimeout::Default)).await.unwrap();
    let history = vec![Message::user_text("hi"), first.message.clone(), Message::user_text("again")];
    // Same handshake: reused socket, continuation frame.
    let second = run(&mock.base, history.clone(), transport.bind("ses_1", Some(Arc::new(Header("a"))), IdleTimeout::Default)).await.unwrap();
    // Another account: a new socket and the whole conversation.
    let history = [history, vec![second.message.clone(), Message::user_text("third")]].concat();
    run(&mock.base, history, transport.bind("ses_1", Some(Arc::new(Header("b"))), IdleTimeout::Default)).await.unwrap();
    let frames = mock.frames();
    assert_eq!(frames.iter().map(|(c, _)| *c).collect::<Vec<_>>(), vec![1, 1, 2]);
    assert_eq!(frames[1].1["previous_response_id"], "resp_1");
    assert!(frames[2].1.get("previous_response_id").is_none());
    assert_eq!(frames[2].1["input"].as_array().unwrap().len(), 5);
    settle().await;
    assert!(mock.log().contains(&Log::Closed { conn: 1, code: Some(1000) }));
}
