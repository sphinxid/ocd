//! Upstream's recorded video tests (`packages/ai/test/provider/*-video.recorded.test.ts`) replayed against local
//! servers. A recording is a sequence of calls (the submit, every status poll, the result read and the download of
//! the finished video, which may be on another host): each recorded origin gets its own server on 127.0.0.1, the
//! servers answer the interactions strictly in order, and each request must match the recorded method, path, query
//! and (JSON) body. Provider URLs in recorded response bodies are rewritten to the matching server, so a download
//! from the provider's CDN reaches the CDN's server.
//!
//! The fixtures under `tests/fixtures/video/` are upstream's, with large base64 payloads (the downloaded videos and
//! Google's inline video `data`) cut to their first 48 bytes, enough for type sniffing. Upstream has no Seedance
//! recording (`seedance-video.recorded.test.ts` has no fixture), so that case is not ported.

#[path = "common/http_mock.rs"]
mod http_mock;

use futures::StreamExt;
use http_mock::Req;
use indexmap::IndexMap;
use oc_ai::json::Object;
use oc_ai::media::{MediaSource, detect_media_type};
use oc_ai::testing::{Recording, recording};
use oc_ai::video::{self, Status, VideoModel, VideoOutput, VideoRequest, VideoStatus, VideoUsage};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/video");

fn load(path: &str) -> Recording {
    recording(&std::fs::read_to_string(format!("{FIXTURES}/{path}")).unwrap())
}

fn origin(url: &str) -> String {
    url::Url::parse(url).unwrap().origin().ascii_serialization()
}

#[derive(Default)]
struct State {
    next: usize,
    requests: Vec<(String, Req)>,
    mismatches: Vec<String>,
}

/// One server per recorded origin, answering the recording's interactions in order.
struct Replay {
    /// Recorded origin → local base URL.
    bases: IndexMap<String, String>,
    state: Arc<Mutex<State>>,
    total: usize,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Replay {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl Replay {
    fn base(&self, recorded: &str) -> &str {
        &self.bases[&origin(recorded)]
    }

    /// Every interaction was asked for, in order, and matched its recording.
    fn finish(&self) {
        let state = self.state.lock();
        assert!(state.mismatches.is_empty(), "{:#?}", state.mismatches);
        assert_eq!(state.next, self.total, "only {} of {} recorded interactions were replayed", state.next, self.total);
    }

    /// The requests that reached the server for `recorded_origin`.
    fn requests_to(&self, recorded_origin: &str) -> Vec<Req> {
        self.state.lock().requests.iter().filter(|(o, _)| o == recorded_origin).map(|(_, r)| r.clone()).collect()
    }
}

fn check(interaction: &oc_ai::testing::Interaction, recorded_origin: &str, request: &Req) -> Result<(), String> {
    let expected = url::Url::parse(&interaction.request.url).unwrap();
    let expected_line = format!(
        "{} {} {}{}",
        interaction.request.method,
        origin(expected.as_str()),
        expected.path(),
        expected.query().map(|q| format!("?{q}")).unwrap_or_default()
    );
    let actual_line = format!(
        "{} {} {}{}",
        request.method,
        recorded_origin,
        request.path,
        if request.query.is_empty() { String::new() } else { format!("?{}", request.query) }
    );
    if expected_line != actual_line {
        return Err(format!("expected {expected_line}, got {actual_line}"));
    }
    let recorded = &interaction.request.body;
    if recorded.is_empty() != request.body.is_empty() {
        return Err(format!("{actual_line}: expected body {recorded:?}, got {:?}", request.body));
    }
    if !recorded.is_empty() {
        let sent: Value = serde_json::from_str(&request.body).map_err(|e| format!("{actual_line}: {e}"))?;
        let want: Value = serde_json::from_str(recorded).unwrap();
        if sent != want {
            return Err(format!("{actual_line}: expected body {want}, got {sent}"));
        }
    }
    Ok(())
}

async fn replay(rec: &Recording) -> Replay {
    let mut listeners = vec![];
    let mut bases = IndexMap::new();
    for interaction in &rec.interactions {
        let o = origin(&interaction.request.url);
        if !bases.contains_key(&o) {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            bases.insert(o.clone(), format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port()));
            listeners.push((o, listener));
        }
    }
    let state = Arc::new(Mutex::new(State::default()));
    let interactions = Arc::new(rec.interactions.clone());
    let rewrites = Arc::new(bases.clone());
    let tasks = listeners
        .into_iter()
        .map(|(recorded_origin, listener)| {
            let state = state.clone();
            let interactions = interactions.clone();
            let rewrites = rewrites.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((mut socket, _)) = listener.accept().await else {
                        return;
                    };
                    let Some(request) = http_mock::read_request(&mut socket).await else {
                        continue;
                    };
                    let (status, headers, body) = {
                        let mut state = state.lock();
                        let index = state.next;
                        state.next += 1;
                        state.requests.push((recorded_origin.clone(), request.clone()));
                        let verdict = match interactions.get(index) {
                            None => Err(format!("unexpected request {} {}", request.method, request.path)),
                            Some(interaction) => check(interaction, &recorded_origin, &request).map(|()| interaction),
                        };
                        match verdict {
                            Err(message) => {
                                state.mismatches.push(format!("interaction {index}: {message}"));
                                (418, vec![("content-type".to_string(), "text/plain".to_string())], message.into_bytes())
                            }
                            Ok(interaction) => {
                                let response = &interaction.response;
                                let mut body = response.bytes();
                                if response.body_encoding.is_none() {
                                    let mut text = response.body.clone();
                                    for (from, to) in rewrites.iter() {
                                        text = text.replace(from.as_str(), to);
                                    }
                                    body = text.into_bytes();
                                }
                                let headers = response
                                    .headers
                                    .iter()
                                    .filter(|(k, _)| k.as_str() != "content-length")
                                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                                    .collect();
                                (response.status, headers, body)
                            }
                        }
                    };
                    let mut head = format!("HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n", body.len());
                    for (k, v) in headers {
                        head.push_str(&format!("{k}: {v}\r\n"));
                    }
                    head.push_str("\r\n");
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(&body).await;
                    let _ = socket.shutdown().await;
                }
            })
        })
        .collect();
    Replay { bases, state, total: rec.interactions.len(), tasks }
}

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

/// `<Provider>.configure({ apiKey: "fixture", baseURL }).video(id)`.
fn model(package: &str, id: &str, base_url: &str) -> VideoModel {
    let settings: Object = json!({ "apiKey": "fixture", "baseURL": base_url }).as_object().unwrap().clone();
    oc_ai::providers::media::video_model(&format!("@opencode/ai/providers/{package}/videos"), id, &settings).unwrap().unwrap()
}

/// Poll the status until the generation leaves `running`, like the recorded tests' `Effect.repeat` (without the
/// interval: the replay answers at once).
async fn settle(model: &VideoModel, id: &str) -> Vec<VideoStatus> {
    let mut observed = vec![];
    for _ in 0..100 {
        let status = video::status(&client(), model, id).await.unwrap_or_else(|e| panic!("status: {e:?}"));
        observed.push(status);
        if status.status != Status::Running {
            return observed;
        }
    }
    panic!("the generation never left running: {observed:?}");
}

async fn bytes(output: &VideoOutput) -> Vec<u8> {
    let mut stream = video::open(&client(), output).await.unwrap();
    let mut out = vec![];
    while let Some(chunk) = stream.next().await {
        out.extend_from_slice(&chunk.unwrap());
    }
    out
}

const BALLOON: &str = "A single red balloon drifting slowly upward against a clear blue sky.";

// provider/xai-video.recorded.test.ts: "generates a video"
#[tokio::test]
async fn xai_generates_a_video() {
    let rec = load("xai-video/generates-a-video.json");
    let server = replay(&rec).await;
    let m = model("xai", "grok-imagine-video-1.5", &format!("{}/v1", server.base("https://api.x.ai")));
    let mut r = VideoRequest::new(m.clone(), BALLOON);
    r.aspect = Some("16:9".into());
    r.resolution = Some("480p".into());
    r.duration = video::parse_duration("2 seconds");
    let id = video::start(&client(), r).await.unwrap();
    let observed = settle(&m, &id).await;
    let progress: Vec<f64> = observed.iter().filter_map(|s| s.progress).collect();
    // xAI reports progress as a percentage; the route normalizes it to a monotonic 0..1 fraction.
    assert!(!progress.is_empty());
    assert!(progress.iter().all(|p| (0.0..=1.0).contains(p)), "{progress:?}");
    assert!(progress.windows(2).all(|w| w[0] <= w[1]), "{progress:?}");
    assert_eq!(observed.last().unwrap().status, Status::Completed);

    let response = video::result(&client(), &m, &id).await.unwrap();
    assert_eq!(response.videos.len(), 1);
    let video = &response.videos[0];
    assert_eq!(video.asset.source_type(), "url");
    assert_eq!(video.asset.info.as_ref().and_then(|i| i.duration_seconds), Some(2.0));
    assert!(!bytes(video).await.is_empty());
    server.finish();
    // The video is on xAI's CDN, another origin than the API, so its download carries no credentials.
    let downloads = server.requests_to("https://vidgen.x.ai");
    assert_eq!(downloads.len(), 1);
    assert_eq!(downloads[0].header("authorization"), None);
    assert_eq!(server.requests_to("https://api.x.ai")[0].header("authorization"), Some("Bearer fixture"));
}

/// The MiniMax recordings share an `https://api.minimax.io` base without a version.
fn minimax(server: &Replay, id: &str) -> VideoModel {
    model("minimax", id, server.base("https://api.minimax.io"))
}

// provider/minimax-video.recorded.test.ts: "generates an H3 video"
#[tokio::test]
async fn minimax_generates_an_h3_video() {
    let rec = load("minimax-video/generates-an-h3-video.json");
    let server = replay(&rec).await;
    let m = minimax(&server, "MiniMax-H3-Lite");
    // H3 needs resolution and duration, and a ratio when the prompt is the only input.
    let mut r = VideoRequest::new(m.clone(), BALLOON);
    r.aspect = Some("16:9".into());
    r.resolution = Some("480p".into());
    r.duration = video::parse_duration("4 seconds");
    let id = video::start(&client(), r).await.unwrap();
    assert_eq!(settle(&m, &id).await.last().unwrap().status, Status::Completed);

    let response = video::result(&client(), &m, &id).await.unwrap();
    assert_eq!(response.videos.len(), 1);
    assert_eq!(response.videos[0].asset.source_type(), "url");
    assert_eq!(response.videos[0].asset.info.as_ref().and_then(|i| i.duration_seconds), Some(4.0));
    assert_eq!(response.usage, Some(VideoUsage::Seconds(4.0)));
    let metadata = &response.provider_metadata.unwrap()["minimax"];
    assert_eq!((&metadata["model"], &metadata["resolution"]), (&json!("MiniMax-H3-Lite"), &json!("480P")));
    server.finish();
}

// provider/minimax-video.recorded.test.ts: "generates a Hailuo video from a first frame"
#[tokio::test]
async fn minimax_generates_a_hailuo_video_from_a_first_frame() {
    let rec = load("minimax-video/generates-a-hailuo-video-from-a-first-frame.json");
    let server = replay(&rec).await;
    let m = minimax(&server, "MiniMax-Hailuo-02");
    let mut r = VideoRequest::new(m.clone(), "The woman slowly turns her head and smiles.");
    // MiniMax's own documentation portrait.
    r.start = Some(oc_ai::MediaAsset::url("https://cdn.hailuoai.com/prod/2025-08-12-17/video_cover/1754990600020238321-411603868533342214-cover.jpg", None));
    r.resolution = Some("512p".into());
    r.duration = video::parse_duration("6 seconds");
    let id = video::start(&client(), r).await.unwrap();
    assert_eq!(settle(&m, &id).await.last().unwrap().status, Status::Completed);

    // The task names a file; the result looks up its one-hour download URL.
    let response = video::result(&client(), &m, &id).await.unwrap();
    assert_eq!(response.videos.len(), 1);
    assert_eq!(response.videos[0].asset.source_type(), "url");
    assert!(response.videos[0].expires_at.unwrap() > 0.0);
    assert_eq!(response.provider_metadata.unwrap()["minimax"]["taskId"], json!(id));
    server.finish();
}

// provider/openrouter-video.recorded.test.ts: "generates a video"
#[tokio::test]
async fn openrouter_generates_a_video() {
    let rec = load("openrouter-video/generates-a-video.json");
    let server = replay(&rec).await;
    let m = model("openrouter", "x-ai/grok-imagine-video-1.5-lite", &format!("{}/api/v1", server.base("https://openrouter.ai")));
    let mut r = VideoRequest::new(m.clone(), BALLOON);
    r.aspect = Some("16:9".into());
    r.resolution = Some("480p".into());
    r.duration = video::parse_duration("1 second");
    let id = video::start(&client(), r).await.unwrap();
    let observed = settle(&m, &id).await;
    // OpenRouter reports no progress, only queued, running and the terminal status.
    assert!(observed.iter().all(|s| s.progress.is_none()));
    assert_eq!(observed.last().unwrap().status, Status::Completed);

    let response = video::result(&client(), &m, &id).await.unwrap();
    assert_eq!(response.videos.len(), 1);
    let video = &response.videos[0];
    assert_eq!(video.asset.source_type(), "url");
    // Date.parse("2026-01-01")
    assert!(video.expires_at.unwrap() > 1_767_225_600_000.0);
    // The content URL only downloads with the API key, which the result attaches to the asset.
    assert_eq!(detect_media_type(&bytes(video).await), Some("video/mp4"));
    server.finish();
    let calls = server.requests_to("https://openrouter.ai");
    assert_eq!(calls.last().unwrap().header("authorization"), Some("Bearer fixture"));
}

// provider/google-interactions-video.recorded.test.ts: "generates a video"
#[tokio::test]
async fn google_interactions_generates_a_video() {
    let rec = load("google-interactions-video/generates-a-video.json");
    let server = replay(&rec).await;
    let base = format!("{}/v1beta", server.base("https://generativelanguage.googleapis.com"));
    let m = model("google", "gemini-omni-1.2-flash", &base);
    let mut r = VideoRequest::new(m.clone(), "A single red balloon drifting slowly upward against a clear blue sky. No dialogue.");
    r.aspect = Some("16:9".into());
    r.resolution = Some("360p".into());
    r.duration = video::parse_duration("3 seconds");
    let id = video::start(&client(), r).await.unwrap();
    assert_eq!(settle(&m, &id).await.last().unwrap().status, Status::Completed);

    let response = video::result(&client(), &m, &id).await.unwrap();
    assert_eq!(response.videos.len(), 1);
    let video = &response.videos[0];
    assert_eq!(video.media_type(), "video/mp4");
    assert!(matches!(video.asset.source, MediaSource::Bytes { .. } | MediaSource::Base64 { .. }));
    assert!(!bytes(video).await.is_empty());
    server.finish();
    assert_eq!(server.requests_to("https://generativelanguage.googleapis.com")[0].header("x-goog-api-key"), Some("fixture"));
}
