//! Upstream's video tests (`packages/ai/test/video.test.ts`, `minimax-video.test.ts`, `openrouter-video.test.ts`,
//! `seedance-video.test.ts`, `google-interactions-video.test.ts`) against a scripted server on 127.0.0.1. Each
//! model is built through its package entrypoint (`video_model`) with its `baseURL` pointed at the server, so the
//! upstream host `https://xai.test/v1` becomes `http://127.0.0.1:<port>/v1` and URLs are compared by method, path
//! and query.

#[path = "common/http_mock.rs"]
mod http_mock;

use futures::StreamExt;
use http_mock::{Mock, Req, Resp, mock};
use indexmap::IndexMap;
use oc_ai::json::Object;
use oc_ai::media::{MediaInfo, MediaSource, parse_data_url};
use oc_ai::video::{self, Reason, Status, VideoModel, VideoOutput, VideoRequest, VideoStatus, VideoUsage};
use oc_ai::{AIError, ErrorKind, HttpOptions, MediaAsset};
use serde_json::{Value, json};

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

fn obj(value: Value) -> Object {
    value.as_object().unwrap().clone()
}

/// `<Provider>.configure({ apiKey, baseURL }).video(id)`.
fn configured(package: &str, id: &str, api_key: &str, base_url: Option<&str>) -> VideoModel {
    let mut settings = obj(json!({ "apiKey": api_key }));
    if let Some(base) = base_url {
        settings.insert("baseURL".into(), json!(base));
    }
    oc_ai::providers::media::video_model(&format!("@opencode/ai/providers/{package}/videos"), id, &settings).expect("a video package").expect("a video model")
}

fn model(package: &str, id: &str, base_url: &str) -> VideoModel {
    configured(package, id, "test", Some(base_url))
}

fn running() -> VideoStatus {
    VideoStatus::running()
}

fn completed() -> VideoStatus {
    VideoStatus { status: Status::Completed, reason: None, progress: None }
}

fn errored(reason: Reason) -> VideoStatus {
    VideoStatus { status: Status::Error, reason: Some(reason), progress: None }
}

fn line(request: &Req) -> String {
    if request.query.is_empty() { format!("{} {}", request.method, request.path) } else { format!("{} {}?{}", request.method, request.path, request.query) }
}

fn lines(server: &Mock) -> Vec<String> {
    server.requests().iter().map(line).collect()
}

fn data(url: &str) -> MediaAsset {
    parse_data_url(url).unwrap()
}

/// A bare URL string, whose type the request can't tell.
fn url(value: &str) -> MediaAsset {
    MediaAsset::url(value, None)
}

/// `Media.url(url, { mediaType })`.
fn typed(value: &str, media_type: &str) -> MediaAsset {
    MediaAsset::url(value, Some(media_type.into()))
}

fn http(headers: &[(&str, &str)], query: &[(&str, &str)]) -> HttpOptions {
    let map = |pairs: &[(&str, &str)]| -> Option<IndexMap<String, String>> {
        (!pairs.is_empty()).then(|| pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect())
    };
    HttpOptions { headers: map(headers), query: map(query), ..Default::default() }
}

fn request(model: &VideoModel, prompt: &str) -> VideoRequest {
    VideoRequest::new(model.clone(), prompt)
}

async fn start(request: VideoRequest) -> Result<String, AIError> {
    video::start(&client(), request).await
}

async fn status(model: &VideoModel, id: &str) -> Result<VideoStatus, AIError> {
    video::status(&client(), model, id).await
}

async fn result(model: &VideoModel, id: &str) -> Result<video::VideoResponse, AIError> {
    video::result(&client(), model, id).await
}

/// A server for cases that must fail before sending: it answers, but the test asserts it was never called.
async fn untouched() -> Mock {
    mock(|_, _| Resp::json(500, json!({ "error": "reached the network" }))).await
}

/// A server answering every request with one JSON body.
async fn reply(body: Value) -> Mock {
    mock(move |_, _| Resp::json(200, body.clone())).await
}

/// The error of a result read answered with `body`, checked to carry exactly that body.
async fn result_error(model_for: impl Fn(&str) -> VideoModel, id: &str, body: &Value) -> AIError {
    let server = reply(body.clone()).await;
    let error = result(&model_for(&server.base), id).await.unwrap_err();
    assert_eq!(error.body.as_deref(), Some(body.to_string().as_str()), "{}", error.message);
    error
}

fn operation(error: &AIError) -> Option<&str> {
    match &error.kind {
        ErrorKind::UnsupportedOperation { operation, .. } => Some(operation),
        _ => None,
    }
}

async fn collect(output: &VideoOutput) -> Vec<Vec<u8>> {
    let mut stream = video::open(&client(), output).await.unwrap();
    let mut chunks = vec![];
    while let Some(chunk) = stream.next().await {
        chunks.push(chunk.unwrap().to_vec());
    }
    chunks
}

fn url_source(value: &str, media_type: &str) -> MediaSource {
    MediaSource::Url { url: value.into(), media_type: Some(media_type.into()), expires_at: None }
}

fn now_ms() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as f64
}

// =============================================================================================================
// video.test.ts (xAI and the queued route)
// =============================================================================================================

mod xai {
    use super::*;

    pub fn model(base: &str) -> VideoModel {
        super::model("xai", "grok-imagine-video-1.5", &format!("{base}/v1"))
    }

    pub fn done() -> Value {
        json!({ "status": "done", "video": { "url": "https://vidgen.x.ai/out.mp4" } })
    }

    async fn submitted() -> Mock {
        mock(|_, _| Resp::json(200, json!({ "request_id": "req_1" }))).await
    }

    // video.test.ts: "maps the request onto xAI's body and returns the job without polling"
    #[tokio::test]
    async fn maps_the_request_onto_xais_body_and_returns_the_job_without_polling() {
        let server = submitted().await;
        let mut r = request(&model(&server.base), "Make the water crash down");
        r.start = Some(video::image(data("data:image/png;base64,AQID")).unwrap());
        r.end = Some(url("https://example.test/last.png"));
        r.references = vec![MediaAsset::reference("xai", "file_1", None), typed("https://example.test/voice.mp3", "audio/mpeg")];
        r.duration = video::parse_duration("10 seconds");
        r.aspect = Some("16:9".into());
        r.resolution = Some("720p".into());
        r.sound = Some(false);
        r.provider_options = Some(obj(json!({ "future_option": true })));
        r.http = Some(http(&[("x-request", "yes")], &[("trace", "1")]));
        assert_eq!(start(r).await.unwrap(), "req_1");
        let calls = server.requests();
        assert_eq!(lines(&server), ["POST /v1/videos/generations?trace=1"]);
        assert_eq!(calls[0].header("authorization"), Some("Bearer test"));
        assert_eq!(calls[0].header("x-request"), Some("yes"));
        assert_eq!(
            calls[0].json(),
            json!({
                "model": "grok-imagine-video-1.5",
                "prompt": "Make the water crash down",
                "image": { "url": "data:image/png;base64,AQID" },
                "last_frame": { "url": "https://example.test/last.png" },
                "reference_images": [{ "file_id": "file_1" }],
                "reference_audios": [{ "url": "https://example.test/voice.mp3" }],
                "duration": 10,
                "aspect_ratio": "16:9",
                "resolution": "720p",
                "generate_audio": false,
                "future_option": true,
            })
        );
    }

    // video.test.ts: "leaves aspect out for auto when a start image sets the shape, and sends it otherwise"
    #[tokio::test]
    async fn leaves_aspect_out_for_auto_when_a_start_image_sets_the_shape_and_sends_it_otherwise() {
        let server = submitted().await;
        let m = model(&server.base);
        let mut framed = request(&m, "x");
        framed.start = Some(url("https://example.test/first.png"));
        framed.aspect = Some("auto".into());
        start(framed).await.unwrap();
        let mut plain = request(&m, "x");
        plain.aspect = Some("auto".into());
        start(plain).await.unwrap();
        let ratios: Vec<Value> = server.requests().iter().map(|c| c.json().get("aspect_ratio").cloned().unwrap_or(Value::Null)).collect();
        assert_eq!(ratios, [Value::Null, json!("auto")]);
    }

    // video.test.ts: "sends a native source video to edits or extensions through providerOptions.mode"
    #[tokio::test]
    async fn sends_a_native_source_video_to_edits_or_extensions_through_provider_options_mode() {
        let server = mock(|_, _| Resp::json(200, json!({ "request_id": "req_2" }))).await;
        let m = model(&server.base);
        let video = json!({ "url": "https://example.test/in.mp4" });
        let mut edit = request(&m, "brighter");
        edit.provider_options = Some(obj(json!({ "mode": "edit", "video": video })));
        start(edit).await.unwrap();
        let mut extend = request(&m, "keep going");
        extend.provider_options = Some(obj(json!({ "mode": "extend", "video": video })));
        start(extend).await.unwrap();
        let calls = server.requests();
        assert_eq!(calls.iter().map(|c| c.path.clone()).collect::<Vec<_>>(), ["/v1/videos/edits", "/v1/videos/extensions"]);
        assert_eq!(
            calls.iter().map(Req::json).collect::<Vec<_>>(),
            [
                json!({ "model": "grok-imagine-video-1.5", "prompt": "brighter", "video": video }),
                json!({ "model": "grok-imagine-video-1.5", "prompt": "keep going", "video": video }),
            ]
        );
    }

    // video.test.ts: "rejects reference videos, which xAI has no field for, before sending"
    #[tokio::test]
    async fn rejects_reference_videos_which_xai_has_no_field_for_before_sending() {
        let server = untouched().await;
        let mut r = request(&model(&server.base), "x");
        r.references = vec![typed("https://example.test/motion.mp4", "video/mp4")];
        let error = start(r).await.unwrap_err();
        assert_eq!(error.tag(), "UnsupportedOperation");
        assert!(error.message.contains("reference video"), "{}", error.message);
        assert!(server.requests().is_empty());
    }

    // video.test.ts: "rejects start and end frames that are known not to be images before sending"
    #[tokio::test]
    async fn rejects_start_and_end_frames_that_are_known_not_to_be_images_before_sending() {
        let server = untouched().await;
        let m = model(&server.base);
        let mut r = request(&m, "x");
        r.start = Some(data("data:video/mp4;base64,AAAA"));
        let video = start(r).await.unwrap_err();
        assert_eq!(video.tag(), "InvalidRequest");
        assert!(video.message.contains("expected an image, got video/mp4"), "{}", video.message);
        let mut r = request(&m, "x");
        r.end = Some(typed("https://example.test/a.mp3", "audio/mpeg"));
        let audio = start(r).await.unwrap_err();
        assert_eq!(audio.tag(), "InvalidRequest");
        assert!(server.requests().is_empty());
    }

    // video.test.ts: "Media.image throws a typed AIError for a known non-image"
    #[test]
    fn media_image_throws_a_typed_aierror_for_a_known_non_image() {
        let error = video::image(data("data:video/mp4;base64,AAAA")).unwrap_err();
        assert_eq!(error.tag(), "InvalidRequest");
        assert!(error.message.contains("expected an image, got video/mp4"), "{}", error.message);
    }

    // video.test.ts: "sends frames whose type it can't tell, like URLs and provider file references"
    #[tokio::test]
    async fn sends_frames_whose_type_it_cant_tell_like_urls_and_provider_file_references() {
        let server = mock(|_, _| Resp::json(200, json!({ "request_id": "r" }))).await;
        let mut r = request(&model(&server.base), "x");
        r.start = Some(url("https://example.test/frame?id=7"));
        r.end = Some(MediaAsset::reference("xai", "file_1", None));
        start(r).await.unwrap();
        let body = server.requests()[0].json();
        assert_eq!(body["image"], json!({ "url": "https://example.test/frame?id=7" }));
        assert_eq!(body["last_frame"], json!({ "file_id": "file_1" }));
    }

    // video.test.ts: "rejects durations that aren't seconds before sending"
    // A duration that isn't a seconds string is a type error in TypeScript; here the string form doesn't parse and a
    // number that isn't a duration fails before the network.
    #[tokio::test]
    async fn rejects_durations_that_arent_seconds_before_sending() {
        assert_eq!(video::parse_duration("8"), None);
        let server = untouched().await;
        for duration in [f64::NAN, -1.0] {
            let mut r = request(&model(&server.base), "x");
            r.duration = Some(duration);
            assert_eq!(start(r).await.unwrap_err().tag(), "InvalidRequest");
        }
        assert!(server.requests().is_empty());
    }

    // video.test.ts: "never retries a failed submit"
    #[tokio::test]
    async fn never_retries_a_failed_submit() {
        let server = mock(|_, _| super::failure(503)).await;
        let error = start(request(&model(&server.base), "x")).await.unwrap_err();
        assert_eq!(error.tag(), "ProviderInternal");
        assert_eq!(server.requests().iter().map(|c| c.method.clone()).collect::<Vec<_>>(), ["POST"]);
    }

    // video.test.ts: "reads one status per call by job ID"
    #[tokio::test]
    async fn reads_one_status_per_call_by_job_id() {
        let server = mock(|r, n| {
            if r.method == "POST" {
                return Resp::json(200, json!({ "request_id": "req_1" }));
            }
            if n == 0 { Resp::json(200, json!({ "status": "pending", "progress": 40 })) } else { Resp::json(200, done()) }
        })
        .await;
        let m = model(&server.base);
        let id = start(request(&m, "x")).await.unwrap();
        let statuses = [status(&m, &id).await.unwrap(), status(&m, &id).await.unwrap()];
        assert_eq!(statuses, [VideoStatus { progress: Some(0.4), ..running() }, completed()]);
        assert_eq!(lines(&server), ["POST /v1/videos/generations", "GET /v1/videos/req_1", "GET /v1/videos/req_1"]);
    }

    // video.test.ts: "encodes the job ID into the request path"
    #[tokio::test]
    async fn encodes_the_job_id_into_the_request_path() {
        let server = reply(done()).await;
        status(&model(&server.base), "../files?x=1").await.unwrap();
        assert_eq!(lines(&server), ["GET /v1/videos/..%2Ffiles%3Fx%3D1"]);
    }

    // video.test.ts: "keeps unknown statuses running, including inherited property names"
    #[tokio::test]
    async fn keeps_unknown_statuses_running_including_inherited_property_names() {
        for raw in ["warming_up", "constructor"] {
            let server = reply(json!({ "status": raw })).await;
            assert_eq!(status(&model(&server.base), "req_1").await.unwrap().status, Status::Running);
        }
    }

    // video.test.ts: "returns the finished video as an expiring URL with xAI metadata"
    #[tokio::test]
    async fn returns_the_finished_video_as_an_expiring_url_with_xai_metadata() {
        let server = reply(json!({
            "status": "done",
            "video": { "url": "https://vidgen.x.ai/out.mp4", "duration": 10, "respect_moderation": true },
            "model": "grok-imagine-video-1.5",
        }))
        .await;
        let response = result(&model(&server.base), "req_1").await.unwrap();
        let video = &response.videos[0];
        assert_eq!(video.asset.source, url_source("https://vidgen.x.ai/out.mp4", "video/mp4"));
        assert_eq!(video.asset.info, Some(MediaInfo { duration_seconds: Some(10.0), ..Default::default() }));
        assert_eq!(
            response.provider_metadata,
            Some(obj(json!({ "xai": { "requestId": "req_1", "model": "grok-imagine-video-1.5", "respectModeration": true } })))
        );
    }

    // video.test.ts: "fails a result read before the job finishes"
    #[tokio::test]
    async fn fails_a_result_read_before_the_job_finishes() {
        let body = json!({ "status": "pending", "progress": 40 });
        let error = result_error(model, "req_1", &body).await;
        assert_eq!(error.tag(), "InvalidRequest");
        assert_eq!(error.message, "xAI Video generation req_1 has not finished; await it before reading the result");
    }

    // video.test.ts: "fails the result of ${terminal.body.error?.code ?? terminal.body.status} jobs with the provider body"
    // (invalid_argument, failed_precondition, permission_denied, service_unavailable, constructor, expired)
    #[tokio::test]
    async fn fails_the_result_of_terminal_jobs_with_the_provider_body() {
        let failed = |code: &str, message: &str| json!({ "status": "failed", "error": { "code": code, "message": message } });
        let cases = [
            (
                failed("invalid_argument", "Prompt cannot be empty."),
                "InvalidRequest",
                "xAI Video generation failed (invalid_argument): Prompt cannot be empty.",
            ),
            (
                failed("failed_precondition", "Extension is not supported."),
                "InvalidRequest",
                "xAI Video generation failed (failed_precondition): Extension is not supported.",
            ),
            (failed("permission_denied", "Team lacks access."), "Authentication", "xAI Video generation failed (permission_denied): Team lacks access."),
            (failed("service_unavailable", "Overloaded."), "ProviderInternal", "xAI Video generation failed (service_unavailable): Overloaded."),
            (failed("constructor", "Future code."), "ProviderInternal", "xAI Video generation failed (constructor): Future code."),
            (json!({ "status": "expired" }), "InvalidRequest", "xAI Video request req_1 expired"),
        ];
        for (body, tag, message) in cases {
            let error = result_error(model, "req_1", &body).await;
            assert_eq!((error.tag(), error.message.as_str()), (tag, message));
        }
    }

    // video.test.ts: "returns moderated results and fails withheld videos as a content policy failure"
    #[tokio::test]
    async fn returns_moderated_results_and_fails_withheld_videos_as_a_content_policy_failure() {
        let server = reply(json!({ "status": "done", "video": { "url": "https://vidgen.x.ai/o.mp4", "respect_moderation": false } })).await;
        let flagged = result(&model(&server.base), "req_1").await.unwrap();
        assert_eq!(flagged.provider_metadata, Some(obj(json!({ "xai": { "requestId": "req_1", "respectModeration": false } }))));
        let server = reply(json!({ "status": "done", "video": { "respect_moderation": false } })).await;
        let withheld = result(&model(&server.base), "req_1").await.unwrap_err();
        assert_eq!(withheld.tag(), "ContentPolicy");
    }

    // video.test.ts: "fails cancel as unsupported without sending a request, since xAI has no cancel endpoint"
    #[tokio::test]
    async fn fails_cancel_as_unsupported_without_sending_a_request_since_xai_has_no_cancel_endpoint() {
        let server = untouched().await;
        let error = video::cancel(&client(), &model(&server.base), "req_1").await.unwrap_err();
        assert_eq!(error.tag(), "UnsupportedOperation");
        assert!(error.message.contains("cancel"), "{}", error.message);
        assert!(server.requests().is_empty());
    }
}

/// `failure(input, status, headers)`.
fn failure(status: u16) -> Resp {
    Resp::json(status, json!({ "error": format!("HTTP {status}") }))
}

// Transient read failures. The read retry waits for real (a jittered first gap of about a second), so only these
// two cases retry.
mod transient {
    use super::*;

    // video.test.ts: "retries a 503 status read and a 503 result read"
    #[tokio::test]
    async fn retries_a_503_status_read_and_a_503_result_read() {
        // 1: status fails, 2: status succeeds, 3: result fails, 4: result succeeds.
        let server = mock(|_, n| if n == 0 || n == 2 { failure(503) } else { Resp::json(200, xai::done()) }).await;
        let m = xai::model(&server.base);
        assert_eq!(status(&m, "req_1").await.unwrap(), completed());
        let response = result(&m, "req_1").await.unwrap();
        assert_eq!(response.videos[0].asset.url_value(), Some("https://vidgen.x.ai/out.mp4"));
        assert_eq!(server.requests().len(), 4);
    }

    // video.test.ts: "waits for a 429 retry-after before reading the status again"
    // Upstream advances a TestClock through a 10 second retry-after; this waits through a real 2 second one, which
    // is longer than the jittered first gap could be on its own.
    #[tokio::test]
    async fn waits_for_a_429_retry_after_before_reading_the_status_again() {
        let server = mock(|_, n| if n == 0 { failure(429).with("retry-after", "2") } else { Resp::json(200, xai::done()) }).await;
        let begun = std::time::Instant::now();
        assert_eq!(status(&xai::model(&server.base), "req_1").await.unwrap(), completed());
        assert!(begun.elapsed() >= std::time::Duration::from_millis(1990), "retried after {:?}", begun.elapsed());
        assert_eq!(server.requests().len(), 2);
    }

    // video.test.ts: "fails a 400 status read without retrying"
    #[tokio::test]
    async fn fails_a_400_status_read_without_retrying() {
        let server = mock(|_, _| failure(400)).await;
        let error = status(&xai::model(&server.base), "req_1").await.unwrap_err();
        assert_eq!(error.tag(), "InvalidRequest");
        assert_eq!(server.requests().len(), 1);
    }
}

// Media.Asset.stream
mod stream {
    use super::*;

    // video.test.ts: "streams a URL asset's bytes from the network with its download headers"
    #[tokio::test]
    async fn streams_a_url_assets_bytes_from_the_network_with_its_download_headers() {
        let server = mock(|_, _| Resp { status: 200, headers: vec![("content-type".into(), "video/mp4".into())], body: "\u{1}\u{2}\u{3}".into() }).await;
        let mut output = VideoOutput::url(&format!("{}/out.mp4", server.base), "video/mp4", None);
        output.headers = Some(IndexMap::from([("x-key".to_string(), "k".to_string())]));
        assert_eq!(collect(&output).await.concat(), [1, 2, 3]);
        assert_eq!(server.requests()[0].header("x-key"), Some("k"));
    }

    // video.test.ts: "streams inline bytes without a request"
    #[tokio::test]
    async fn streams_inline_bytes_without_a_request() {
        let output = VideoOutput::new(MediaAsset::bytes(vec![4, 5], Some("video/mp4")));
        assert_eq!(collect(&output).await, [vec![4u8, 5]]);
    }
}

// =============================================================================================================
// minimax-video.test.ts
// =============================================================================================================

mod minimax {
    use super::*;

    pub fn model(base: &str, id: &str) -> VideoModel {
        super::model("minimax", id, base)
    }
    fn h3(base: &str) -> VideoModel {
        model(base, "MiniMax-H3-Lite")
    }
    fn hailuo(base: &str) -> VideoModel {
        model(base, "MiniMax-Hailuo-02")
    }

    fn task(fields: Value) -> Value {
        let mut task = obj(json!({ "id": "task_1", "model": "MiniMax-H3-Lite" }));
        task.extend(obj(fields));
        json!({ "task": task })
    }

    fn succeeded() -> Value {
        task(json!({
            "status": "succeeded",
            "content": { "url": "https://cdn.minimax.test/out.mp4" },
            "resolution": "480P",
            "duration": 4,
            "ratio": "16:9",
            "usage": { "total_seconds": 4, "output_seconds": 4, "input_image_count": 0 },
        }))
    }

    fn hailuo_task(fields: Value) -> Value {
        let mut task = obj(json!({ "task_id": "task_1", "base_resp": { "status_code": 0, "status_msg": "success" } }));
        task.extend(obj(fields));
        Value::Object(task)
    }

    async fn submitted() -> Mock {
        mock(|_, _| Resp::json(200, json!({ "task_id": "task_1", "base_resp": { "status_code": 0, "status_msg": "success" } }))).await
    }

    /// `sent(requests)`: the paths and parsed bodies MiniMax receives for each submitted request.
    async fn sent(server: &Mock, requests: Vec<VideoRequest>) -> Vec<(String, Value)> {
        for mut r in requests {
            if r.prompt.is_empty() {
                r.prompt = "test".into();
            }
            start(r).await.unwrap();
        }
        server.requests().iter().map(|c| (line(c), c.json())).collect()
    }

    // minimax-video.test.ts: "maps every shared field onto the v2 content array and returns the job without polling"
    #[tokio::test]
    async fn maps_every_shared_field_onto_the_v2_content_array_and_returns_the_job_without_polling() {
        let server = mock(|_, _| Resp::json(200, json!({ "task_id": "task_1" }))).await;
        let mut r = request(&h3(&server.base), "Make the water crash down");
        r.references = vec![
            data("data:image/png;base64,AQID"),
            typed("https://example.test/motion.mp4", "video/mp4"),
            typed("https://example.test/voice.mp3", "audio/mpeg"),
            MediaAsset::reference("minimax", "file_1", None),
        ];
        r.duration = Some(6.0);
        r.aspect = Some("16:9".into());
        r.resolution = Some("768p".into());
        r.sound = Some(true);
        r.provider_options = Some(obj(json!({ "extra": { "prompt_expansion_mode": "disabled" }, "future_option": true })));
        r.http = Some(http(&[("x-request", "yes")], &[("trace", "1")]));
        assert_eq!(start(r).await.unwrap(), "task_1");
        let calls = server.requests();
        assert_eq!(lines(&server), ["POST /v2/video_generation?trace=1"]);
        assert_eq!(calls[0].header("authorization"), Some("Bearer test"));
        assert_eq!(calls[0].header("x-request"), Some("yes"));
        assert_eq!(
            calls[0].json(),
            json!({
                "model": "MiniMax-H3-Lite",
                "content": [
                    { "type": "text", "text": "Make the water crash down" },
                    { "type": "image_url", "image_url": { "url": "data:image/png;base64,AQID" }, "role": "reference_image" },
                    { "type": "video_url", "video_url": { "url": "https://example.test/motion.mp4" }, "role": "reference_video" },
                    { "type": "audio_url", "audio_url": { "url": "https://example.test/voice.mp3" }, "role": "reference_audio" },
                    { "type": "image_url", "image_url": { "url": "mm_file://file_1" }, "role": "reference_image" },
                ],
                "duration": 6,
                "ratio": "16:9",
                "resolution": "768P",
                "extra": { "prompt_expansion_mode": "disabled" },
                "future_option": true,
            })
        );
    }

    // minimax-video.test.ts: "sends first and last frames by role and maps auto to adaptive"
    #[tokio::test]
    async fn sends_first_and_last_frames_by_role_and_maps_auto_to_adaptive() {
        let server = submitted().await;
        let mut r = request(&h3(&server.base), "");
        r.start = Some(url("https://example.test/first.png"));
        r.end = Some(url("https://example.test/last.png"));
        r.aspect = Some("auto".into());
        r.resolution = Some("2k".into());
        let requests = sent(&server, vec![r]).await;
        assert_eq!(
            requests[0].1,
            json!({
                "model": "MiniMax-H3-Lite",
                "content": [
                    { "type": "text", "text": "test" },
                    { "type": "image_url", "image_url": { "url": "https://example.test/first.png" }, "role": "first_frame" },
                    { "type": "image_url", "image_url": { "url": "https://example.test/last.png" }, "role": "last_frame" },
                ],
                "ratio": "adaptive",
                "resolution": "2K",
            })
        );
    }

    // minimax-video.test.ts: "defaults a text-only ratio to 16:9 and leaves resolution and duration unset"
    #[tokio::test]
    async fn defaults_a_text_only_ratio_to_16_9_and_leaves_resolution_and_duration_unset() {
        let server = submitted().await;
        let plain = request(&h3(&server.base), "");
        let mut auto = request(&h3(&server.base), "");
        auto.aspect = Some("auto".into());
        let requests = sent(&server, vec![plain, auto]).await;
        let expected = json!({ "model": "MiniMax-H3-Lite", "content": [{ "type": "text", "text": "test" }], "ratio": "16:9" });
        assert_eq!(requests.into_iter().map(|(_, body)| body).collect::<Vec<_>>(), [expected.clone(), expected]);
    }

    // minimax-video.test.ts: "sends nothing for sound, which neither API has a field for"
    #[tokio::test]
    async fn sends_nothing_for_sound_which_neither_api_has_a_field_for() {
        let server = submitted().await;
        let with = |m: VideoModel, sound: bool| {
            let mut r = request(&m, "");
            r.sound = Some(sound);
            r
        };
        let requests = sent(&server, vec![with(h3(&server.base), false), with(h3(&server.base), true), with(hailuo(&server.base), true)]).await;
        for (_, body) in requests {
            assert!(body.get("generate_audio").is_none() && body.get("sound").is_none(), "{body}");
        }
    }

    // minimax-video.test.ts: "returns MiniMax's HTTP errors typed"
    #[tokio::test]
    async fn returns_minimaxs_http_errors_typed() {
        let body = json!({
            "type": "error",
            "error": {
                "type": "bad_request_error",
                "message": "invalid params, ratio is required for t2va (text-only) and cannot be 'adaptive' (2013)",
                "http_code": "400",
            },
        });
        let server = mock(move |_, _| Resp::json(400, body.clone())).await;
        let mut r = request(&h3(&server.base), "x");
        r.resolution = Some("480p".into());
        r.duration = Some(4.0);
        assert_eq!(start(r).await.unwrap_err().tag(), "InvalidRequest");
    }

    // minimax-video.test.ts: "routes Hailuo and 01-series models to the v1 API and H3 to v2"
    #[tokio::test]
    async fn routes_hailuo_and_01_series_models_to_the_v1_api_and_h3_to_v2() {
        let server = submitted().await;
        let ids = ["MiniMax-Hailuo-2.3", "MiniMax-Hailuo-2.3-Fast", "I2V-01-live", "S2V-01", "MiniMax-H3", "MiniMax-H4"];
        let requests = sent(&server, ids.iter().map(|id| request(&model(&server.base, id), "")).collect()).await;
        assert_eq!(
            requests.into_iter().map(|(line, _)| line).collect::<Vec<_>>(),
            [
                "POST /v1/video_generation",
                "POST /v1/video_generation",
                "POST /v1/video_generation",
                "POST /v1/video_generation",
                "POST /v2/video_generation",
                "POST /v2/video_generation",
            ]
        );
    }

    // minimax-video.test.ts: "drops the version from a baseURL shared with the other MiniMax APIs"
    #[tokio::test]
    async fn drops_the_version_from_a_base_url_shared_with_the_other_minimax_apis() {
        let server = submitted().await;
        let bases = [format!("{}/v1", server.base), format!("{}/v1/", server.base), format!("{}/", server.base)];
        let requests = bases.iter().flat_map(|base| [request(&model(base, "MiniMax-H3"), ""), request(&model(base, "MiniMax-Hailuo-2.3"), "")]).collect();
        let lines: Vec<String> = sent(&server, requests).await.into_iter().map(|(line, _)| line).collect();
        assert_eq!(lines, ["POST /v2/video_generation", "POST /v1/video_generation"].repeat(3));
    }

    // minimax-video.test.ts: "maps the shared fields onto v1's flat body"
    #[tokio::test]
    async fn maps_the_shared_fields_onto_v1s_flat_body() {
        let server = submitted().await;
        let mut r = request(&hailuo(&server.base), "");
        r.start = Some(video::image(data("data:image/png;base64,AQID")).unwrap());
        r.end = Some(url("https://example.test/last.png"));
        r.references = vec![url("https://example.test/face.png")];
        r.duration = Some(10.0);
        r.aspect = Some("auto".into());
        r.resolution = Some("768p".into());
        r.sound = Some(false);
        r.provider_options = Some(obj(json!({ "prompt_optimizer": false, "fast_pretreatment": true })));
        let requests = sent(&server, vec![r]).await;
        assert_eq!(
            requests[0].1,
            json!({
                "model": "MiniMax-Hailuo-02",
                "prompt": "test",
                "first_frame_image": "data:image/png;base64,AQID",
                "last_frame_image": "https://example.test/last.png",
                "subject_reference": [{ "type": "character", "image": ["https://example.test/face.png"] }],
                "duration": 10,
                "resolution": "768P",
                "prompt_optimizer": false,
                "fast_pretreatment": true,
            })
        );
    }

    // minimax-video.test.ts: "rejects ${rejected.name}, which v1 has no field for, before sending"
    // (a reference video, reference audio, an aspect ratio, a MiniMax file reference)
    #[tokio::test]
    async fn rejects_what_v1_has_no_field_for_before_sending() {
        let server = untouched().await;
        let m = hailuo(&server.base);
        let mut video_ref = request(&m, "x");
        video_ref.references = vec![typed("https://example.test/m.mp4", "video/mp4")];
        let mut audio_ref = request(&m, "x");
        audio_ref.references = vec![typed("https://example.test/v.mp3", "audio/mpeg")];
        let mut aspect = request(&m, "x");
        aspect.aspect = Some("16:9".into());
        let mut file = request(&m, "x");
        file.start = Some(MediaAsset::reference("minimax", "file_1", None));
        for (name, r, tag) in [
            ("a reference video", video_ref, "UnsupportedOperation"),
            ("reference audio", audio_ref, "UnsupportedOperation"),
            ("an aspect ratio", aspect, "UnsupportedOperation"),
            ("a MiniMax file reference", file, "InvalidRequest"),
        ] {
            assert_eq!(start(r).await.unwrap_err().tag(), tag, "{name}");
        }
        assert!(server.requests().is_empty());
    }

    // minimax-video.test.ts: "fails a submit that v1 answers with a non-zero base_resp"
    #[tokio::test]
    async fn fails_a_submit_that_v1_answers_with_a_non_zero_base_resp() {
        let body = json!({
            "task_id": "",
            "base_resp": { "status_code": 2013, "status_msg": "invalid params, model does not support Text-to-Video mode" },
        });
        let server = reply(body.clone()).await;
        let error = start(request(&hailuo(&server.base), "x")).await.unwrap_err();
        assert_eq!(error.tag(), "InvalidRequest");
        assert_eq!(error.message, "MiniMax Hailuo Video generation failed (2013): invalid params, model does not support Text-to-Video mode");
        assert_eq!(error.body, Some(body.to_string()));
    }

    // minimax-video.test.ts: "reads statuses from a JSON round-tripped token and keeps unknown ones running"
    #[tokio::test]
    async fn reads_statuses_from_a_json_round_tripped_token_and_keeps_unknown_ones_running() {
        let raw: String = serde_json::from_value(json!("task_1")).unwrap();
        let mut statuses = vec![];
        let mut paths = vec![];
        for s in ["queued", "running", "succeeded", "failed", "cancelled", "warming_up", "constructor"] {
            let server = reply(task(json!({ "status": s }))).await;
            statuses.push(status(&h3(&server.base), &raw).await.unwrap());
            paths.extend(lines(&server));
        }
        assert_eq!(statuses, [running(), running(), completed(), errored(Reason::Failed), errored(Reason::Cancelled), running(), running()]);
        assert_eq!(paths[0], "GET /v2/query/video_generation/task_1");
    }

    // minimax-video.test.ts: "returns the finished video with duration, usage and metadata"
    #[tokio::test]
    async fn returns_the_finished_video_with_duration_usage_and_metadata() {
        let server = reply(succeeded()).await;
        let response = result(&h3(&server.base), "task_1").await.unwrap();
        let video = &response.videos[0];
        assert_eq!(video.asset.source, url_source("https://cdn.minimax.test/out.mp4", "video/mp4"));
        assert_eq!(video.expires_at, None);
        assert_eq!(video.asset.info, Some(MediaInfo { duration_seconds: Some(4.0), ..Default::default() }));
        assert_eq!(response.usage, Some(VideoUsage::Seconds(4.0)));
        assert_eq!(
            response.provider_metadata,
            Some(obj(json!({
                "minimax": {
                    "taskId": "task_1",
                    "model": "MiniMax-H3-Lite",
                    "resolution": "480P",
                    "ratio": "16:9",
                    "usage": { "total_seconds": 4, "output_seconds": 4, "input_image_count": 0 },
                },
            })))
        );
    }

    // minimax-video.test.ts: "reads the flat response format too"
    #[tokio::test]
    async fn reads_the_flat_response_format_too() {
        let server = reply(succeeded()["task"].clone()).await;
        let response = result(&h3(&server.base), "task_1").await.unwrap();
        assert_eq!(response.videos[0].asset.url_value(), Some("https://cdn.minimax.test/out.mp4"));
    }

    // minimax-video.test.ts: "fails a result read before the task finishes"
    #[tokio::test]
    async fn fails_a_result_read_before_the_task_finishes() {
        let error = result_error(h3, "task_1", &task(json!({ "status": "running" }))).await;
        assert_eq!(error.tag(), "InvalidRequest");
        assert!(error.message.contains("has not finished"), "{}", error.message);
    }

    // minimax-video.test.ts: "fails the result of ${terminal.message} with the provider body" (H3)
    #[tokio::test]
    async fn fails_the_h3_result_with_the_provider_body() {
        let failed = |code: &str, message: &str| task(json!({ "status": "failed", "error": { "code": code, "message": message } }));
        let cases = [
            (
                failed("1026", "video description contains sensitive content"),
                "ContentPolicy",
                "MiniMax Video generation failed (1026): video description contains sensitive content",
            ),
            (failed("1027", "output contains sensitive content"), "ContentPolicy", "MiniMax Video generation failed (1027): output contains sensitive content"),
            (failed("2013", "invalid params"), "InvalidRequest", "MiniMax Video generation failed (2013): invalid params"),
            (failed("1008", "insufficient balance"), "QuotaExceeded", "MiniMax Video generation failed (1008): insufficient balance"),
            (failed("constructor", "future"), "ProviderInternal", "MiniMax Video generation failed (constructor): future"),
            (task(json!({ "status": "cancelled" })), "InvalidRequest", "MiniMax Video task task_1 was cancelled"),
            (task(json!({ "status": "succeeded" })), "InvalidProviderOutput", "MiniMax Video completed without a video URL"),
        ];
        for (body, tag, message) in cases {
            let error = result_error(h3, "task_1", &body).await;
            assert_eq!((error.tag(), error.message.as_str()), (tag, message));
        }
    }

    // minimax-video.test.ts: "cancels with DELETE on the task"
    #[tokio::test]
    async fn cancels_with_delete_on_the_task() {
        let server = reply(json!({ "task_id": "task_1", "action": "cancelled" })).await;
        video::cancel(&client(), &h3(&server.base), "task_1").await.unwrap();
        assert_eq!(lines(&server), ["DELETE /v2/video_generation/task_1"]);
    }

    // minimax-video.test.ts: "maps v1 statuses and keeps unknown ones running"
    #[tokio::test]
    async fn maps_v1_statuses_and_keeps_unknown_ones_running() {
        let mut statuses = vec![];
        let mut paths = vec![];
        for s in ["Preparing", "Queueing", "Processing", "Success", "Fail", "Warming"] {
            let server = reply(hailuo_task(json!({ "status": s }))).await;
            statuses.push(status(&hailuo(&server.base), "task_1").await.unwrap());
            paths.extend(lines(&server));
        }
        assert_eq!(statuses, [running(), running(), running(), completed(), errored(Reason::Failed), running()]);
        assert_eq!(paths[0], "GET /v1/query/video_generation?task_id=task_1");
    }

    // minimax-video.test.ts: "fails a status read that v1 answers with a non-zero base_resp"
    #[tokio::test]
    async fn fails_a_status_read_that_v1_answers_with_a_non_zero_base_resp() {
        let server = reply(hailuo_task(json!({
            "status": "",
            "file_id": "",
            "base_resp": { "status_code": 2013, "status_msg": "invalid params, This task_id does not exist" },
        })))
        .await;
        let error = status(&hailuo(&server.base), "task_1").await.unwrap_err();
        assert_eq!(error.tag(), "InvalidRequest");
        assert!(error.message.contains("This task_id does not exist"), "{}", error.message);
    }

    // minimax-video.test.ts: "looks up the file's download URL, which expires after one hour"
    #[tokio::test]
    async fn looks_up_the_files_download_url_which_expires_after_one_hour() {
        let server = mock(|r, _| {
            if r.path.contains("/query/") {
                return Resp::json(200, hailuo_task(json!({ "status": "Success", "file_id": "file_9", "video_width": 752, "video_height": 1360 })));
            }
            Resp::json(
                200,
                json!({
                    "file": { "file_id": 9, "bytes": 1024, "download_url": "https://cdn.minimax.test/out.mp4?sig=1" },
                    "base_resp": { "status_code": 0, "status_msg": "success" },
                }),
            )
        })
        .await;
        let before = now_ms();
        let response = result(&hailuo(&server.base), "task_1").await.unwrap();
        let after = now_ms();
        assert_eq!(lines(&server), ["GET /v1/query/video_generation?task_id=task_1", "GET /v1/files/retrieve?file_id=file_9"]);
        assert_eq!(server.requests()[1].header("authorization"), Some("Bearer test"));
        let video = &response.videos[0];
        assert_eq!(video.asset.source, url_source("https://cdn.minimax.test/out.mp4?sig=1", "video/mp4"));
        // Upstream pins the clock at 0 and expects 60 * 60 * 1000.
        let expires = video.expires_at.unwrap();
        assert!(expires >= before + 3_600_000.0 && expires <= after + 3_600_000.0, "{expires}");
        assert_eq!(video.asset.info, Some(MediaInfo { width: Some(752.0), height: Some(1360.0), ..Default::default() }));
        assert_eq!(response.provider_metadata, Some(obj(json!({ "minimax": { "taskId": "task_1", "fileId": "file_9" } }))));
    }

    // minimax-video.test.ts: "fails the result with \"${terminal.message}\"" (Hailuo)
    #[tokio::test]
    async fn fails_the_hailuo_result_with_the_provider_body() {
        let cases = [
            (
                hailuo_task(json!({ "status": "Fail", "base_resp": { "status_code": 1027, "status_msg": "output sensitive" } })),
                "ContentPolicy",
                "MiniMax Hailuo Video generation failed (1027): output sensitive",
            ),
            (hailuo_task(json!({ "status": "Fail" })), "ProviderInternal", "MiniMax Hailuo Video generation failed"),
            (
                hailuo_task(json!({ "status": "Processing" })),
                "InvalidRequest",
                "MiniMax Hailuo Video generation task_1 has not finished; await it before reading the result",
            ),
            (hailuo_task(json!({ "status": "Success", "file_id": "" })), "InvalidProviderOutput", "MiniMax Hailuo Video completed without a file_id"),
        ];
        for (body, tag, message) in cases {
            let error = result_error(hailuo, "task_1", &body).await;
            assert_eq!((error.tag(), error.message.as_str()), (tag, message));
        }
    }

    // minimax-video.test.ts: "fails cancel as unsupported without sending a request, since v1 has no cancel endpoint"
    #[tokio::test]
    async fn fails_cancel_as_unsupported_without_sending_a_request_since_v1_has_no_cancel_endpoint() {
        let server = untouched().await;
        let error = video::cancel(&client(), &hailuo(&server.base), "task_1").await.unwrap_err();
        assert_eq!(error.tag(), "UnsupportedOperation");
        assert!(error.message.contains("cancel"), "{}", error.message);
        assert!(server.requests().is_empty());
    }
}

// =============================================================================================================
// openrouter-video.test.ts
// =============================================================================================================

mod openrouter {
    use super::*;

    const ID: &str = "gen-vid-1-abc";

    fn model(base: &str) -> VideoModel {
        super::model("openrouter", "x-ai/grok-imagine-video-1.5-lite", &format!("{base}/api/v1"))
    }

    fn started(host: &str) -> Value {
        json!({ "id": ID, "polling_url": format!("http://{host}/api/v1/videos/{ID}"), "status": "pending" })
    }

    /// `completed`, with the content URL on the server that answered.
    fn completed_on(host: &str) -> Value {
        json!({
            "id": ID,
            "generation_id": ID,
            "status": "completed",
            "unsigned_urls": [format!("http://{host}/api/v1/videos/{ID}/content?index=0")],
            "usage": { "cost": 0.03, "is_byok": false },
            "expires_at": 1794633487u64,
        })
    }

    fn host(r: &Req) -> String {
        r.header("host").unwrap().to_string()
    }

    async fn record() -> Mock {
        mock(|r, _| Resp::json(200, started(&host(r)))).await
    }

    // openrouter-video.test.ts: "maps the request onto OpenRouter's body and returns the job without polling"
    #[tokio::test]
    async fn maps_the_request_onto_openrouters_body_and_returns_the_job_without_polling() {
        let server = record().await;
        let mut r = request(&model(&server.base), "A red balloon floats up");
        r.start = Some(video::image(data("data:image/png;base64,AQID")).unwrap());
        r.end = Some(url("https://example.test/last.png"));
        r.duration = Some(6.0);
        r.aspect = Some("16:9".into());
        r.resolution = Some("4k".into());
        r.sound = Some(false);
        r.provider_options = Some(obj(json!({ "provider": { "only": ["seed"], "options": { "seed": { "watermark": false } } }, "seed": 7 })));
        assert_eq!(start(r).await.unwrap(), ID);
        let calls = server.requests();
        assert_eq!(lines(&server), ["POST /api/v1/videos"]);
        assert_eq!(calls[0].header("authorization"), Some("Bearer test"));
        assert_eq!(
            calls[0].json(),
            json!({
                "model": "x-ai/grok-imagine-video-1.5-lite",
                "prompt": "A red balloon floats up",
                "frame_images": [
                    { "type": "image_url", "image_url": { "url": "data:image/png;base64,AQID" }, "frame_type": "first_frame" },
                    { "type": "image_url", "image_url": { "url": "https://example.test/last.png" }, "frame_type": "last_frame" },
                ],
                "duration": 6,
                "aspect_ratio": "16:9",
                "resolution": "4K",
                "generate_audio": false,
                "provider": { "only": ["seed"], "options": { "seed": { "watermark": false } } },
                "seed": 7,
            })
        );
    }

    // openrouter-video.test.ts: "sends image, video and audio references by kind and leaves aspect out for auto"
    #[tokio::test]
    async fn sends_image_video_and_audio_references_by_kind_and_leaves_aspect_out_for_auto() {
        let server = record().await;
        let mut r = request(&model(&server.base), "x");
        r.aspect = Some("auto".into());
        r.references = vec![
            data("data:image/png;base64,AQID"),
            typed("https://example.test/motion.mp4", "video/mp4"),
            typed("https://example.test/voice.mp3", "audio/mpeg"),
        ];
        start(r).await.unwrap();
        assert_eq!(
            server.requests()[0].json(),
            json!({
                "model": "x-ai/grok-imagine-video-1.5-lite",
                "prompt": "x",
                "input_references": [
                    { "type": "image_url", "image_url": { "url": "data:image/png;base64,AQID" } },
                    { "type": "video_url", "video_url": { "url": "https://example.test/motion.mp4" } },
                    { "type": "audio_url", "audio_url": { "url": "https://example.test/voice.mp3" } },
                ],
            })
        );
    }

    // openrouter-video.test.ts: "sends a start frame and references together"
    #[tokio::test]
    async fn sends_a_start_frame_and_references_together() {
        let server = record().await;
        let mut r = request(&model(&server.base), "x");
        r.start = Some(url("https://example.test/first.png"));
        r.references = vec![url("https://example.test/style.png")];
        start(r).await.unwrap();
        assert_eq!(
            server.requests()[0].json(),
            json!({
                "model": "x-ai/grok-imagine-video-1.5-lite",
                "prompt": "x",
                "frame_images": [
                    { "type": "image_url", "image_url": { "url": "https://example.test/first.png" }, "frame_type": "first_frame" },
                ],
                "input_references": [{ "type": "image_url", "image_url": { "url": "https://example.test/style.png" } }],
            })
        );
    }

    // openrouter-video.test.ts: "passes a previous job through to edit or extend it"
    #[tokio::test]
    async fn passes_a_previous_job_through_to_edit_or_extend_it() {
        let server = record().await;
        let mut r = request(&model(&server.base), "keep going");
        r.provider_options = Some(obj(json!({ "previous_job_id": "gen-vid-1-prev" })));
        start(r).await.unwrap();
        assert_eq!(server.requests()[0].json()["previous_job_id"], json!("gen-vid-1-prev"));
    }

    // openrouter-video.test.ts: "rejects what OpenRouter can't use before sending"
    #[tokio::test]
    async fn rejects_what_openrouter_cant_use_before_sending() {
        let server = untouched().await;
        let mut r = request(&model(&server.base), "x");
        r.references = vec![MediaAsset::bytes(vec![0, 0, 0], Some("video/mp4"))];
        let inline = start(r).await.unwrap_err();
        assert_eq!(inline.tag(), "UnsupportedOperation");
        assert!(inline.message.contains("only takes reference video as http(s) URLs"), "{}", inline.message);
        let mut r = request(&model(&server.base), "x");
        r.references = vec![MediaAsset::bytes(vec![0x49, 0x44, 0x33], Some("audio/mpeg"))];
        let audio = start(r).await.unwrap_err();
        assert_eq!(audio.tag(), "UnsupportedOperation");
        assert!(audio.message.contains("only takes reference audio as http(s) URLs"), "{}", audio.message);
        assert!(server.requests().is_empty());
    }

    // openrouter-video.test.ts: "maps OpenRouter statuses, keeping unknown ones running"
    #[tokio::test]
    async fn maps_openrouter_statuses_keeping_unknown_ones_running() {
        let mut statuses = vec![];
        for s in ["pending", "in_progress", "completed", "failed", "cancelled", "expired", "warming_up"] {
            let server = reply(json!({ "id": ID, "status": s })).await;
            statuses.push(status(&model(&server.base), ID).await.unwrap());
        }
        assert_eq!(statuses, [running(), running(), completed(), errored(Reason::Failed), errored(Reason::Cancelled), errored(Reason::Expired), running()]);
    }

    // openrouter-video.test.ts: "returns the content URL with the API key to download it, its expiry and the cost"
    #[tokio::test]
    async fn returns_the_content_url_with_the_api_key_to_download_it_its_expiry_and_the_cost() {
        let server = mock(|r, _| {
            if r.path.ends_with("/content") && r.query == "index=0" {
                return Resp { status: 200, headers: vec![("content-type".into(), "video/mp4".into())], body: "\u{1}\u{2}\u{3}".into() };
            }
            Resp::json(200, completed_on(&host(r)))
        })
        .await;
        let response = result(&model(&server.base), ID).await.unwrap();
        let video = &response.videos[0];
        assert_eq!(collect(video).await.concat(), [1, 2, 3]);
        assert_eq!(lines(&server), [format!("GET /api/v1/videos/{ID}"), format!("GET /api/v1/videos/{ID}/content?index=0")]);
        assert_eq!(server.requests()[1].header("authorization"), Some("Bearer test"));
        let content = format!("{}/api/v1/videos/{ID}/content?index=0", server.base);
        assert_eq!(video.asset.source, url_source(&content, "video/mp4"));
        assert_eq!(video.expires_at, Some(1794633487000.0));
        assert_eq!(response.provider_metadata, Some(obj(json!({ "openrouter": { "id": ID, "generationId": ID, "cost": 0.03, "isByok": false } }))));
    }

    // openrouter-video.test.ts: "keeps the caller's scoped middleware for a download on the provider's host, and drops it on another"
    // Skipped: Rust has no scoped request middleware; the credential half (provider host only) is covered by the
    // cases around it.

    // openrouter-video.test.ts: "attaches no credentials to a result URL on another host"
    #[tokio::test]
    async fn attaches_no_credentials_to_a_result_url_on_another_host() {
        let server = mock(|r, _| {
            let mut body = completed_on(&host(r));
            body["unsigned_urls"] = json!(["https://cdn.example.test/video.mp4"]);
            Resp::json(200, body)
        })
        .await;
        let response = result(&model(&server.base), ID).await.unwrap();
        assert_eq!(response.videos[0].headers, None);
        assert_eq!(server.requests().len(), 1);
    }

    // openrouter-video.test.ts: "fails a result read before the job finishes"
    #[tokio::test]
    async fn fails_a_result_read_before_the_job_finishes() {
        let server = reply(json!({ "id": ID, "status": "in_progress" })).await;
        let error = result(&model(&server.base), ID).await.unwrap_err();
        assert_eq!(error.tag(), "InvalidRequest");
        assert!(error.message.contains("has not finished"), "{}", error.message);
    }

    // openrouter-video.test.ts: "fails the result of a job that ${terminal.body.status} with \"${terminal.body.error}\""
    #[tokio::test]
    async fn fails_the_result_of_a_terminal_job_with_its_error() {
        let cases = [
            ("failed", "Upstream timed out", "ProviderInternal", "OpenRouter Video generation failed: Upstream timed out"),
            ("failed", "Content policy violation", "ContentPolicy", "OpenRouter Video generation failed: Content policy violation"),
            ("expired", "Job exceeded maximum time to live", "InvalidRequest", "OpenRouter Video job gen-vid-1-abc expired: Job exceeded maximum time to live"),
            ("cancelled", "Job was cancelled", "InvalidRequest", "OpenRouter Video job gen-vid-1-abc cancelled: Job was cancelled"),
        ];
        for (s, e, tag, message) in cases {
            let server = reply(json!({ "id": ID, "status": s, "error": e })).await;
            let error = result(&model(&server.base), ID).await.unwrap_err();
            assert_eq!((error.tag(), error.message.as_str()), (tag, message));
        }
    }

    // openrouter-video.test.ts: "fails cancel as unsupported without a request, since OpenRouter has no cancel endpoint"
    #[tokio::test]
    async fn fails_cancel_as_unsupported_without_a_request_since_openrouter_has_no_cancel_endpoint() {
        let server = untouched().await;
        let error = video::cancel(&client(), &model(&server.base), ID).await.unwrap_err();
        assert_eq!(error.tag(), "UnsupportedOperation");
        assert!(server.requests().is_empty());
    }
}

// =============================================================================================================
// seedance-video.test.ts
// =============================================================================================================

mod seedance {
    use super::*;

    const ID: &str = "cgt-1";

    fn model(base: &str) -> VideoModel {
        super::model("volcengine", "doubao-seedance-2-0-260128", &format!("{base}/api/v3"))
    }

    fn succeeded() -> Value {
        json!({
            "id": "cgt-1",
            "model": "doubao-seedance-2-0-260128",
            "status": "succeeded",
            "content": { "video_url": "https://ark-content.test/out.mp4" },
            "usage": { "completion_tokens": 108900, "total_tokens": 108900 },
            "created_at": 1779348818u64,
            "updated_at": 1779348874u64,
            "seed": 78674,
            "resolution": "720p",
            "ratio": "16:9",
            "duration": 5,
            "framespersecond": 24,
            "service_tier": "default",
            "generate_audio": true,
            "draft": false,
        })
    }

    async fn submitted(id: &'static str) -> Mock {
        mock(move |_, _| Resp::json(200, json!({ "id": id }))).await
    }

    // seedance-video.test.ts: "maps frames, shared fields and provider options onto Ark's content[] body"
    #[tokio::test]
    async fn maps_frames_shared_fields_and_provider_options_onto_arks_content_body() {
        let server = submitted("cgt-1").await;
        let mut r = request(&model(&server.base), "The kitten yawns at the camera");
        r.start = Some(video::image(data("data:image/png;base64,AQID")).unwrap());
        r.end = Some(url("https://example.test/last.png"));
        r.duration = Some(5.0);
        r.aspect = Some("16:9".into());
        r.resolution = Some("720p".into());
        r.sound = Some(false);
        r.provider_options = Some(obj(json!({ "watermark": true, "return_last_frame": true, "future_option": 1 })));
        r.http = Some(http(&[("x-request", "yes")], &[("trace", "1")]));
        assert_eq!(start(r).await.unwrap(), "cgt-1");
        let calls = server.requests();
        assert_eq!(lines(&server), ["POST /api/v3/contents/generations/tasks?trace=1"]);
        assert_eq!(calls[0].header("authorization"), Some("Bearer test"));
        assert_eq!(calls[0].header("x-request"), Some("yes"));
        assert_eq!(
            calls[0].json(),
            json!({
                "model": "doubao-seedance-2-0-260128",
                "content": [
                    { "type": "text", "text": "The kitten yawns at the camera" },
                    { "type": "image_url", "image_url": { "url": "data:image/png;base64,AQID" }, "role": "first_frame" },
                    { "type": "image_url", "image_url": { "url": "https://example.test/last.png" }, "role": "last_frame" },
                ],
                "duration": 5,
                "ratio": "16:9",
                "resolution": "720p",
                "generate_audio": false,
                "watermark": true,
                "return_last_frame": true,
                "future_option": 1,
            })
        );
    }

    // seedance-video.test.ts: "maps image, video and audio references to their roles, with own assets as asset:// URIs"
    #[tokio::test]
    async fn maps_image_video_and_audio_references_to_their_roles_with_own_assets_as_asset_uris() {
        let server = submitted("cgt-2").await;
        let mut r = request(&model(&server.base), "Match @Image1 to the motion of @Video1");
        r.references = vec![
            MediaAsset::reference("volcengine", "asset-123", None),
            typed("https://example.test/style.png", "image/png"),
            typed("https://example.test/motion.mp4", "video/mp4"),
            typed("https://example.test/voice.mp3", "audio/mpeg"),
        ];
        start(r).await.unwrap();
        assert_eq!(
            server.requests()[0].json(),
            json!({
                "model": "doubao-seedance-2-0-260128",
                "content": [
                    { "type": "text", "text": "Match @Image1 to the motion of @Video1" },
                    { "type": "image_url", "image_url": { "url": "asset://asset-123" }, "role": "reference_image" },
                    { "type": "image_url", "image_url": { "url": "https://example.test/style.png" }, "role": "reference_image" },
                    { "type": "video_url", "video_url": { "url": "https://example.test/motion.mp4" }, "role": "reference_video" },
                    { "type": "audio_url", "audio_url": { "url": "https://example.test/voice.mp3" }, "role": "reference_audio" },
                ],
            })
        );
    }

    // seedance-video.test.ts: "sends aspect auto as adaptive and leaves unset fields out"
    #[tokio::test]
    async fn sends_aspect_auto_as_adaptive_and_leaves_unset_fields_out() {
        let server = submitted("cgt-3").await;
        let mut auto = request(&model(&server.base), "x");
        auto.aspect = Some("auto".into());
        start(auto).await.unwrap();
        start(request(&model(&server.base), "x")).await.unwrap();
        assert_eq!(
            server.requests().iter().map(Req::json).collect::<Vec<_>>(),
            [
                json!({ "model": "doubao-seedance-2-0-260128", "content": [{ "type": "text", "text": "x" }], "ratio": "adaptive" }),
                json!({ "model": "doubao-seedance-2-0-260128", "content": [{ "type": "text", "text": "x" }] }),
            ]
        );
    }

    // seedance-video.test.ts: "sends frames and references together for the provider to judge"
    #[tokio::test]
    async fn sends_frames_and_references_together_for_the_provider_to_judge() {
        let server = submitted("cgt-4").await;
        let mut r = request(&model(&server.base), "x");
        r.start = Some(url("https://example.test/first.png"));
        r.references = vec![typed("https://example.test/ref.png", "image/png")];
        start(r).await.unwrap();
        let body = server.requests()[0].json();
        let roles: Vec<Value> = body["content"].as_array().unwrap().iter().map(|item| item.get("role").cloned().unwrap_or(Value::Null)).collect();
        assert_eq!(roles, [Value::Null, json!("first_frame"), json!("reference_image")]);
    }

    // seedance-video.test.ts: "adds providerOptions.draft_task as a draft_task content item and omits an empty prompt"
    #[tokio::test]
    async fn adds_provider_options_draft_task_as_a_draft_task_content_item_and_omits_an_empty_prompt() {
        let server = submitted("cgt-5").await;
        let mut r = request(&model(&server.base), "");
        r.provider_options = Some(obj(json!({ "draft_task": { "id": "cgt-draft" }, "resolution": "720p" })));
        start(r).await.unwrap();
        assert_eq!(
            server.requests()[0].json(),
            json!({
                "model": "doubao-seedance-2-0-260128",
                "content": [{ "type": "draft_task", "draft_task": { "id": "cgt-draft" } }],
                "resolution": "720p",
            })
        );
    }

    // seedance-video.test.ts: "rejects a reference owned by another provider before sending"
    #[tokio::test]
    async fn rejects_a_reference_owned_by_another_provider_before_sending() {
        let server = untouched().await;
        let mut r = request(&model(&server.base), "x");
        r.references = vec![MediaAsset::reference("xai", "file_1", None)];
        let error = start(r).await.unwrap_err();
        assert_eq!(error.tag(), "InvalidRequest");
        assert!(error.message.contains("volcengine references"), "{}", error.message);
        assert!(server.requests().is_empty());
    }

    // seedance-video.test.ts: "uses each provider's Ark host and its own provider ID"
    // The default hosts are real, so this renders each submit's URL, auth headers and body instead of sending it.
    #[tokio::test]
    async fn uses_each_providers_ark_host_and_its_own_provider_id() {
        use oc_ai::route::auth::{AuthInput, apply};
        let byteplus = configured("byteplus", "dreamina-seedance-2-0-260128", "bp", None);
        let volcengine = configured("volcengine", "doubao-seedance-2-0-260128", "ark", None);
        assert_eq!(byteplus.provider, "byteplus");
        assert_eq!(volcengine.provider, "volcengine");
        let mut bp = request(&byteplus, "x");
        bp.references = vec![MediaAsset::reference("byteplus", "a1", None)];
        let ark = request(&volcengine, "x");
        let mut sent = vec![];
        for r in [&bp, &ark] {
            let url = r.model.route.endpoint.render(r).unwrap().to_string();
            let headers = apply(r.model.route.auth.as_ref(), AuthInput { method: "POST", url: &url, body: "", headers: Default::default() }).await.unwrap();
            sent.push((url, headers.get("authorization").cloned()));
        }
        assert_eq!(
            sent,
            [
                ("https://ark.ap-southeast.bytepluses.com/api/v3/contents/generations/tasks".to_string(), Some("Bearer bp".to_string())),
                ("https://ark.cn-beijing.volces.com/api/v3/contents/generations/tasks".to_string(), Some("Bearer ark".to_string())),
            ]
        );
        let body = video::body(&bp).unwrap();
        assert_eq!(body.json_value().unwrap()["content"][1]["image_url"]["url"], json!("asset://a1"));
    }

    // seedance-video.test.ts: "reads one status per call from a JSON round-tripped token"
    #[tokio::test]
    async fn reads_one_status_per_call_from_a_json_round_tripped_token() {
        let server = mock(|r, n| {
            if r.method == "POST" {
                return Resp::json(200, json!({ "id": "cgt-1" }));
            }
            match n {
                0 => Resp::json(200, json!({ "id": "cgt-1", "status": "queued" })),
                1 => Resp::json(200, json!({ "id": "cgt-1", "status": "running" })),
                _ => Resp::json(200, succeeded()),
            }
        })
        .await;
        let m = model(&server.base);
        let id = start(request(&m, "x")).await.unwrap();
        let restored: String = serde_json::from_value(json!(id)).unwrap();
        let mut statuses = vec![];
        for _ in 0..3 {
            statuses.push(status(&m, &restored).await.unwrap());
        }
        assert_eq!(statuses, [running(), running(), completed()]);
        assert_eq!(
            lines(&server),
            [
                "POST /api/v3/contents/generations/tasks",
                "GET /api/v3/contents/generations/tasks/cgt-1",
                "GET /api/v3/contents/generations/tasks/cgt-1",
                "GET /api/v3/contents/generations/tasks/cgt-1",
            ]
        );
    }

    // seedance-video.test.ts: "maps every documented status and keeps unknown ones running"
    #[tokio::test]
    async fn maps_every_documented_status_and_keeps_unknown_ones_running() {
        let mut statuses = vec![];
        for s in ["queued", "running", "succeeded", "failed", "cancelled", "expired", "paused", "constructor"] {
            let server = reply(json!({ "id": "cgt-1", "status": s })).await;
            statuses.push(status(&model(&server.base), ID).await.unwrap());
        }
        assert_eq!(
            statuses,
            [running(), running(), completed(), errored(Reason::Failed), errored(Reason::Cancelled), errored(Reason::Expired), running(), running(),]
        );
    }

    // seedance-video.test.ts: "returns the video as a URL that expires 24 hours after the task finished, with usage and metadata"
    #[tokio::test]
    async fn returns_the_video_as_a_url_that_expires_24_hours_after_the_task_finished_with_usage_and_metadata() {
        let server = reply(succeeded()).await;
        let response = result(&model(&server.base), ID).await.unwrap();
        let video = &response.videos[0];
        assert_eq!(video.asset.source, url_source("https://ark-content.test/out.mp4", "video/mp4"));
        assert_eq!(video.expires_at, Some((1779348874.0 + 24.0 * 60.0 * 60.0) * 1000.0));
        assert_eq!(video.asset.info, Some(MediaInfo { duration_seconds: Some(5.0), ..Default::default() }));
        assert_eq!(
            response.usage,
            Some(VideoUsage::Tokens(oc_ai::image::TokenUsage { input: None, output: Some(108900.0), total: Some(108900.0), details: None }))
        );
        assert_eq!(
            response.provider_metadata,
            Some(obj(json!({
                "volcengine": {
                    "taskId": "cgt-1",
                    "model": "doubao-seedance-2-0-260128",
                    "seed": 78674,
                    "ratio": "16:9",
                    "resolution": "720p",
                    "duration": 5,
                    "framesPerSecond": 24,
                    "generateAudio": true,
                    "draft": false,
                    "serviceTier": "default",
                },
            })))
        );
    }

    // seedance-video.test.ts: "derives duration from frames, types mov output and keeps the last frame URL"
    #[tokio::test]
    async fn derives_duration_from_frames_types_mov_output_and_keeps_the_last_frame_url() {
        let server = reply(json!({
            "status": "succeeded",
            "content": { "video_url": "https://ark-content.test/out.mov", "last_frame_url": "https://ark-content.test/l.jpg" },
            "frames": 57,
            "framespersecond": 24,
            "output_format": "mov",
        }))
        .await;
        let response = result(&model(&server.base), ID).await.unwrap();
        let video = &response.videos[0];
        assert_eq!(video.media_type(), "video/quicktime");
        assert_eq!(video.asset.info, Some(MediaInfo { duration_seconds: Some(57.0 / 24.0), ..Default::default() }));
        assert!(video.expires_at.unwrap() > 0.0);
        let metadata = &response.provider_metadata.unwrap()["volcengine"];
        assert_eq!(metadata["frames"], json!(57));
        assert_eq!(metadata["outputFormat"], json!("mov"));
        assert_eq!(metadata["lastFrameUrl"], json!("https://ark-content.test/l.jpg"));
    }

    // seedance-video.test.ts: "fails a result read before the task finishes"
    #[tokio::test]
    async fn fails_a_result_read_before_the_task_finishes() {
        let mut errors = vec![];
        for s in ["queued", "running"] {
            let server = reply(json!({ "id": "cgt-1", "status": s })).await;
            errors.push(result(&model(&server.base), ID).await.unwrap_err());
        }
        assert_eq!(errors.iter().map(AIError::tag).collect::<Vec<_>>(), ["InvalidRequest", "InvalidRequest"]);
        assert_eq!(errors[0].message, "Seedance Video generation cgt-1 has not finished; await it before reading the result");
    }

    // seedance-video.test.ts: "fails the result of ${terminal.body.error?.code ?? terminal.body.status} tasks with the body"
    #[tokio::test]
    async fn fails_the_result_of_terminal_tasks_with_the_body() {
        let failed = |code: &str, message: &str| json!({ "status": "failed", "error": { "code": code, "message": message } });
        let cases = [
            (
                failed("InvalidParameter.TaskTypeConstraint", "ratio must be adaptive."),
                "InvalidRequest",
                "Seedance Video generation failed (InvalidParameter.TaskTypeConstraint): ratio must be adaptive.",
            ),
            (failed("AuthenticationError", "Bad key."), "Authentication", "Seedance Video generation failed (AuthenticationError): Bad key."),
            (failed("QuotaExceeded", "Too many queued tasks."), "RateLimit", "Seedance Video generation failed (QuotaExceeded): Too many queued tasks."),
            (failed("InternalServiceError", "Retry later."), "ProviderInternal", "Seedance Video generation failed (InternalServiceError): Retry later."),
            (failed("constructor", "Future code."), "ProviderInternal", "Seedance Video generation failed (constructor): Future code."),
            (
                failed("OutputVideoSensitiveContentDetected", "The output video may be sensitive."),
                "ContentPolicy",
                "Seedance Video generation failed (OutputVideoSensitiveContentDetected): The output video may be sensitive.",
            ),
            (
                failed("InputImageSensitiveContentDetected.PrivacyInformation", "Real person."),
                "ContentPolicy",
                "Seedance Video generation failed (InputImageSensitiveContentDetected.PrivacyInformation): Real person.",
            ),
            (json!({ "status": "cancelled" }), "InvalidRequest", "Seedance Video task cgt-1 was cancelled"),
            (json!({ "status": "expired" }), "InvalidRequest", "Seedance Video task cgt-1 expired"),
            (json!({ "status": "succeeded", "content": {} }), "InvalidProviderOutput", "Seedance Video completed without a video URL"),
        ];
        for (body, tag, message) in cases {
            let error = result_error(model, ID, &body).await;
            assert_eq!(error.tag(), tag, "{message}");
            assert!(error.message.contains(message), "{} / {message}", error.message);
        }
    }

    // seedance-video.test.ts: "cancels with DELETE on the task and does not retry a refusal"
    #[tokio::test]
    async fn cancels_with_delete_on_the_task_and_does_not_retry_a_refusal() {
        let accepted = reply(json!({})).await;
        video::cancel(&client(), &model(&accepted.base), ID).await.unwrap();
        let refusing = mock(|_, _| Resp::json(400, json!({ "error": { "code": "InvalidParameter", "message": "running" } }))).await;
        let refused = video::cancel(&client(), &model(&refusing.base), ID).await.unwrap_err();
        assert_eq!(refused.tag(), "InvalidRequest");
        let calls: Vec<String> = lines(&accepted).into_iter().chain(lines(&refusing)).collect();
        assert_eq!(calls, ["DELETE /api/v3/contents/generations/tasks/cgt-1", "DELETE /api/v3/contents/generations/tasks/cgt-1"]);
    }
}

// =============================================================================================================
// google-interactions-video.test.ts
// =============================================================================================================

mod google {
    use super::*;

    const ID: &str = "v1_abc";

    fn model(base: &str) -> VideoModel {
        super::model("google", "gemini-omni-1.2-flash", &format!("{base}/v1beta"))
    }

    fn started() -> Value {
        json!({ "id": "v1_abc", "status": "in_progress", "object": "interaction", "model": "gemini-omni-1.2-flash" })
    }

    // "AAAAIGZ0eXBpc29t" is the base64 of an MP4 `ftyp` box header.
    fn completed_body() -> Value {
        json!({
            "id": "v1_abc",
            "status": "completed",
            "model": "gemini-omni-1.2-flash",
            "usage": { "total_input_tokens": 17, "total_output_tokens": 5793, "total_thought_tokens": 361, "total_tokens": 6171 },
            "steps": [
                { "type": "user_input", "content": [{ "type": "text", "text": "x" }] },
                { "type": "thought", "signature": "sig" },
                { "type": "model_output", "content": [{ "type": "video", "mime_type": "video/mp4", "data": "AAAAIGZ0eXBpc29t" }] },
            ],
        })
    }

    /// `sent(input)`: the submitted body.
    async fn sent(build: impl FnOnce(&mut VideoRequest)) -> Value {
        let server = reply(started()).await;
        let mut r = request(&model(&server.base), "x");
        build(&mut r);
        assert_eq!(start(r).await.unwrap(), ID);
        server.requests()[0].json()
    }

    fn image() -> MediaAsset {
        data("data:image/png;base64,AQID")
    }

    // google-interactions-video.test.ts: "maps the request onto a background interaction and returns the job without polling"
    #[tokio::test]
    async fn maps_the_request_onto_a_background_interaction_and_returns_the_job_without_polling() {
        let server = reply(started()).await;
        let mut r = request(&model(&server.base), "A balloon drifts upward");
        r.duration = Some(3.0);
        r.aspect = Some("9:16".into());
        r.resolution = Some("4K".into());
        r.sound = Some(true);
        r.provider_options = Some(obj(json!({ "service_tier": "flex", "response_format": { "delivery": "uri" } })));
        r.http = Some(http(&[("x-request", "yes")], &[("trace", "1")]));
        assert_eq!(start(r).await.unwrap(), ID);
        let calls = server.requests();
        assert_eq!(lines(&server), ["POST /v1beta/interactions?trace=1"]);
        assert_eq!(calls[0].header("x-goog-api-key"), Some("test"));
        assert_eq!(calls[0].header("x-request"), Some("yes"));
        assert_eq!(
            calls[0].json(),
            json!({
                "model": "gemini-omni-1.2-flash",
                "input": [{ "type": "text", "text": "A balloon drifts upward" }],
                "background": true,
                "response_format": { "type": "video", "aspect_ratio": "9:16", "resolution": "4k", "duration": "3s", "delivery": "uri" },
                "service_tier": "flex",
            })
        );
    }

    // google-interactions-video.test.ts: "leaves unset fields out instead of filling defaults"
    #[tokio::test]
    async fn leaves_unset_fields_out_instead_of_filling_defaults() {
        let body = sent(|_| {}).await;
        assert_eq!(body, json!({ "model": "gemini-omni-1.2-flash", "input": [{ "type": "text", "text": "x" }], "background": true }));
    }

    // google-interactions-video.test.ts: "leaves the aspect ratio out for auto so the model follows the input"
    #[tokio::test]
    async fn leaves_the_aspect_ratio_out_for_auto_so_the_model_follows_the_input() {
        let body = sent(|r| {
            r.start = Some(image());
            r.aspect = Some("auto".into());
        })
        .await;
        assert_eq!(body.get("response_format"), None);
        let sized = sent(|r| {
            r.aspect = Some("auto".into());
            r.resolution = Some("720p".into());
        })
        .await;
        assert_eq!(sized["response_format"], json!({ "type": "video", "resolution": "720p" }));
    }

    // google-interactions-video.test.ts: "sends start and end as the first two images and pins the frame task"
    #[tokio::test]
    async fn sends_start_and_end_as_the_first_two_images_and_pins_the_frame_task() {
        let body = sent(|r| {
            r.prompt = "Morph".into();
            r.start = Some(video::image(image()).unwrap());
            r.end = Some(
                video::image(MediaAsset::reference("google", "https://generativelanguage.googleapis.com/v1beta/files/last", Some("image/jpeg".into())))
                    .unwrap(),
            );
        })
        .await;
        assert_eq!(
            body["input"],
            json!([
                { "type": "image", "data": "AQID", "mime_type": "image/png" },
                { "type": "image", "uri": "https://generativelanguage.googleapis.com/v1beta/files/last", "mime_type": "image/jpeg" },
                { "type": "text", "text": "Morph" },
            ])
        );
        assert_eq!(body["generation_config"], json!({ "video_config": { "task": "image_to_video" } }));
    }

    // google-interactions-video.test.ts: "sends references as image and video parts and pins the reference task"
    #[tokio::test]
    async fn sends_references_as_image_and_video_parts_and_pins_the_reference_task() {
        let body = sent(|r| {
            r.prompt = "The cat plays".into();
            r.references = vec![image(), typed("https://example.test/motion.mp4", "video/mp4")];
        })
        .await;
        assert_eq!(
            body["input"],
            json!([
                { "type": "image", "data": "AQID", "mime_type": "image/png" },
                { "type": "video", "uri": "https://example.test/motion.mp4", "mime_type": "video/mp4" },
                { "type": "text", "text": "The cat plays" },
            ])
        );
        assert_eq!(body["generation_config"], json!({ "video_config": { "task": "reference_to_video" } }));
    }

    // google-interactions-video.test.ts: "sends frames with references without a task, and lets providerOptions set one"
    #[tokio::test]
    async fn sends_frames_with_references_without_a_task_and_lets_provider_options_set_one() {
        let mixed = sent(|r| {
            r.start = Some(image());
            r.references = vec![image()];
        })
        .await;
        assert_eq!(mixed["input"].as_array().unwrap().len(), 3);
        assert_eq!(mixed.get("generation_config"), None);
        let edit = sent(|r| {
            r.prompt = "Make it anime".into();
            r.references = vec![typed("https://example.test/in.mp4", "video/mp4")];
            r.provider_options = Some(obj(json!({ "generation_config": { "video_config": { "task": "edit" } } })));
        })
        .await;
        assert_eq!(edit["generation_config"], json!({ "video_config": { "task": "edit" } }));
    }

    // google-interactions-video.test.ts: "sends nothing for sound, since Omni always generates audio and has no field for it"
    #[tokio::test]
    async fn sends_nothing_for_sound_since_omni_always_generates_audio_and_has_no_field_for_it() {
        let silent = sent(|r| r.sound = Some(false)).await;
        assert!(!silent.to_string().contains("audio"), "{silent}");
    }

    // google-interactions-video.test.ts: "rejects ${rejected.name}, which Omni has no way to send, before sending"
    // (an end frame without a start frame)
    #[tokio::test]
    async fn rejects_an_end_frame_without_a_start_frame_which_omni_has_no_way_to_send_before_sending() {
        let server = untouched().await;
        let mut r = request(&model(&server.base), "x");
        r.end = Some(image());
        let error = start(r).await.unwrap_err();
        assert_eq!(error.tag(), "UnsupportedOperation");
        assert_eq!(operation(&error), Some("video.end"));
        assert!(server.requests().is_empty());
    }

    // google-interactions-video.test.ts: "reads statuses from a JSON round-tripped token"
    #[tokio::test]
    async fn reads_statuses_from_a_json_round_tripped_token() {
        let server = mock(|r, n| {
            if r.method == "POST" || n == 0 {
                return Resp::json(200, started());
            }
            Resp::json(200, completed_body())
        })
        .await;
        let m = model(&server.base);
        let id = start(request(&m, "x")).await.unwrap();
        let decoded: String = serde_json::from_value(json!(id)).unwrap();
        let statuses = [status(&m, &decoded).await.unwrap(), status(&m, &decoded).await.unwrap()];
        assert_eq!(statuses, [running(), completed()]);
        assert_eq!(lines(&server), ["POST /v1beta/interactions", "GET /v1beta/interactions/v1_abc", "GET /v1beta/interactions/v1_abc"]);
    }

    // google-interactions-video.test.ts: "maps documented statuses and keeps unknown ones running"
    #[tokio::test]
    async fn maps_documented_statuses_and_keeps_unknown_ones_running() {
        let mut statuses = vec![];
        for s in ["in_progress", "completed", "incomplete", "failed", "cancelled", "requires_action", "constructor"] {
            let server = reply(json!({ "id": "v1_abc", "status": s })).await;
            statuses.push(status(&model(&server.base), ID).await.unwrap());
        }
        assert_eq!(statuses, [running(), completed(), completed(), errored(Reason::Failed), errored(Reason::Cancelled), running(), running()]);
    }

    // google-interactions-video.test.ts: "decodes the inline video with token usage and interaction metadata"
    #[tokio::test]
    async fn decodes_the_inline_video_with_token_usage_and_interaction_metadata() {
        let server = reply(completed_body()).await;
        let response = result(&model(&server.base), ID).await.unwrap();
        assert_eq!(response.videos.len(), 1);
        let video = &response.videos[0];
        assert_eq!(video.asset.source_type(), "bytes");
        assert_eq!(video.media_type(), "video/mp4");
        assert_eq!(video.asset.inline().unwrap().base64, "AAAAIGZ0eXBpc29t");
        let Some(VideoUsage::Tokens(usage)) = &response.usage else { panic!("token usage: {:?}", response.usage) };
        assert_eq!((usage.input, usage.output, usage.total), (Some(17.0), Some(5793.0), Some(6171.0)));
        assert_eq!(
            response.provider_metadata,
            Some(obj(json!({ "google": { "interactionId": "v1_abc", "model": "gemini-omni-1.2-flash", "status": "completed" } })))
        );
    }

    // google-interactions-video.test.ts: "keeps a Files API video URI as a Google reference"
    #[tokio::test]
    async fn keeps_a_files_api_video_uri_as_a_google_reference() {
        let uri = "https://generativelanguage.googleapis.com/v1beta/files/out:download?alt=media";
        let server = reply(json!({
            "id": "v1_abc",
            "status": "completed",
            "steps": [{ "type": "model_output", "content": [{ "type": "video", "mime_type": "video/mp4", "uri": uri }] }],
        }))
        .await;
        let response = result(&model(&server.base), ID).await.unwrap();
        assert_eq!(response.videos[0].asset.source, MediaSource::Ref { provider: "google".into(), id: uri.into(), media_type: Some("video/mp4".into()) });
    }

    // google-interactions-video.test.ts: "fails a result read before the interaction finishes"
    #[tokio::test]
    async fn fails_a_result_read_before_the_interaction_finishes() {
        let server = reply(started()).await;
        let error = result(&model(&server.base), ID).await.unwrap_err();
        assert_eq!(error.tag(), "InvalidRequest");
        assert_eq!(error.message, "Google Interactions Video generation v1_abc has not finished; await it before reading the result");
    }

    // google-interactions-video.test.ts: "fails the result of a ${terminal.body.status} interaction as ${terminal.tag}"
    #[tokio::test]
    async fn fails_the_result_of_a_terminal_interaction() {
        let cases = [
            (
                json!({ "id": "v1_abc", "status": "failed", "errors": [{ "code": "type.googleapis.com/INVALID_ARGUMENT", "message": "Bad duration." }] }),
                "InvalidRequest",
                "Google Interactions Video generation failed (INVALID_ARGUMENT): Bad duration.",
            ),
            (
                json!({ "id": "v1_abc", "status": "failed", "errors": [{ "code": "resource-exhausted", "message": "Slow down." }] }),
                "RateLimit",
                "Google Interactions Video generation failed (RESOURCE_EXHAUSTED): Slow down.",
            ),
            (
                json!({ "id": "v1_abc", "status": "failed", "errors": [{ "code": "SAFETY", "message": "Blocked." }] }),
                "ContentPolicy",
                "Google Interactions Video generation failed (SAFETY): Blocked.",
            ),
            (json!({ "id": "v1_abc", "status": "failed" }), "ProviderInternal", "Google Interactions Video generation failed"),
            (json!({ "id": "v1_abc", "status": "cancelled" }), "InvalidRequest", "Google Interactions Video interaction v1_abc was cancelled"),
            (
                json!({ "id": "v1_abc", "status": "completed", "steps": [], "errors": [{ "code": "PROHIBITED_CONTENT", "message": "No." }] }),
                "ContentPolicy",
                "Google Interactions Video withheld the video: No.",
            ),
            (json!({ "id": "v1_abc", "status": "incomplete", "steps": [] }), "InvalidProviderOutput", "Google Interactions Video incomplete without a video"),
        ];
        for (body, tag, message) in cases {
            let error = result_error(model, ID, &body).await;
            assert_eq!((error.tag(), error.message.as_str()), (tag, message));
        }
    }

    // google-interactions-video.test.ts: "cancels through the interaction cancel endpoint"
    #[tokio::test]
    async fn cancels_through_the_interaction_cancel_endpoint() {
        let server = reply(json!({ "id": "v1_abc", "status": "cancelled" })).await;
        video::cancel(&client(), &model(&server.base), ID).await.unwrap();
        assert_eq!(lines(&server), ["POST /v1beta/interactions/v1_abc/cancel"]);
        assert_eq!(server.requests()[0].header("x-goog-api-key"), Some("test"));
    }
}
