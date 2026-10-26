//! `http.encoding` compresses JSON request bodies (upstream `test/request-encoding.test.ts`).

use std::io::Read;

use futures::StreamExt;
use oc_ai::protocols::openai_chat as oc;
use oc_ai::route::auth;
use oc_ai::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const SSE: &str = "data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hi\"}}]}\n\n\
data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";

/// Serves one request; returns its `content-encoding` header and raw body bytes.
async fn serve() -> (String, tokio::task::JoinHandle<(Option<String>, Vec<u8>)>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let (head, length) = loop {
            let n = socket.read(&mut chunk).await.unwrap();
            buf.extend_from_slice(&chunk[..n]);
            if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..end]).to_string();
                let length = head
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                if buf.len() >= end + 4 + length {
                    break (head, length);
                }
            }
            assert!(n > 0, "connection closed early");
        };
        let start = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        let body = buf[start..start + length].to_vec();
        let encoding = head.lines().find_map(|l| {
            l.to_ascii_lowercase()
                .strip_prefix("content-encoding:")
                .map(|v| v.trim().to_string())
        });
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{SSE}",
            SSE.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        socket.shutdown().await.ok();
        (encoding, body)
    });
    (base, handle)
}

async fn send(encoding: Option<RequestEncoding>) -> (Option<String>, Vec<u8>) {
    let (base, server) = serve().await;
    let model = oc::route()
        .base_url(Some(base))
        .auth(auth::none())
        .model("gpt-4.1-mini")
        .unwrap();
    let mut req = LLMRequest::new(model).prompt("Hello");
    req.http = encoding.map(|encoding| HttpOptions {
        encoding: Some(encoding),
        ..Default::default()
    });
    let mut stream = Client::new().stream(req, Default::default()).await.unwrap();
    while let Some(event) = stream.next().await {
        event.unwrap();
    }
    server.await.unwrap()
}

fn model_of(body: &[u8]) -> String {
    let value: serde_json::Value = serde_json::from_slice(body).unwrap();
    value["model"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn compresses_the_json_body_with_the_requested_encoding() {
    let (encoding, body) = send(Some(RequestEncoding::Zstd)).await;
    assert_eq!(encoding.as_deref(), Some("zstd"));
    assert_eq!(
        model_of(&zstd::decode_all(body.as_slice()).unwrap()),
        "gpt-4.1-mini"
    );

    let (encoding, body) = send(Some(RequestEncoding::Gzip)).await;
    assert_eq!(encoding.as_deref(), Some("gzip"));
    let mut plain = vec![];
    flate2::read::GzDecoder::new(body.as_slice())
        .read_to_end(&mut plain)
        .unwrap();
    assert_eq!(model_of(&plain), "gpt-4.1-mini");
}

#[tokio::test]
async fn sends_plain_json_when_unset_or_none() {
    for encoding in [None, Some(RequestEncoding::None)] {
        let (header, body) = send(encoding).await;
        assert_eq!(header, None);
        assert_eq!(model_of(&body), "gpt-4.1-mini");
    }
}
