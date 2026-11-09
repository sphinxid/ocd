//! Image and decision protocol behaviour, ported from upstream's `packages/ai/test/image.test.ts` and
//! `decision.test.ts`: request lowering per host (sizes, image inputs, native options), response decoding and the
//! typed failures that stop a request before it is sent. Every URL points at a local server.

#[path = "common/http_mock.rs"]
mod http_mock;

use http_mock::{mock, Req, Resp};
use oc_ai::decision::{self, DecisionPart, DecisionRequest};
use oc_ai::image::{self, ImageRequest};
use oc_ai::media::MediaSource;
use oc_ai::{ErrorKind, MediaAsset};
use serde_json::{json, Map, Value};

fn object(value: Value) -> Map<String, Value> {
    value.as_object().unwrap().clone()
}

fn png() -> MediaAsset {
    MediaAsset::bytes(vec![1, 2, 3], Some("image/png"))
}

fn image_model(package: &str, id: &str, settings: Value) -> image::ImageModel {
    oc_ai::providers::media::image_model(&format!("@opencode/ai/providers/{package}"), id, &object(settings)).unwrap().unwrap()
}

fn decision_model(package: &str, id: &str, settings: Value) -> decision::DecisionModel {
    oc_ai::providers::media::decision_model(&format!("@opencode/ai/providers/{package}"), id, &object(settings)).unwrap().unwrap()
}

fn ok_images() -> Resp {
    Resp::json(200, json!({ "data": [{ "b64_json": "AQID" }] }))
}

async fn generate(request: ImageRequest) -> Result<image::ImageResponse, oc_ai::AIError> {
    image::generate(&reqwest::Client::new(), request).await
}

fn request(model: image::ImageModel, prompt: &str, build: impl FnOnce(&mut ImageRequest)) -> ImageRequest {
    let mut r = ImageRequest::new(model, prompt);
    build(&mut r);
    r
}

fn only(requests: Vec<Req>) -> Req {
    assert_eq!(requests.len(), 1, "{requests:?}");
    requests.into_iter().next().unwrap()
}

#[tokio::test]
async fn lowers_aspect_to_the_nearest_openai_size() {
    let server = mock(|_, _| ok_images()).await;
    let openai = |id: &str| image_model("openai/images", id, json!({ "apiKey": "test", "baseURL": format!("{}/v1", server.base) }));
    generate(request(openai("gpt-image-1.5"), "A lighthouse", |r| r.aspect = Some("16:9".into()))).await.unwrap();
    assert_eq!(only(server.requests()).json(), json!({ "model": "gpt-image-1.5", "prompt": "A lighthouse", "size": "1536x1024" }));
    let conflict = generate(request(openai("gpt-image-2"), "A lighthouse", |r| {
        r.aspect = Some("16:9".into());
        r.provider_options = Some(object(json!({ "size": "1024x1024" })));
    }))
    .await
    .unwrap_err();
    assert_eq!(conflict.tag(), "InvalidRequest");
}

#[tokio::test]
async fn computes_a_free_size_for_gateway_hosted_models() {
    let server = mock(|_, _| ok_images()).await;
    let model = image_model("vercel-ai-gateway/images", "bfl/flux-2-klein-4b", json!({ "apiKey": "test", "baseURL": format!("{}/v1", server.base) }));
    generate(request(model, "A lighthouse", |r| r.aspect = Some("16:9".into()))).await.unwrap();
    assert_eq!(only(server.requests()).json(), json!({ "model": "bfl/flux-2-klein-4b", "prompt": "A lighthouse", "size": "1360x768" }));
}

#[tokio::test]
async fn gateway_and_google_sizing() {
    let server = mock(|req, _| {
        if req.path.contains(":generateContent") {
            return Resp::json(200, json!({ "candidates": [{ "content": { "parts": [{ "inlineData": { "mimeType": "image/png", "data": "AQID" } }] } }] }));
        }
        ok_images()
    })
    .await;
    let openrouter = |id: &str| image_model("openrouter/images", id, json!({ "apiKey": "test", "baseURL": format!("{}/v1", server.base) }));
    generate(request(openrouter("google/gemini-3.1-flash-image"), "A lighthouse", |r| {
        r.aspect = Some("16:9".into());
        r.resolution = Some("0.5K".into());
    }))
    .await
    .unwrap();
    generate(request(openrouter("x/y"), "A lighthouse", |r| {
        r.aspect = Some("1:1".into());
        r.resolution = Some("8K".into());
    }))
    .await
    .unwrap();
    for id in ["spacexai/grok-imagine-image", "xai/grok-imagine-image"] {
        let model = image_model("vercel-ai-gateway/images", id, json!({ "apiKey": "test", "baseURL": format!("{}/v1", server.base) }));
        generate(request(model, "A lighthouse", |r| {
            r.aspect = Some("16:9".into());
            r.resolution = Some("2K".into());
        }))
        .await
        .unwrap();
    }
    let google = image_model("google/images", "gemini-3.1-flash-image", json!({ "apiKey": "test", "baseURL": format!("{}/v1beta", server.base) }));
    generate(request(google, "A lighthouse", |r| {
        r.aspect = Some("1:1".into());
        r.resolution = Some("0.5K".into());
    }))
    .await
    .unwrap();
    let bodies: Vec<Value> = server.requests().iter().map(Req::json).collect();
    assert_eq!(bodies[0], json!({ "model": "google/gemini-3.1-flash-image", "prompt": "A lighthouse", "aspect_ratio": "16:9", "resolution": "512" }));
    assert_eq!(bodies[1], json!({ "model": "x/y", "prompt": "A lighthouse", "aspect_ratio": "1:1" }));
    for (body, id) in bodies[2..4].iter().zip(["spacexai/grok-imagine-image", "xai/grok-imagine-image"]) {
        assert_eq!(body, &json!({ "model": id, "prompt": "A lighthouse", "providerOptions": { "xai": { "aspect_ratio": "16:9", "resolution": "2k" } } }));
    }
    assert_eq!(bodies[4]["generationConfig"]["imageConfig"], json!({ "aspectRatio": "1:1", "imageSize": "512" }));
}

#[tokio::test]
async fn preserves_native_snake_case_and_unknown_request_options() {
    let server = mock(|_, _| Resp::json(200, json!({ "data": [{ "b64_json": "AQID" }], "output_format": "avif" }))).await;
    let model = image_model("openai/images", "future-image-model", json!({ "apiKey": "test", "baseURL": format!("{}/v1", server.base) }));
    let response = generate(request(model, "A lighthouse in fog", |r| {
        r.provider_options = Some(object(json!({
            "outputFormat": "jpeg", "output_format": "avif", "outputCompression": 30, "output_compression": 40,
            "provider_future_option": { "enabled": true },
        })));
    }))
    .await
    .unwrap();
    assert_eq!(response.images[0].media_type(), "image/avif");
    assert_eq!(
        only(server.requests()).json(),
        json!({ "model": "future-image-model", "prompt": "A lighthouse in fog", "output_format": "avif", "output_compression": 40, "provider_future_option": { "enabled": true } })
    );
}

#[tokio::test]
async fn routes_openai_byte_inputs_and_masks_through_multipart_edits() {
    let server = mock(|_, _| ok_images()).await;
    let model = image_model("openai/images", "future-model", json!({ "apiKey": "test", "baseURL": format!("{}/v1", server.base) }));
    let mut http = oc_ai::HttpOptions::default();
    http.body = Some(object(json!({ "quality": "low", "model": "corrupt", "prompt": "corrupt", "image": "corrupt", "image[]": "corrupt" })));
    http.headers = Some([("content-type".to_string(), "application/json".to_string())].into_iter().collect());
    generate(request(model, "Combine these images", |r| {
        r.images = vec![png(), oc_ai::media::parse_data_url("data:image/jpeg;base64,BAUG").unwrap()];
        r.provider_options = Some(object(json!({
            "mask": { "source": { "type": "bytes", "data": "BwgJ", "mediaType": "image/png" } },
            "quality": "high", "future_option": true,
        })));
        r.http = Some(http);
    }))
    .await
    .unwrap();
    let sent = only(server.requests());
    assert_eq!(sent.path, "/v1/images/edits");
    assert!(sent.header("content-type").unwrap().starts_with("multipart/form-data; boundary="));
    assert!(sent.body.contains("name=\"model\"\r\n\r\nfuture-model"));
    assert!(sent.body.contains("name=\"prompt\"\r\n\r\nCombine these images"));
    assert_eq!(sent.body.matches("name=\"image[]\"").count(), 2);
    assert!(sent.body.contains("name=\"mask\""));
    assert!(sent.body.contains("name=\"quality\"\r\n\r\nlow"));
    assert!(!sent.body.contains("corrupt"));
}

#[tokio::test]
async fn routes_openai_url_and_file_inputs_through_json_edits() {
    let server = mock(|_, _| ok_images()).await;
    let model = image_model("openai/images", "future-model", json!({ "apiKey": "test", "baseURL": format!("{}/v1", server.base) }));
    let mut http = oc_ai::HttpOptions::default();
    http.body = Some(object(json!({ "future_option": true })));
    generate(request(model, "Combine these images", |r| {
        r.images = vec![MediaAsset::url("https://example.test/source.png", None), MediaAsset::reference("openai", "file_123", None)];
        r.provider_options = Some(object(json!({ "mask": { "source": { "type": "ref", "provider": "openai", "id": "file_mask" } } })));
        r.http = Some(http);
    }))
    .await
    .unwrap();
    assert_eq!(
        only(server.requests()).json(),
        json!({
            "model": "future-model", "prompt": "Combine these images",
            "images": [{ "image_url": "https://example.test/source.png" }, { "file_id": "file_123" }],
            "mask": { "file_id": "file_mask" }, "future_option": true,
        })
    );
}

#[tokio::test]
async fn openrouter_image_inputs() {
    let server = mock(|_, _| ok_images()).await;
    let openrouter = || image_model("openrouter/images", "openai/gpt-image-1-mini", json!({ "apiKey": "test", "baseURL": format!("{}/v1", server.base) }));
    generate(request(openrouter(), "A lighthouse", |_| {})).await.unwrap();
    generate(request(openrouter(), "Combine these images", |r| {
        r.images = vec![png(), MediaAsset::url("https://example.test/source.png", None)];
        r.provider_options = Some(object(json!({ "quality": "low" })));
    }))
    .await
    .unwrap();
    let sent = server.requests();
    assert!(sent.iter().all(|r| r.path == "/v1/images/generations" && r.header("content-type") == Some("application/json")));
    assert_eq!(sent[0].json(), json!({ "model": "openai/gpt-image-1-mini", "prompt": "A lighthouse" }));
    assert_eq!(
        sent[1].json(),
        json!({
            "model": "openai/gpt-image-1-mini", "prompt": "Combine these images",
            "input_references": [
                { "type": "image_url", "image_url": { "url": "data:image/png;base64,AQID" } },
                { "type": "image_url", "image_url": { "url": "https://example.test/source.png" } },
            ],
            "quality": "low",
        })
    );
    let error = generate(request(openrouter(), "Paint the masked area", |r| {
        r.images = vec![png()];
        r.provider_options = Some(object(json!({ "mask": { "source": { "type": "bytes", "data": "BwgJ", "mediaType": "image/png" } } })));
    }))
    .await
    .unwrap_err();
    assert_eq!(error.tag(), "UnsupportedOperation");
    assert!(error.message.contains("openrouter image edits take no mask"));
    assert_eq!(server.requests().len(), 2);
}

#[tokio::test]
async fn sends_openai_edits_as_json_to_the_chatgpt_backend() {
    // `https://chatgpt.com/` itself is matched by prefix; the request goes to the local server through `http.query`
    // free routing, so only the lowering is checked here.
    let model = image_model("openai/images", "gpt-image-2", json!({ "apiKey": "test", "baseURL": "https://chatgpt.com/backend-api/codex" }));
    let request = request(model.clone(), "Add a red cap", |r| r.images = vec![png()]);
    let body = image::body(&request).unwrap();
    assert_eq!(
        Value::Object(body.json_value().unwrap().clone()),
        json!({ "model": "gpt-image-2", "prompt": "Add a red cap", "images": [{ "image_url": "data:image/png;base64,AQID" }] })
    );
    let url = model.route.endpoint.render(&image::resolve(&request)).unwrap();
    assert_eq!(url.as_str(), "https://chatgpt.com/backend-api/codex/images/edits");
}

#[tokio::test]
async fn routes_xai_image_inputs() {
    let server = mock(|_, _| Resp::json(200, json!({ "data": [{ "b64_json": "AQID", "mime_type": "image/png" }] }))).await;
    let xai = || image_model("xai/images", "future-model", json!({ "apiKey": "test", "baseURL": format!("{}/v1", server.base) }));
    generate(request(xai(), "Combine these images", |r| {
        r.images = vec![png(), MediaAsset::url("https://example.test/source.jpg", None), MediaAsset::reference("xai", "file_123", None)];
    }))
    .await
    .unwrap();
    generate(request(xai(), "Edit this image", |r| r.images = vec![MediaAsset::reference("xai", "file_123", None)])).await.unwrap();
    let sent = server.requests();
    assert_eq!(sent[0].path, "/v1/images/edits");
    assert_eq!(
        sent[0].json(),
        json!({
            "model": "future-model", "prompt": "Combine these images",
            "images": [
                { "url": "data:image/png;base64,AQID", "type": "image_url" },
                { "url": "https://example.test/source.jpg", "type": "image_url" },
                { "file_id": "file_123" },
            ],
            "response_format": "b64_json",
        })
    );
    assert_eq!(sent[1].json(), json!({ "model": "future-model", "prompt": "Edit this image", "image": { "file_id": "file_123" }, "response_format": "b64_json" }));
}

#[tokio::test]
async fn decodes_xai_url_images_and_rejects_items_without_data() {
    let server = mock(|_, n| {
        if n == 0 {
            Resp::json(200, json!({ "data": [{ "url": "https://xai.test/a.png", "mime_type": "image/png" }] }))
        } else {
            Resp::json(200, json!({ "data": [{}] }))
        }
    })
    .await;
    let xai = || image_model("xai/images", "future-model", json!({ "apiKey": "test", "baseURL": format!("{}/v1", server.base) }));
    let response = generate(request(xai(), "A kite", |_| {})).await.unwrap();
    assert!(matches!(&response.images[0].source, MediaSource::Url { url, media_type: Some(m), .. } if url == "https://xai.test/a.png" && m == "image/png"));
    let error = generate(request(xai(), "A kite", |_| {})).await.unwrap_err();
    assert_eq!(error.tag(), "InvalidProviderOutput");
    assert!(error.message.contains("xAI Images result 0 has neither image data nor a URL"));
}

#[tokio::test]
async fn lowers_google_image_inputs_into_parts() {
    let server = mock(|_, _| {
        Resp::json(200, json!({ "candidates": [{ "content": { "parts": [{ "inlineData": { "mimeType": "image/png", "data": "AQID" } }] } }] }))
    })
    .await;
    let model = image_model("google/images", "future-model", json!({ "apiKey": "test", "baseURL": format!("{}/v1beta", server.base) }));
    generate(request(model, "Combine these images", |r| {
        r.images = vec![
            png(),
            oc_ai::media::parse_data_url("data:image/jpeg;base64,BAUG").unwrap(),
            MediaAsset::reference("google", "https://generativelanguage.googleapis.com/v1beta/files/123", Some("image/webp".into())),
        ];
    }))
    .await
    .unwrap();
    let sent = only(server.requests());
    assert_eq!(sent.path, "/v1beta/models/future-model:generateContent");
    assert_eq!(
        sent.json()["contents"][0]["parts"],
        json!([
            { "text": "Combine these images" },
            { "inlineData": { "mimeType": "image/png", "data": "AQID" } },
            { "inlineData": { "mimeType": "image/jpeg", "data": "BAUG" } },
            { "fileData": { "mimeType": "image/webp", "fileUri": "https://generativelanguage.googleapis.com/v1beta/files/123" } },
        ])
    );
}

#[tokio::test]
async fn rejects_what_a_route_cannot_honor_before_sending() {
    let google = || image_model("google/images", "gemini-3.1-flash-image", json!({ "apiKey": "test", "baseURL": "http://127.0.0.1:9/v1beta" }));
    let zai = image_model("zai/images", "model", json!({ "apiKey": "test", "baseURL": "http://127.0.0.1:9/v1" }));
    let url = generate(request(google(), "edit", |r| r.images = vec![MediaAsset::url("https://example.test/image.png", None)])).await.unwrap_err();
    assert_eq!(url.tag(), "InvalidRequest");
    let images = generate(request(zai, "edit", |r| r.images = vec![png()])).await.unwrap_err();
    assert!(matches!(&images.kind, ErrorKind::UnsupportedOperation { operation, .. } if operation == "media.images"));
    assert_eq!(images.message, "zai/zai-images does not support images");
    let n = generate(request(google(), "A lighthouse", |r| r.n = Some(2))).await.unwrap_err();
    assert!(matches!(&n.kind, ErrorKind::UnsupportedOperation { operation, .. } if operation == "media.n"));
}

#[tokio::test]
async fn reports_a_google_safety_stop_as_a_content_policy_refusal() {
    let server = mock(|_, _| Resp::json(200, json!({ "candidates": [{ "finishReason": "IMAGE_SAFETY", "content": { "parts": [] } }] }))).await;
    let model = image_model("google/images", "m", json!({ "apiKey": "test", "baseURL": format!("{}/v1beta", server.base) }));
    let error = generate(request(model, "x", |_| {})).await.unwrap_err();
    assert_eq!(error.tag(), "ContentPolicy");
    assert_eq!(error.message, "Google Images withheld its images (finish reasons: IMAGE_SAFETY)");
}

#[tokio::test]
async fn maps_minimax_status_codes() {
    let codes = [(1002, "RateLimit"), (1004, "Authentication"), (1008, "QuotaExceeded"), (1026, "ContentPolicy"), (2013, "InvalidRequest"), (9999, "ProviderInternal")];
    let server = mock(move |_, n| Resp::json(200, json!({ "base_resp": { "status_code": codes[n].0, "status_msg": "nope" } }))).await;
    for (code, tag) in codes {
        let model = image_model("minimax/images", "image-01", json!({ "apiKey": "test", "baseURL": format!("{}/v1", server.base) }));
        let error = generate(request(model, "x", |_| {})).await.unwrap_err();
        assert_eq!(error.tag(), tag, "{code}");
        assert_eq!(error.message, format!("MiniMax Images failed ({code}): nope"));
    }
}

#[tokio::test]
async fn minimax_presets_and_sizes() {
    let model = |id: &str| image_model("minimax/images", id, json!({ "apiKey": "test" }));
    let body = |id: &str, aspect: Option<&str>, resolution: Option<&str>| {
        image::body(&request(model(id), "x", |r| {
            r.aspect = aspect.map(String::from);
            r.resolution = resolution.map(String::from);
        }))
    };
    let fields = |b: Result<oc_ai::route::media::MediaBody, oc_ai::AIError>| Value::Object(b.unwrap().json_value().unwrap().clone());
    assert_eq!(fields(body("image-01", Some("16:9"), None))["aspect_ratio"], "16:9");
    let free = fields(body("image-01", Some("16:9"), Some("2K")));
    assert_eq!((free["width"].clone(), free["height"].clone()), (json!(2048), json!(1152)));
    let error = body("image-01-live", Some("21:9"), None).unwrap_err();
    assert_eq!(
        error.message,
        "image-01-live can't render 21:9; the closest size it takes is 1280x720 (16:9). Change aspect or resolution, or set aspect_ratio in providerOptions"
    );
}

// ---------------------------------------------------------------------------------------------
// Decisions
// ---------------------------------------------------------------------------------------------

fn questions() -> Vec<Map<String, Value>> {
    [
        json!({ "name": "department", "type": "choice", "instructions": "Which team should handle this?",
                "choices": [{ "value": "billing", "description": "Payments and refunds" }, { "value": "technical" }] }),
        json!({ "name": "urgency", "type": "score", "instructions": "How urgent is this?",
                "levels": [{ "label": "Can wait" }, { "label": "Needs attention", "description": "Same day" }, { "label": "Blocking" }] }),
        json!({ "name": "refund", "type": "predicate", "instructions": "Is the customer asking for a refund?" }),
    ]
    .into_iter()
    .map(object)
    .collect()
}

fn expected_answers() -> Value {
    json!([
        { "name": "department", "type": "choice", "choice": "billing",
          "probabilities": [{ "value": "billing", "probability": 0.9 }, { "value": "technical", "probability": 0.1 }], "confidence": 0.8 },
        { "name": "urgency", "type": "score", "score": 1.2,
          "probabilities": [
              { "value": 0, "label": "Can wait", "probability": 0 },
              { "value": 1, "label": "Needs attention", "probability": 0.8 },
              { "value": 2, "label": "Blocking", "probability": 0.2 }],
          "confidence": 0.6 },
        { "name": "refund", "type": "predicate", "probability": 0.97 },
    ])
}

fn system_one_answers() -> Value {
    json!({
        "department": { "type": "choice", "choice": "billing", "probabilities": { "technical": 0.1, "billing": 0.9 }, "confidence": 0.8 },
        "urgency": { "type": "score", "score": 1.2, "probabilities": { "2": 0.2, "0": 0, "1": 0.8 },
                     "legend": { "0": "Can wait", "1": "Needs attention: Same day", "2": "Blocking" }, "confidence": 0.6 },
        "refund": { "type": "noul", "noul": 0.97 },
    })
}

fn answers(response: &decision::DecisionResponse) -> Value {
    Value::Array(response.answers.iter().cloned().map(Value::Object).collect())
}

async fn decide(model: decision::DecisionModel, input: Vec<DecisionPart>, questions: Vec<Map<String, Value>>, build: impl FnOnce(&mut DecisionRequest)) -> Result<decision::DecisionResponse, oc_ai::AIError> {
    let mut request = DecisionRequest { model, input, questions, provider_options: None, http: None };
    build(&mut request);
    decision::generate(&reqwest::Client::new(), request).await
}

#[tokio::test]
async fn decides_through_system_one_with_typesafe_image_assets() {
    let server = mock(|_, _| Resp::json(200, json!({ "model": "jev-1.14.0", "answers": system_one_answers(), "usage": { "input_tokens": 30, "output_tokens": 6 } }))).await;
    let settings = json!({ "apiKey": "test", "baseURL": format!("{}/v1/", server.base), "headers": { "x-default": "yes" }, "body": { "deployment": "test" } });
    let mut model = decision_model("typesafe-ai/decisions", "jev-latest", settings);
    let mut route = (*model.route).clone();
    route.http = oc_ai::HttpOptions::merge(&[route.http.as_ref(), Some(&oc_ai::HttpOptions { query: Some([("api".to_string(), "v1".to_string())].into_iter().collect()), ..Default::default() })]);
    model.http = route.http.clone();
    model.route = std::sync::Arc::new(route);
    let response = decide(
        model,
        vec![DecisionPart::Text("Ticket:".into()), DecisionPart::Json(json!({ "ticket": "Please refund the duplicate charge." })), DecisionPart::Media { media: png(), filename: None }],
        questions(),
        |r| {
            r.provider_options = Some(object(json!({ "trace": { "enabled": true } })));
            r.http = Some(oc_ai::HttpOptions {
                body: Some(object(json!({ "request_metadata": "value" }))),
                headers: Some([("x-request".to_string(), "yes".to_string())].into_iter().collect()),
                query: Some([("trace".to_string(), "1".to_string())].into_iter().collect()),
                ..Default::default()
            });
        },
    )
    .await
    .unwrap();
    assert_eq!(answers(&response), expected_answers());
    assert_eq!(response.usage.as_ref().unwrap().total_tokens, Some(36.0));
    assert_eq!(Value::Object(response.provider_metadata.unwrap()), json!({ "typesafe": { "model": "jev-1.14.0", "usage": { "input_tokens": 30, "output_tokens": 6 } } }));
    let sent = only(server.requests());
    assert_eq!((sent.path.as_str(), sent.query.as_str()), ("/v1/systemone", "api=v1&trace=1"));
    assert_eq!(sent.header("authorization"), Some("Bearer test"));
    assert_eq!(sent.header("x-default"), Some("yes"));
    assert_eq!(sent.header("x-request"), Some("yes"));
    assert_eq!(sent.header("typesafe-danger-zone-images"), Some("true"));
    assert_eq!(
        sent.json(),
        json!({
            "deployment": "test", "request_metadata": "value", "trace": { "enabled": true }, "model": "jev-latest",
            "state": ["Ticket:", { "ticket": "Please refund the duplicate charge." }, "<image-1>"],
            "assets": { "<image-1>": { "type": "image", "format": "png", "data_base64": "AQID" } },
            "questions": {
                "department": { "type": "choice", "instructions": "Which team should handle this?", "criteria": { "billing": "Payments and refunds", "technical": null } },
                "urgency": { "type": "score", "instructions": "How urgent is this?", "criteria": ["Can wait", "Needs attention: Same day", "Blocking"] },
                "refund": { "type": "noul", "instructions": "Is the customer asking for a refund?" },
            },
        })
    );
}

#[tokio::test]
async fn sends_a_lone_input_object_and_maps_boolean_choices_back() {
    let server = mock(|_, _| {
        Resp::json(200, json!({ "model": "jev-1.13.0", "answers": { "greeting": { "type": "choice", "choice": "true", "probabilities": { "false": 0.01, "true": 0.99 }, "confidence": 1 } },
                                "usage": { "input_tokens": 10, "output_tokens": 2, "cost": 0.0004 } }))
    })
    .await;
    let model = decision_model("opencode/decisions", "jev-1.13", json!({ "apiKey": "opencode-key", "baseURL": format!("{}/v1", server.base) }));
    let question = object(json!({ "name": "greeting", "type": "choice", "instructions": "Is this a greeting?", "choices": [{ "value": true }, { "value": false }] }));
    let response = decide(model, vec![DecisionPart::Json(json!({ "ticket": "hello" }))], vec![question], |_| {}).await.unwrap();
    assert_eq!(answers(&response)[0]["choice"], json!(true));
    assert_eq!(answers(&response)[0]["probabilities"], json!([{ "value": true, "probability": 0.99 }, { "value": false, "probability": 0.01 }]));
    assert_eq!(response.usage, Some(decision::DecisionUsage { input_tokens: Some(10.0), output_tokens: Some(2.0), total_tokens: Some(12.0), cost: Some(0.0004) }));
    let sent = only(server.requests());
    assert_eq!(sent.path, "/v1/systemone");
    assert_eq!(sent.header("authorization"), Some("Bearer opencode-key"));
    assert_eq!(
        sent.json(),
        json!({ "model": "jev-1.13", "state": { "ticket": "hello" }, "questions": { "greeting": { "type": "choice", "instructions": "Is this a greeting?", "criteria": { "true": null, "false": null } } } })
    );
}

#[tokio::test]
async fn rejects_choices_system_one_cannot_tell_apart_and_duplicate_names() {
    let model = || decision_model("typesafe-ai/decisions", "jev-latest", json!({ "baseURL": "http://127.0.0.1:9/v1" }));
    let question = object(json!({ "name": "ok", "type": "choice", "instructions": "OK?", "choices": [{ "value": true }, { "value": "true" }] }));
    let error = decide(model(), vec![DecisionPart::Json(json!({ "ticket": "hello" }))], vec![question], |_| {}).await.unwrap_err();
    assert_eq!(error.tag(), "InvalidRequest");
    assert!(error.message.contains("\"ok\""));
    let same = |instructions: &str| object(json!({ "name": "same", "type": "predicate", "instructions": instructions }));
    let error = decide(model(), vec![DecisionPart::Text("hello".into())], vec![same("One?"), same("Two?")], |_| {}).await.unwrap_err();
    assert_eq!((error.tag(), error.message.as_str()), ("InvalidRequest", "Decision question names must be unique"));
}

#[tokio::test]
async fn places_media_in_the_openrouter_state_and_downloads_url_media_first() {
    let server = mock(|req, _| {
        if req.method == "GET" {
            return Resp { status: 200, headers: vec![("content-type".into(), "image/png".into())], body: "\u{1}\u{2}\u{3}".into() };
        }
        Resp::json(200, json!({ "id": "gen-1", "model": "cloudflare/clef-omni-20261001", "provider": "Cloudflare", "answers": { "red": { "type": "noul", "noul": 0.98 } } }))
    })
    .await;
    let model = decision_model("openrouter/decisions", "cloudflare/clef-omni", json!({ "apiKey": "openrouter-key", "baseURL": format!("{}/api/v1", server.base) }));
    let response = decide(
        model,
        vec![
            DecisionPart::Text("Before:".into()),
            DecisionPart::Media { media: MediaAsset::url(format!("{}/before.png", server.base), Some("image/png".into())), filename: None },
            DecisionPart::Media { media: MediaAsset::base64("AAAA", "audio/wav"), filename: None },
        ],
        vec![object(json!({ "name": "red", "type": "predicate", "instructions": "Is the image mostly red?" }))],
        |r| r.provider_options = Some(object(json!({ "user": "user-1", "session_id": "session-1" }))),
    )
    .await
    .unwrap();
    assert_eq!(answers(&response), json!([{ "name": "red", "type": "predicate", "probability": 0.98 }]));
    assert_eq!(response.provider_metadata.unwrap()["openrouter"], json!({ "model": "cloudflare/clef-omni-20261001", "responseId": "gen-1", "provider": "Cloudflare" }));
    let sent = server.requests_to("/api/v1/systemone");
    assert_eq!(
        sent[0].json(),
        json!({
            "model": "cloudflare/clef-omni", "user": "user-1", "session_id": "session-1",
            "state": ["Before:", { "type": "image_url", "image_url": { "url": "data:image/png;base64,AQID" } }, { "type": "input_audio", "input_audio": { "data": "AAAA", "format": "wav" } }],
            "questions": { "red": { "type": "noul", "instructions": "Is the image mostly red?" } },
        })
    );
}

#[tokio::test]
async fn puts_media_in_a_user_message_inside_the_baseten_state() {
    let server = mock(|_, _| Resp::json(200, json!({ "model": "mammoth", "answers": { "red": { "type": "noul", "noul": 0.99 } } }))).await;
    let model = decision_model("baseten/decisions", "decision-models/baseten-mammoth-27b-latest", json!({ "apiKey": "baseten-key", "baseURL": format!("{}/v1", server.base) }));
    decide(
        model,
        vec![DecisionPart::Text("Photo:".into()), DecisionPart::Media { media: png(), filename: None }, DecisionPart::Json(json!({ "source": "upload" }))],
        vec![object(json!({ "name": "red", "type": "predicate", "instructions": "Is the photo mostly red?" }))],
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(
        only(server.requests()).json(),
        json!({
            "model": "decision-models/baseten-mammoth-27b-latest",
            "state": { "messages": [{ "role": "user", "content": [
                { "type": "text", "text": "Photo:" },
                { "type": "image_url", "image_url": { "url": "data:image/png;base64,AQID" } },
                { "type": "text", "text": "{\"source\":\"upload\"}" },
            ] }] },
            "questions": { "red": { "type": "noul", "instructions": "Is the photo mostly red?" } },
        })
    );
}

fn openai_decisions(base: &str) -> decision::DecisionModel {
    decision::openai::route()
        .with_base_url(Some(format!("{base}/v1")))
        .with_auth(oc_ai::route::auth::bearer_options(Some("openai-key".into()), &[]))
        .with_http(Some(oc_ai::HttpOptions { query: Some([("api".to_string(), "v1".to_string())].into_iter().collect()), ..Default::default() }))
        .model("gpt-6-luna", None, Some(Map::new()))
}

#[tokio::test]
async fn decides_through_openai_decisions() {
    let server = mock(|_, _| {
        Resp::json(200, json!({
            "model": "gpt-6-luna",
            "answers": [
                { "type": "predicate", "name": "refund", "probability": 0.97 },
                { "type": "choice", "name": "department", "choice": "billing", "probabilities": [{ "value": "billing", "probability": 0.9 }, { "value": "technical", "probability": 0.1 }], "confidence": 0.8 },
                { "type": "score", "name": "urgency", "score": 1.2, "probabilities": [
                    { "value": 0, "label": "Can wait", "probability": 0 }, { "value": 1, "label": "Needs attention", "probability": 0.8 }, { "value": 2, "label": "Blocking", "probability": 0.2 }], "confidence": 0.6 },
            ],
            "usage": { "input_tokens": 40, "output_tokens": 0, "total_tokens": 40 },
        }))
    })
    .await;
    let response = decide(
        openai_decisions(&server.base),
        vec![
            DecisionPart::Text("Ticket:".into()),
            DecisionPart::Json(json!({ "ticket": "Please refund the duplicate charge." })),
            DecisionPart::Json(json!(42)),
            DecisionPart::Media { media: png(), filename: None },
            DecisionPart::Media { media: MediaAsset::url("https://assets.test/receipt.jpg", Some("image/jpeg".into())), filename: None },
        ],
        questions(),
        |r| r.provider_options = Some(object(json!({ "safetyIdentifier": "user-1", "promptCacheKey": "support-v1" }))),
    )
    .await
    .unwrap();
    assert_eq!(answers(&response), expected_answers());
    assert_eq!(response.usage.as_ref().unwrap().input_tokens, Some(40.0));
    let sent = only(server.requests());
    assert_eq!((sent.path.as_str(), sent.query.as_str()), ("/v1/decisions", "api=v1"));
    assert_eq!(sent.header("authorization"), Some("Bearer openai-key"));
    assert_eq!(
        sent.json(),
        json!({
            "safety_identifier": "user-1", "prompt_cache_key": "support-v1", "model": "gpt-6-luna",
            "input": [{ "role": "user", "content": [
                { "type": "input_text", "text": "Ticket:" },
                { "type": "input_json", "data": { "ticket": "Please refund the duplicate charge." } },
                { "type": "input_text", "text": "42" },
                { "type": "input_image", "image_url": "data:image/png;base64,AQID" },
                { "type": "input_image", "image_url": "https://assets.test/receipt.jpg" },
            ] }],
            "questions": questions(),
        })
    );
}

#[tokio::test]
async fn returns_openai_refusals_and_rejects_bad_answers() {
    let server = mock(|_, n| match n {
        0 => Resp::json(200, json!({ "model": "gpt-6-luna", "answers": [
            { "type": "predicate", "name": "greeting", "probability": 0.99 },
            { "type": "refusal", "name": "unsafe" },
            { "type": "refusal", "name": "explained", "message": "I can't help with that." },
        ] })),
        _ => Resp::json(200, json!({ "model": "gpt-6-luna", "answers": [
            { "type": "choice", "name": "tone", "choice": "angry", "probabilities": [{ "value": "angry", "probability": 1 }], "confidence": 1 },
        ] })),
    })
    .await;
    let predicate = |name: &str| object(json!({ "name": name, "type": "predicate", "instructions": "?" }));
    let response = decide(openai_decisions(&server.base), vec![DecisionPart::Text("hello".into())], vec![predicate("greeting"), predicate("unsafe"), predicate("explained")], |_| {})
        .await
        .unwrap();
    assert_eq!(
        answers(&response),
        json!([
            { "name": "greeting", "type": "predicate", "probability": 0.99 },
            { "name": "unsafe", "type": "error", "reason": "refusal" },
            { "name": "explained", "type": "error", "reason": "refusal", "message": "I can't help with that." },
        ])
    );
    let tone = object(json!({ "name": "tone", "type": "choice", "instructions": "Tone?", "choices": [{ "value": "calm" }] }));
    let error = decide(openai_decisions(&server.base), vec![DecisionPart::Text("refund".into())], vec![tone], |_| {}).await.unwrap_err();
    assert_eq!(error.tag(), "InvalidProviderOutput");
    let audio = decide(
        openai_decisions(&server.base),
        vec![DecisionPart::Media { media: MediaAsset::base64("AAAA", "audio/wav"), filename: None }],
        vec![predicate("speech")],
        |_| {},
    )
    .await
    .unwrap_err();
    assert_eq!(audio.tag(), "UnsupportedOperation");
    assert!(audio.message.contains("audio"));
    assert_eq!(server.requests().len(), 2);
}

#[tokio::test]
async fn decides_through_the_vercel_gateway_and_cloudflare_run() {
    let server = mock(|req, n| {
        if req.path.ends_with("/decisions") {
            return Resp::json(200, json!({ "model": "typesafe-ai/jev", "answers": [{ "type": "predicate", "name": "refund", "probability": 0.98 }], "usage": { "input_tokens": 10, "output_tokens": 0 } }));
        }
        if n == 0 {
            return Resp::json(200, json!({ "result": { "model": "clef-omni", "answers": { "red": { "type": "noul", "noul": 0.9 } } } }));
        }
        Resp::json(400, json!({ "errors": [{ "message": "Model execution failed (User Input Error): Validation error at questions: At least one question is required", "code": 7003 }], "success": false, "result": {}, "messages": [] }))
    })
    .await;
    let vercel = decision_model("vercel-ai-gateway/decisions", "typesafe-ai/jev", json!({ "apiKey": "gateway-key", "baseURL": format!("{}/v1/", server.base) }));
    let refund = object(json!({ "name": "refund", "type": "predicate", "instructions": "Is a refund requested?" }));
    let response = decide(vercel, vec![DecisionPart::Text("refund".into())], vec![refund.clone()], |_| {}).await.unwrap();
    assert_eq!(Value::Object(response.provider_metadata.unwrap()), json!({ "vercel-ai-gateway": { "model": "typesafe-ai/jev", "usage": { "input_tokens": 10, "output_tokens": 0 } } }));
    let base = format!("{}/client/v4/accounts/test-account/ai/v1", server.base);
    let workers = || decision_model("cloudflare-workers-ai/decisions", "@cf/cloudflare/clef-omni", json!({ "apiKey": "cloudflare-key", "baseURL": base }));
    let response = decide(
        workers(),
        vec![DecisionPart::Text("Compare:".into()), DecisionPart::Media { media: png(), filename: None }, DecisionPart::Media { media: MediaAsset::base64("AAAA", "audio/wav"), filename: None }],
        vec![object(json!({ "name": "red", "type": "predicate", "instructions": "Is the image mostly red?" }))],
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(Value::Object(response.provider_metadata.unwrap()), json!({ "cloudflare-workers-ai": { "model": "clef-omni" } }));
    let run = server.requests_to("/client/v4/accounts/test-account/ai/run");
    assert_eq!(
        run[0].json(),
        json!({ "model": "@cf/cloudflare/clef-omni", "input": {
            "state": "Compare:", "images": ["data:image/png;base64,AQID"], "audio": ["data:audio/wav;base64,AAAA"],
            "questions": { "red": { "type": "noul", "instructions": "Is the image mostly red?" } },
        } })
    );
    let error = decide(workers(), vec![DecisionPart::Text("refund".into())], vec![refund], |_| {}).await.unwrap_err();
    assert_eq!(error.tag(), "InvalidRequest");
    assert!(error.message.contains("At least one question is required"));
}

#[tokio::test]
async fn fails_when_system_one_leaves_a_question_unanswered() {
    let server = mock(|_, _| Resp::json(200, json!({ "model": "jev-1.14.0", "answers": { "refund": { "type": "noul", "noul": 0.9 } } }))).await;
    let model = decision_model("typesafe-ai/decisions", "jev-latest", json!({ "apiKey": "test", "baseURL": format!("{}/v1", server.base) }));
    let predicate = |name: &str| object(json!({ "name": name, "type": "predicate", "instructions": "?" }));
    let error = decide(model, vec![DecisionPart::Text("refund".into())], vec![predicate("refund"), predicate("angry")], |_| {}).await.unwrap_err();
    assert_eq!(error.tag(), "InvalidProviderOutput");
}
