//! Upstream's recorded image and decision tests (`packages/ai/test/provider/*-images.recorded.test.ts`,
//! `decision.recorded.test.ts`) replayed against a local server: each case builds the model through its package
//! entrypoint pointed at the server, sends the recorded test's request, checks the wire body against the
//! recording and decodes the recorded answer. Large base64 payloads in the fixtures are cut to their first 48
//! bytes (enough for type sniffing), so pixel dimensions are not checked.

#[path = "common/http_mock.rs"]
mod http_mock;

use base64::Engine;
use oc_ai::decision::{self, DecisionPart, DecisionRequest};
use oc_ai::image::{self, ImageRequest};
use oc_ai::media::MediaSource;
use oc_ai::testing::{recording, Recording};
use oc_ai::MediaAsset;
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/media");

fn load(path: &str) -> Recording {
    recording(&std::fs::read_to_string(format!("{FIXTURES}/{path}")).unwrap())
}

fn edit_source() -> MediaAsset {
    MediaAsset::bytes(std::fs::read(format!("{FIXTURES}/edit-source.jpg")).unwrap(), Some("image/jpeg"))
}

/// A server answering every request with the recording's response; it keeps the requests.
struct Replay {
    base: String,
    requests: Arc<Mutex<Vec<http_mock::Req>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Replay {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn replay(rec: &Recording) -> Replay {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let requests = Arc::new(Mutex::new(vec![]));
    let recorded = requests.clone();
    let response = rec.interactions[0].response.clone();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else { return };
            let Some(request) = http_mock::read_request(&mut socket).await else { continue };
            recorded.lock().push(request);
            let body = response.bytes();
            let mut head = format!("HTTP/1.1 {} X\r\ncontent-length: {}\r\nconnection: close\r\n", response.status, body.len());
            for (k, v) in &response.headers {
                if k != "content-length" {
                    head.push_str(&format!("{k}: {}\r\n", v.as_str().unwrap_or_default()));
                }
            }
            head.push_str("\r\n");
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(&body).await;
            let _ = socket.shutdown().await;
        }
    });
    Replay { base, requests, task }
}

fn settings(value: Value) -> serde_json::Map<String, Value> {
    value.as_object().unwrap().clone()
}

/// The recorded URL's path with the account redaction undone the way the upstream test redacts it.
fn recorded_path(rec: &Recording) -> String {
    let url = url::Url::parse(&rec.interactions[0].request.url.replace("{account}", "fixture-account")).unwrap();
    url.path().to_string()
}

/// The JSON body sent must equal the recorded one; multipart bodies (whose recorded field order predates the
/// current code) must carry the same text fields.
fn check_body(rec: &Recording, sent: &http_mock::Req) {
    let recorded = &rec.interactions[0].request.body;
    if sent.header("content-type").is_some_and(|t| t.starts_with("multipart/form-data")) {
        let fields = |body: &str| -> Vec<(String, String)> {
            let mut out: Vec<(String, String)> = body
                .split("\r\n--")
                .filter_map(|part| {
                    let (head, value) = part.split_once("\r\n\r\n")?;
                    let name = head.split("name=\"").nth(1)?.split('"').next()?.to_string();
                    (!head.contains("filename=")).then(|| (name, value.trim_end_matches("\r\n").to_string()))
                })
                .collect();
            out.sort();
            out
        };
        assert_eq!(fields(&sent.body), fields(recorded));
        return;
    }
    let sent: Value = serde_json::from_str(&sent.body).unwrap();
    let recorded: Value = serde_json::from_str(recorded).unwrap();
    assert_eq!(sent, recorded);
}

async fn run_image(dir_file: &str, package: &str, model: &str, base_suffix: &str, extra: Value, build: impl FnOnce(&mut ImageRequest)) -> (image::ImageResponse, Vec<http_mock::Req>) {
    let rec = load(dir_file);
    let server = replay(&rec).await;
    let mut s = settings(json!({ "apiKey": "fixture", "baseURL": format!("{}{base_suffix}", server.base) }));
    for (k, v) in extra.as_object().cloned().unwrap_or_default() {
        s.insert(k, v);
    }
    let model = oc_ai::providers::media::image_model(&format!("@opencode/ai/providers/{package}"), model, &s).unwrap().unwrap();
    let mut request = ImageRequest::new(model, "");
    build(&mut request);
    let response = image::generate(&reqwest::Client::new(), request).await.unwrap_or_else(|e| panic!("{dir_file}: {e:?}"));
    let requests = server.requests.lock().clone();
    assert_eq!(requests.len(), 1, "{dir_file}");
    assert_eq!(requests[0].path, recorded_path(&rec), "{dir_file}");
    check_body(&rec, &requests[0]);
    (response, requests)
}

fn mime(response: &image::ImageResponse) -> String {
    response.images[0].media_type()
}

fn object(value: Value) -> serde_json::Map<String, Value> {
    value.as_object().unwrap().clone()
}

#[tokio::test]
async fn openai_images() {
    let (response, requests) = run_image("openai-images/generates-an-image.json", "openai/images", "gpt-image-1-mini", "/v1", json!({}), |r| {
        r.prompt = "A simple flat black circle centered on a plain white background.".into();
        r.aspect = Some("1:1".into());
        r.provider_options = Some(object(json!({ "outputFormat": "jpeg", "quality": "low", "outputCompression": 10 })));
    })
    .await;
    assert_eq!(requests[0].header("authorization"), Some("Bearer fixture"));
    assert_eq!(mime(&response), "image/jpeg");
    assert_eq!(
        Value::Object(response.provider_metadata.unwrap()),
        json!({ "openai": { "outputFormat": "jpeg", "size": "1024x1024", "quality": "low", "background": "opaque" } })
    );
    let (response, requests) = run_image("openai-images/edits-an-image.json", "openai/images", "gpt-image-1-mini", "/v1", json!({}), |r| {
        r.prompt = "Keep the simple shape and change it from black to bright green.".into();
        r.images = vec![edit_source()];
        r.provider_options = Some(object(json!({ "size": "1024x1024", "outputFormat": "jpeg", "quality": "low", "outputCompression": 10 })));
    })
    .await;
    assert!(requests[0].body.contains("Content-Disposition: form-data; name=\"image[]\"; filename=\"image-0\"\r\nContent-Type: image/jpeg"));
    assert_eq!(mime(&response), "image/jpeg");
}

#[tokio::test]
async fn openrouter_images() {
    let base = "/api/v1";
    let (_, _) = run_image("openrouter-images/renders-gemini-at-the-requested-2k-tier.json", "openrouter/images", "google/gemini-3.1-flash-image", base, json!({}), |r| {
        r.prompt = "A simple flat black circle centered on a plain white background.".into();
        r.aspect = Some("16:9".into());
        r.resolution = Some("2K".into());
    })
    .await;
    let (_, _) = run_image("openrouter-images/renders-flux-3-from-an-aspect-ratio-and-tier.json", "openrouter/images", "black-forest-labs/flux-3-image-klein", base, json!({}), |r| {
        r.prompt = "A simple flat black circle centered on a plain white background.".into();
        r.aspect = Some("16:9".into());
        r.resolution = Some("1K".into());
    })
    .await;
    let (response, _) = run_image("openrouter-images/edits-an-image-through-input-references.json", "openrouter/images", "openai/gpt-image-1-mini", base, json!({}), |r| {
        r.prompt = "Keep the simple shape and change it from black to bright green.".into();
        r.images = vec![edit_source()];
        r.aspect = Some("1:1".into());
        r.resolution = Some("1K".into());
        r.provider_options = Some(object(json!({ "outputFormat": "jpeg", "quality": "low", "outputCompression": 10 })));
    })
    .await;
    assert_eq!(mime(&response), "image/jpeg");
}

#[tokio::test]
async fn vercel_images() {
    let (response, _) = run_image("vercel-ai-gateway-images/generates-an-image.json", "vercel-ai-gateway/images", "meta/muse-image-1.0", "/v1", json!({}), |r| {
        r.prompt = "A simple flat black circle centered on a plain white background.".into();
        r.provider_options = Some(object(json!({ "outputFormat": "webp" })));
    })
    .await;
    assert_eq!(mime(&response), "image/webp");
    assert!(response.usage.unwrap().details.unwrap().contains_key("vercel-ai-gateway"));
    assert!(response.provider_metadata.unwrap().contains_key("vercel-ai-gateway"));
    // Grok ignores `size` behind the Gateway: sizing goes through `providerOptions.xai`.
    run_image("vercel-ai-gateway-images/sizes-grok-through-xai-provider-options.json", "vercel-ai-gateway/images", "spacexai/grok-imagine-image", "/v1", json!({}), |r| {
        r.prompt = "A simple flat black circle centered on a plain white background.".into();
        r.aspect = Some("16:9".into());
        r.resolution = Some("1K".into());
    })
    .await;
}

#[tokio::test]
async fn google_images() {
    let (response, requests) = run_image("google-images/generates-an-image.json", "google/images", "gemini-3.1-flash-image", "/v1beta", json!({}), |r| {
        r.prompt = "A simple flat blue circle centered on a plain white background.".into();
        r.aspect = Some("1:1".into());
    })
    .await;
    assert_eq!(requests[0].header("x-goog-api-key"), Some("fixture"));
    assert_eq!(response.images.len(), 1);
    assert!(mime(&response).starts_with("image/"));
    let (response, _) = run_image("google-images/edits-an-image.json", "google/images", "gemini-3.1-flash-image", "/v1beta", json!({}), |r| {
        r.prompt = "Transform this minimal source into a bright orange sun icon with eight rounded rays on a pale blue background.".into();
        r.images = vec![edit_source()];
        r.aspect = Some("1:1".into());
    })
    .await;
    assert_eq!(mime(&response), "image/jpeg");
}

#[tokio::test]
async fn xai_images() {
    let (response, _) = run_image("xai-images/generates-an-image.json", "xai/images", "grok-imagine-image", "/v1", json!({}), |r| {
        r.prompt = "A simple flat black diamond centered on a plain white background.".into();
        r.aspect = Some("1:1".into());
        r.resolution = Some("1K".into());
    })
    .await;
    assert!(mime(&response).starts_with("image/"));
    let (response, _) = run_image("xai-images/edits-an-image.json", "xai/images", "grok-imagine-image", "/v1", json!({}), |r| {
        r.prompt = "Keep the simple shape and change it from black to bright purple.".into();
        r.images = vec![edit_source()];
        r.aspect = Some("1:1".into());
        r.resolution = Some("1K".into());
    })
    .await;
    assert!(["image/jpeg", "image/png"].contains(&mime(&response).as_str()));
}

#[tokio::test]
async fn zai_images() {
    let (response, _) = run_image("zai-images/generates-an-image.json", "zai/images", "cogview-4-250304", "/api/paas/v4", json!({}), |r| {
        r.prompt = "A simple flat red circle centered on a plain white background.".into();
        r.aspect = Some("1:1".into());
        r.provider_options = Some(object(json!({ "quality": "standard", "userID": "opencode-image-test" })));
    })
    .await;
    assert_eq!(response.images.len(), 1);
    assert_eq!(mime(&response), "application/octet-stream");
    assert!(matches!(&response.images[0].source, MediaSource::Url { url, .. } if url.starts_with("https://")));
    assert!(response.provider_metadata.unwrap().contains_key("zai"));
}

#[tokio::test]
async fn alibaba_images() {
    let (response, _) = run_image("alibaba-images/generates-qwen-image-images.json", "alibaba/images", "qwen-image-2.0", "/api/v1", json!({}), |r| {
        r.prompt = "A simple flat red circle centered on a plain white background.".into();
        r.n = Some(2);
        r.aspect = Some("1:1".into());
        r.resolution = Some("0.5K".into());
        r.provider_options = Some(object(json!({ "seed": 7, "promptExtend": false, "watermark": false })));
    })
    .await;
    assert_eq!(response.images.len(), 2);
    let usage = &response.provider_metadata.unwrap()["alibaba"]["usage"];
    assert_eq!((usage["image_count"].clone(), usage["width"].clone(), usage["height"].clone()), (json!(2), json!(512), json!(512)));
    let (response, _) = run_image("alibaba-images/generates-wan-images.json", "alibaba/images", "wan2.7-image", "/api/v1", json!({}), |r| {
        r.prompt = "A simple flat blue square centered on a plain white background.".into();
        r.n = Some(2);
        r.resolution = Some("0.75K".into());
        r.provider_options = Some(object(json!({ "thinkingMode": false })));
    })
    .await;
    assert_eq!(response.provider_metadata.unwrap()["alibaba"]["usage"]["size"], "768*768");
    let (response, _) = run_image("alibaba-images/edits-an-image.json", "alibaba/images", "qwen-image-2.0", "/api/v1", json!({}), |r| {
        r.prompt = "Keep the simple shape and change it from black to bright purple.".into();
        r.images = vec![edit_source()];
        r.provider_options = Some(object(json!({ "size": "512*512" })));
    })
    .await;
    assert_eq!(response.images[0].source_type(), "url");
}

#[tokio::test]
async fn minimax_images() {
    let (response, _) = run_image("minimax-images/generates-an-image.json", "minimax/images", "image-01", "/v1", json!({}), |r| {
        r.prompt = "A simple flat red circle centered on a plain white background.".into();
        r.aspect = Some("1:1".into());
        r.resolution = Some("0.5K".into());
    })
    .await;
    assert!(["image/jpeg", "image/png"].contains(&mime(&response).as_str()));
    assert!(response.provider_metadata.unwrap().contains_key("minimax"));
    let (response, _) = run_image("minimax-images/generates-from-a-subject-reference.json", "minimax/images", "image-01", "/v1", json!({}), |r| {
        r.prompt = "The same woman reading a book in a quiet library, soft window light.".into();
        r.images = vec![MediaAsset::url(
            "https://cdn.hailuoai.com/prod/2025-08-12-17/video_cover/1754990600020238321-411603868533342214-cover.jpg",
            None,
        )];
        r.resolution = Some("0.5K".into());
    })
    .await;
    assert_eq!(response.images.len(), 1);
}

#[tokio::test]
async fn meta_images() {
    let controls = json!({
        "reasoningStrength": "low",
        "toolEnablement": { "enable_image_search": false, "enable_web_search": false, "enable_shell": false },
        "size": "256x256",
    });
    // Meta's image factory takes no provider options from settings: the request carries them.
    let (response, _) = run_image("meta-images/generates-default-webp-bytes.json", "meta/images", "muse-image-1.0", "/v1", json!({}), |r| {
        r.prompt = "A flat black square centered on a plain white background. No text.".into();
        r.n = Some(1);
        r.provider_options = Some(object(controls.clone()));
    })
    .await;
    assert_eq!(mime(&response), "image/webp");
    assert!(response.usage.unwrap().output.unwrap() > 0.0);
    let mut options = object(controls.clone());
    options.insert("outputFormat".into(), json!("png"));
    let (response, _) = run_image("meta-images/edits-image-bytes-and-returns-png.json", "meta/images", "muse-image-1.0", "/v1", json!({}), |r| {
        r.prompt = "Change the shape to bright purple. Keep the plain white background.".into();
        r.images = vec![edit_source()];
        r.n = Some(1);
        r.provider_options = Some(options);
    })
    .await;
    assert_eq!(mime(&response), "image/png");
}

#[tokio::test]
async fn cloudflare_workers_ai_images() {
    let base = "/client/v4/accounts/fixture-account/ai/v1";
    let (response, _) = run_image("cloudflare-workers-ai-images/generates-a-raw-binary-image.json", "cloudflare-workers-ai/images", "@cf/bytedance/stable-diffusion-xl-lightning", base, json!({}), |r| {
        r.prompt = "A red apple on a wooden table".into();
        r.aspect = Some("1:1".into());
        r.resolution = Some("0.25K".into());
        r.provider_options = Some(object(json!({ "seed": 3, "num_steps": 1 })));
    })
    .await;
    // The Run API labels this response image/png, but the bytes are JPEG.
    assert_eq!(mime(&response), "image/jpeg");
    let (response, requests) = run_image("cloudflare-workers-ai-images/generates-a-flux-2-image-from-multipart-fields.json", "cloudflare-workers-ai/images", "@cf/black-forest-labs/flux-2-klein-4b", base, json!({}), |r| {
        r.prompt = "A red apple on a wooden table".into();
        r.provider_options = Some(object(json!({ "width": 64, "height": 64, "seed": 3, "steps": 1 })));
    })
    .await;
    assert!(requests[0].header("content-type").unwrap().starts_with("multipart/form-data; boundary=----WebKitFormBoundary"));
    assert_eq!(mime(&response), "image/jpeg");
}

// ---------------------------------------------------------------------------------------------
// Decisions
// ---------------------------------------------------------------------------------------------

const TICKET: &str = "I was charged twice for the same invoice. Please refund the duplicate payment today.";

fn questions() -> Vec<serde_json::Map<String, Value>> {
    [
        json!({
            "name": "department", "type": "choice", "instructions": "Which team should handle this support request?",
            "choices": [
                { "value": "billing", "description": "Payments, invoices, refunds, or failed charges" },
                { "value": "technical", "description": "Bugs, outages, or integrations" },
                { "value": "sales", "description": "Pricing, upgrades, or new accounts" },
            ],
        }),
        json!({
            "name": "urgency", "type": "score", "instructions": "How urgent is this support request?",
            "levels": [
                { "label": "Can wait for normal support" },
                { "label": "Needs prompt attention" },
                { "label": "Actively blocking revenue" },
            ],
        }),
        json!({ "name": "refund", "type": "predicate", "instructions": "Is the customer asking for a refund?" }),
    ]
    .into_iter()
    .map(object)
    .collect()
}

/// `assertDecision`.
fn assert_decision(response: &decision::DecisionResponse) {
    let [department, urgency, refund] = [&response.answers[0], &response.answers[1], &response.answers[2]];
    assert_eq!(department["choice"], "billing");
    let billing = department["probabilities"].as_array().unwrap().iter().find(|p| p["value"] == "billing").unwrap();
    assert!(billing["probability"].as_f64().unwrap() > 0.5);
    assert!(department["confidence"].as_f64().unwrap() > 0.0);
    let labels: Vec<&str> = urgency["probabilities"].as_array().unwrap().iter().map(|p| p["label"].as_str().unwrap()).collect();
    assert_eq!(labels, ["Can wait for normal support", "Needs prompt attention", "Actively blocking revenue"]);
    let score = urgency["score"].as_f64().unwrap();
    assert!((0.0..=2.0).contains(&score));
    assert!(urgency["confidence"].as_f64().unwrap() > 0.0);
    assert!(refund["probability"].as_f64().unwrap() > 0.5);
    assert!(response.usage.as_ref().unwrap().input_tokens.unwrap() > 0.0);
}

async fn run_decision(dir_file: &str, model: impl FnOnce(&str) -> decision::DecisionModel, input: Vec<DecisionPart>) -> decision::DecisionResponse {
    let rec = load(dir_file);
    let server = replay(&rec).await;
    let request = DecisionRequest { model: model(&server.base), input, questions: questions(), provider_options: None, http: None };
    let response = decision::generate(&reqwest::Client::new(), request).await.unwrap_or_else(|e| panic!("{dir_file}: {e:?}"));
    let requests = server.requests.lock().clone();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, recorded_path(&rec), "{dir_file}");
    check_body(&rec, &requests[0]);
    assert_decision(&response);
    response
}

fn package(package: &str, model: &str, settings: Value) -> decision::DecisionModel {
    oc_ai::providers::media::decision_model(&format!("@opencode/ai/providers/{package}"), model, &object(settings)).unwrap().unwrap()
}

fn ticket() -> Vec<DecisionPart> {
    vec![DecisionPart::Text(TICKET.into())]
}

#[tokio::test]
async fn system_one_decisions() {
    for (file, pkg, model, suffix) in [
        ("typesafe-decision", "typesafe-ai/decisions", "jev-latest", "/v1"),
        ("opencode-decision", "opencode/decisions", "jev-1.13-free", "/inference/systemone/v1"),
        ("openrouter-decision", "openrouter/decisions", "typesafe/jev-1.13", "/api/v1"),
        ("baseten-decision", "baseten/decisions", "decision-models/baseten-elephant-9b-latest", "/v1"),
        ("digitalocean-decision", "digitalocean/decisions", "typesafe-jev-1.13.0", "/v1"),
    ] {
        let rec = format!("{file}/decides-choice-score-and-predicate-questions.json");
        run_decision(&rec, |base| package(pkg, model, json!({ "apiKey": "fixture", "baseURL": format!("{base}{suffix}") })), ticket()).await;
    }
}

#[tokio::test]
async fn cloudflare_decisions() {
    let base = "/client/v4/accounts/fixture-account/ai/v1";
    run_decision(
        "cloudflare-workers-ai-decision/decides-choice-score-and-predicate-questions.json",
        |b| package("cloudflare-workers-ai/decisions", "@cf/cloudflare/clef", json!({ "apiKey": "fixture", "baseURL": format!("{b}{base}") })),
        ticket(),
    )
    .await;
    run_decision(
        "cloudflare-ai-gateway-decision/decides-choice-score-and-predicate-questions.json",
        |b| package("cloudflare-ai-gateway/decisions", "typesafe/jev", json!({ "gatewayApiKey": "fixture", "baseURL": format!("{b}{base}") })),
        ticket(),
    )
    .await;
}

#[tokio::test]
async fn openai_decisions() {
    // `OpenAI.configure(...).decision(...)`: no package entrypoint exports it, so the route is built directly.
    let response = run_decision(
        "openai-decision/decides-choice-score-and-predicate-questions-about-json-state.json",
        |base| {
            decision::openai::route()
                .with_base_url(Some(format!("{base}/v1")))
                .with_auth(oc_ai::route::auth::bearer_options(Some("fixture".into()), &[]))
                .model("gpt-6-luna", None, Some(serde_json::Map::new()))
        },
        vec![DecisionPart::Json(json!({ "ticket": TICKET, "plan": "pro" }))],
    )
    .await;
    let openai = &response.provider_metadata.as_ref().unwrap()["openai"];
    assert_eq!(openai["model"], "gpt-6-luna");
    assert_eq!(openai["usage"]["input_tokens"], 430);
    assert_eq!(response.usage.unwrap().output_tokens, Some(0.0));
}

/// URL results are downloaded when materialized, and their type comes from the bytes.
#[tokio::test]
async fn materializes_url_results() {
    let rec = load("zai-images/generates-an-image.json");
    let server = replay(&Recording {
        version: 1,
        metadata: Value::Null,
        interactions: vec![oc_ai::testing::Interaction {
            transport: None,
            request: rec.interactions[0].request.clone(),
            response: oc_ai::testing::RecordedResponse {
                status: 200,
                headers: object(json!({ "content-type": "application/octet-stream" })),
                body: base64::engine::general_purpose::STANDARD.encode(b"GIF89a\x01\x00\x01\x00"),
                body_encoding: Some("base64".into()),
            },
        }],
    })
    .await;
    let asset = MediaAsset::url(format!("{}/out", server.base), None);
    let (data, mime) = image::materialize(&reqwest::Client::new(), &asset).await.unwrap();
    assert_eq!(mime, "image/gif");
    assert_eq!(data, base64::engine::general_purpose::STANDARD.encode(b"GIF89a\x01\x00\x01\x00"));
}
