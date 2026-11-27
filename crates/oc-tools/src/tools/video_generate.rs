//! `generate_video` (`core/src/tool/plugin/video-generate.ts`, 2.4.1): generate a video as a background job. Core
//! registers it while a connected provider has a video model and `experimental.video_generate` isn't `false`. The
//! submission runs in the foreground, so a rejected request fails the call; the server then polls the job and stores
//! its videos as blobs, and the job waits for them and copies them into the workspace (`ToolHost::video_track`).

use async_trait::async_trait;
use base64::Engine;
use oc_schema::Metadata;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::file_access::{self, Kind};
use crate::host::{ModelSummary, VideoTrack};
use crate::tool::{DefinitionContext, ErrorCause, Tool, ToolContext, ToolDefinition, ToolError, ToolOptions, ToolResult, decode, meta};

pub const NAME: &str = "generate_video";
pub const MAX_INPUT_BYTES: u64 = 100 * 1024 * 1024;

pub const DESCRIPTION: &str = "Generates a video from a prompt, optionally starting or ending on given images and guided by reference images, videos or audio.\nOnly call this when the user asks for a video: videos are slow and expensive.\nIt runs in the background, and you are notified with every saved file when it finishes. Never poll, sleep, or check for the file while you wait.";

const SCHEMA: &str = include_str!("../../schemas/generate_video.json");
/// A failure that is not a `ToolFailure` of the tool's own (`Unable to generate video`).
const LEAF: &str = "Unable to generate video";

/// The completion notice for a background video job, tagged with its job ID like `<background-result kind="image" id=…>`
/// notices.
pub fn notice(id: &str, state: &str, body: &str) -> String {
    crate::background_result::render(&format!("kind=\"video\" id=\"{id}\" state=\"{state}\""), body)
}

/// Where the job's `index`th video is saved: the path itself for the first, then `cat-1.mp4`, `cat-2.mp4`, …
pub fn video_path(file: &str, index: usize) -> String {
    if index == 0 {
        return file.to_string();
    }
    let extension = extname(file);
    format!("{}-{index}{extension}", &file[..file.len() - extension.len()])
}

/// `path.extname`: the last `.` suffix of the base name, unless the name starts with it.
fn extname(file: &str) -> &str {
    let base_start = file.rfind(['/', std::path::MAIN_SEPARATOR]).map(|i| i + 1).unwrap_or(0);
    let base = &file[base_start..];
    match base.rfind('.') {
        Some(i) if i > 0 && base[..i].chars().any(|c| c != '.') => &base[i..],
        _ => "",
    }
}

/// `urlType`: the type a URL's extension names, for the video and audio formats providers take.
fn url_type(url: &str) -> Option<&'static str> {
    static EXT: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"(?i)\.([a-z0-9]+)(?:[?#]|$)").unwrap());
    let ext = EXT.captures(url)?[1].to_lowercase();
    Some(match ext.as_str() {
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "m4a" => "audio/mp4",
        "ogg" => "audio/ogg",
        "flac" => "audio/flac",
        _ => return None,
    })
}

/// `Media.Kind` of a media type.
fn kind(mime: Option<&str>) -> &'static str {
    let lower = mime.unwrap_or("application/octet-stream").to_lowercase();
    if lower.starts_with("image/") {
        "image"
    } else if lower.starts_with("video/") {
        "video"
    } else if lower.starts_with("audio/") {
        "audio"
    } else if lower == "application/pdf" || lower.starts_with("text/") {
        "document"
    } else {
        "other"
    }
}

/// `VideoAspectRatio`: `width:height` (`Media.AspectRatio`), or `auto`.
fn valid_aspect(value: &str) -> bool {
    if value == "auto" {
        return true;
    }
    static RATIO: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"^\d+(?:\.\d+)?:\d+(?:\.\d+)?$").unwrap());
    RATIO.is_match(value)
}

/// The preferred model from `experimental.video_generate.model`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredModel {
    pub provider_id: String,
    pub model: String,
}

#[derive(Deserialize)]
struct Input {
    prompt: String,
    start: Option<String>,
    end: Option<String>,
    references: Option<Vec<String>>,
    duration: Option<u64>,
    aspect: Option<String>,
    resolution: Option<String>,
    sound: Option<bool>,
    model: Option<String>,
    path: Option<String>,
}

/// `Model.Ref.parse`: `providerID/modelID[#variant]`.
fn parse_ref(input: &str) -> Option<(String, String)> {
    let provider_end = input.find('/').filter(|i| *i > 0)?;
    let provider = &input[..provider_end];
    let rest = &input[provider_end + 1..];
    let (id, variant) = match rest.find('#') {
        Some(i) => (&rest[..i], Some(&rest[i + 1..])),
        None => (rest, None),
    };
    if id.is_empty() || provider.contains('#') || variant.is_some_and(|v| v.is_empty() || v.contains('#')) {
        return None;
    }
    Some((provider.to_string(), id.to_string()))
}

pub struct VideoGenerateTool;

impl VideoGenerateTool {
    async fn resolve_model(&self, ctx: &ToolContext, input: Option<&str>) -> Result<ModelSummary, ToolError> {
        let available = ctx.host.models_list(&["video".to_string()]).await.map_err(|m| ToolError::caused_by(LEAF, m))?;
        let Some(input) = input else {
            // A configured default that isn't available falls back to the automatic pick.
            let configured = ctx.config.video_model.as_ref().and_then(|c| available.iter().find(|m| m.provider_id == c.provider_id && m.id == c.model));
            if let Some(model) = configured {
                return Ok(model.clone());
            }
            let session = ctx.host.session_get(&ctx.session_id).await.map_err(|m| ToolError::caused_by(LEAF, m))?;
            let provider_id = session.model.map(|m| m.provider_id);
            let model = available.iter().find(|m| Some(&m.provider_id) == provider_id.as_ref()).or(available.first());
            return model.cloned().ok_or_else(|| ToolError::new("No video model is available."));
        };
        let Some((provider_id, id)) = parse_ref(input) else {
            return Err(ToolError::new(format!("Invalid model \"{input}\". Use \"providerID/modelID\".")));
        };
        available.iter().find(|m| m.provider_id == provider_id && m.id == id).cloned().ok_or_else(|| {
            ToolError::new(format!(
                "Model \"{provider_id}/{id}\" is not an available video model. Use the models tool with type: \"video\" to see what is available."
            ))
        })
    }

    /// `readMedia`: URLs go to the provider as they are, which most video APIs prefer; local files are read and sent
    /// inline as `{type: "bytes", data, mediaType}`.
    async fn read_media(&self, ctx: &ToolContext, value: &str, kinds: &[&str]) -> Result<Value, ToolError> {
        let lower = value.to_ascii_lowercase();
        let (asset, mime) = if lower.starts_with("http://") || lower.starts_with("https://") {
            // The provider fetches the URL, but the agent chose it, so the webfetch rule still applies.
            let mut metadata = Metadata::new();
            metadata.insert("url".into(), json!(value));
            ctx.host
                .ask_permission(ctx.permission("webfetch", vec![value.to_string()], Some(vec!["*".into()]), Some(metadata)))
                .await
                .map_err(|e| e.into_tool_error(LEAF))?;
            let mime = url_type(value);
            let mut asset = json!({ "type": "url", "url": value });
            if let Some(m) = mime {
                asset["mediaType"] = json!(m);
            }
            (asset, mime)
        } else {
            let target = file_access::authorize_read(ctx, value, None).await.map_err(|e| e.into_tool_error(LEAF))?;
            let Ok(info) = tokio::fs::metadata(&target.absolute).await else {
                return Err(ToolError::new(format!("File not found: {value}")));
            };
            if info.len() > MAX_INPUT_BYTES {
                return Err(ToolError::new(format!("{value} is larger than 100 MiB.")));
            }
            let bytes = tokio::fs::read(&target.absolute).await.map_err(|e| ToolError::caused_by(LEAF, e.to_string()))?;
            let Some(mime) = crate::mime::detect_media_type(&bytes) else {
                return Err(ToolError::new(format!("{value} is not a supported image, video or audio file.")));
            };
            (json!({ "type": "bytes", "data": base64::engine::general_purpose::STANDARD.encode(&bytes), "mediaType": mime }), Some(mime))
        };
        // A URL without a known extension has no type yet; the provider checks it.
        let found = kind(mime);
        if found != "other" && !kinds.contains(&found) {
            let list = match kinds {
                [one] => one.to_string(),
                [rest @ .., last] => format!("{} or {last}", rest.join(", ")),
                [] => String::new(),
            };
            return Err(ToolError::new(format!("{value} must be {list}.")));
        }
        Ok(asset)
    }
}

#[async_trait]
impl Tool for VideoGenerateTool {
    fn name(&self) -> &str {
        NAME
    }

    fn options(&self) -> ToolOptions {
        ToolOptions::direct()
    }

    fn definition(&self, _ctx: &DefinitionContext) -> ToolDefinition {
        super::definition(NAME, DESCRIPTION, SCHEMA)
    }

    async fn execute(&self, raw: Value, ctx: ToolContext) -> Result<ToolResult, ToolError> {
        let input: Input = decode(raw.clone())?;
        let model = self.resolve_model(&ctx, input.model.as_deref()).await?;
        let reference = format!("{}/{}", model.provider_id, model.id);
        if let Some(aspect) = &input.aspect
            && !valid_aspect(aspect)
        {
            return Err(ToolError::new(format!("Invalid aspect \"{aspect}\". Use \"width:height\", such as \"16:9\", or \"auto\".")));
        }
        let target = input.path.as_deref().map(|p| file_access::resolve(&ctx, p, Some(Kind::File)));

        let mut metadata = raw.as_object().cloned().unwrap_or_default();
        metadata.insert("model".into(), json!(reference));
        ctx.host
            .ask_permission(ctx.permission(NAME, vec![reference.clone()], Some(vec!["*".into()]), Some(metadata)))
            .await
            .map_err(|e| e.into_tool_error(LEAF))?;
        if let Some(target) = &target {
            file_access::authorize_external(&ctx, &[target], None).await.map_err(|e| e.into_tool_error(LEAF))?;
            ctx.host
                .ask_permission(ctx.permission("edit", vec![target.resource.clone()], Some(vec!["*".into()]), None))
                .await
                .map_err(|e| e.into_tool_error(LEAF))?;
        }
        let start = match &input.start {
            Some(v) => Some(self.read_media(&ctx, v, &["image"]).await?),
            None => None,
        };
        let end = match &input.end {
            Some(v) => Some(self.read_media(&ctx, v, &["image"]).await?),
            None => None,
        };
        let mut references = vec![];
        for reference in input.references.iter().flatten() {
            references.push(self.read_media(&ctx, reference, &["image", "video", "audio"]).await?);
        }

        // Submitting runs in the foreground so a rejected request fails the call; the wait runs as a job.
        let mut request = serde_json::Map::new();
        request.insert("model".into(), json!({ "providerID": model.provider_id, "id": model.id }));
        request.insert("prompt".into(), json!(input.prompt));
        if let Some(v) = start {
            request.insert("start".into(), v);
        }
        if let Some(v) = end {
            request.insert("end".into(), v);
        }
        if input.references.is_some() {
            request.insert("references".into(), Value::Array(references));
        }
        if let Some(d) = input.duration {
            request.insert("duration".into(), json!(d));
        }
        if let Some(a) = &input.aspect {
            request.insert("aspect".into(), json!(a));
        }
        if let Some(r) = &input.resolution {
            request.insert("resolution".into(), json!(r));
        }
        if let Some(s) = input.sound {
            request.insert("sound".into(), json!(s));
        }
        let id = ctx
            .host
            .video_start(&ctx.session_id, Value::Object(request))
            .await
            .map_err(|m| ToolError::with_cause(LEAF, ErrorCause::Error(m)))?;
        let file = match &target {
            Some(t) => t.absolute.to_string_lossy().into_owned(),
            None => ctx.global.tmp.join("videos").join(&ctx.session_id).join(format!("{id}.mp4")).to_string_lossy().into_owned(),
        };
        ctx.host
            .video_track(VideoTrack {
                session_id: ctx.session_id.clone(),
                video_id: id.clone(),
                path: file.clone(),
                title: Some(input.prompt.clone()),
                notification_id: None,
            })
            .await
            .map_err(|m| ToolError::with_cause(LEAF, ErrorCause::Error(m)))?;
        let content = [
            // The model is named only when the tool picked it.
            format!(
                "Video job {id} started in the background{}. It will be saved to {file}; if the model returns more than one video, the others are saved next to it with -1, -2, … added to the name.",
                if input.model.is_some() { String::new() } else { format!(" with {reference}") }
            ),
            "You will be notified automatically when it finishes or fails. Videos usually take a few minutes.".to_string(),
            "DO NOT sleep, poll, check whether the file exists, or request the same video again while it runs.".to_string(),
            "Work on other tasks, or briefly tell the user what you started and end your response.".to_string(),
        ]
        .join("\n");
        Ok(ToolResult::text(content)
            .with_output(json!({ "id": id, "path": file, "model": reference }))
            .with_metadata(meta(vec![("path", json!(file)), ("model", json!(reference)), ("jobID", json!(id))])))
    }
}

/// The text a finished job keeps (`track`'s run): every saved file.
pub fn finished_text(files: &[String]) -> String {
    if files.len() == 1 {
        format!("Video request finished. Saved video: {}.", files[0])
    } else {
        format!("Video request finished. Saved {} videos: {}.", files.len(), files.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_paths_count_up_before_the_extension() {
        assert_eq!(video_path("/w/cat.mp4", 0), "/w/cat.mp4");
        assert_eq!(video_path("/w/cat.mp4", 1), "/w/cat-1.mp4");
        assert_eq!(video_path("/w/cat.mp4", 2), "/w/cat-2.mp4");
        assert_eq!(video_path("/w/cat", 1), "/w/cat-1");
        assert_eq!(video_path("/w/.env", 1), "/w/.env-1");
        assert_eq!(video_path("/w.d/clip", 1), "/w.d/clip-1");
    }

    #[test]
    fn url_types_follow_the_extension() {
        assert_eq!(url_type("https://example.test/voice.mp3?sig=1"), Some("audio/mpeg"));
        assert_eq!(url_type("https://example.test/motion.MP4"), Some("video/mp4"));
        assert_eq!(url_type("https://example.test/last.png"), None);
        assert_eq!(url_type("https://example.test/clip"), None);
    }

    #[test]
    fn aspects() {
        assert!(valid_aspect("16:9"));
        assert!(valid_aspect("auto"));
        assert!(!valid_aspect("wide"));
        assert!(valid_aspect("2.39:1"));
        assert!(!valid_aspect("16/9"));
    }

    // ---- the tool against a fake host (`test/tool-video-generate.test.ts`) ----

    use crate::host::{PermissionError, PermissionRequest, SessionSummary, ToolHost};
    use crate::tool::ToolResult;
    use parking_lot::Mutex;
    use std::sync::Arc;

    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";
    const MP4: [u8; 16] = [0, 0, 0, 0x18, 0x66, 0x74, 0x79, 0x70, 0x69, 0x73, 0x6f, 0x6d, 0, 0, 0, 0];

    #[derive(Default)]
    struct Fake {
        models: Mutex<Vec<ModelSummary>>,
        session_provider: Mutex<Option<String>>,
        asked: Mutex<Vec<PermissionRequest>>,
        starts: Mutex<Vec<Value>>,
        tracks: Mutex<Vec<VideoTrack>>,
        reject: Mutex<Option<String>>,
    }

    #[async_trait]
    impl ToolHost for Fake {
        async fn ask_permission(&self, request: PermissionRequest) -> Result<(), PermissionError> {
            self.asked.lock().push(request);
            Ok(())
        }
        async fn models_list(&self, types: &[String]) -> Result<Vec<ModelSummary>, String> {
            assert_eq!(types, ["video".to_string()]);
            Ok(self.models.lock().clone())
        }
        async fn session_get(&self, session_id: &str) -> Result<SessionSummary, String> {
            let model = self.session_provider.lock().clone().map(|p| oc_schema::ModelRef { id: "chat".into(), provider_id: p, variant: None });
            Ok(SessionSummary { id: session_id.into(), parent_id: None, agent: None, model, directory: String::new() })
        }
        async fn video_start(&self, session_id: &str, request: Value) -> Result<String, String> {
            assert_eq!(session_id, "ses_test");
            if let Some(message) = self.reject.lock().clone() {
                return Err(message);
            }
            self.starts.lock().push(request);
            Ok("video_0123456789abABCDEFGHIJKLMN".into())
        }
        async fn video_track(&self, track: VideoTrack) -> Result<(), String> {
            self.tracks.lock().push(track);
            Ok(())
        }
    }

    fn model(provider: &str, id: &str) -> ModelSummary {
        ModelSummary {
            provider_id: provider.into(),
            id: id.into(),
            name: id.into(),
            family: None,
            released: 0.0,
            variants: vec![],
            cost: json!([]),
            status: "active".into(),
            input: vec!["text".into()],
        }
    }

    fn setup(fake: Arc<Fake>, configured: Option<ConfiguredModel>) -> (tempfile::TempDir, ToolContext) {
        let dir = tempfile::tempdir().unwrap();
        let mut global = oc_util::Global::resolve();
        global.tmp = dir.path().join(".tmp");
        global.home = dir.path().join(".home");
        let ctx = ToolContext {
            codemode: false,
            session_id: "ses_test".into(),
            message_id: "msg_test".into(),
            call_id: "call_1".into(),
            agent: "build".into(),
            directory: dir.path().to_path_buf(),
            worktree: dir.path().to_path_buf(),
            project_id: "prj".into(),
            workspace_id: None,
            global: Arc::new(global),
            cancel: tokio_util::sync::CancellationToken::new(),
            stop: Default::default(),
            progress: Arc::new(|_| {}),
            config: Arc::new(crate::tool::ToolConfig { video_model: configured, ..Default::default() }),
            host: fake,
        };
        (dir, ctx)
    }

    async fn run(ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        crate::registry::execute_tool(&VideoGenerateTool, NAME, input, ctx.clone(), &crate::registry::NoHooks).await
    }

    // tool-video-generate.test.ts: "starts the job, saves every video it returns and lists them all in the notice" (the
    // tool's half; the wait, copy and notice are oc-core's `track_video`).
    #[tokio::test]
    async fn starts_the_job_with_its_media_and_tracks_it() {
        let fake = Arc::new(Fake::default());
        *fake.models.lock() = vec![model("test", "director")];
        *fake.session_provider.lock() = Some("test".into());
        let (dir, ctx) = setup(fake.clone(), None);
        std::fs::write(dir.path().join("first.png"), base64::engine::general_purpose::STANDARD.decode(PNG).unwrap()).unwrap();
        let input = json!({
            "prompt": "the kite takes off",
            "start": "first.png",
            "end": "https://example.test/last.png",
            "references": ["https://example.test/voice.mp3?sig=1", "https://example.test/motion.mp4"],
            "duration": 6,
            "aspect": "16:9",
            "resolution": "720p",
            "sound": true,
            "path": "kite.mp4",
        });
        let result = run(&ctx, input).await.unwrap();
        let id = "video_0123456789abABCDEFGHIJKLMN";
        let file = dir.path().join("kite.mp4").display().to_string();
        assert_eq!(result.output, Some(json!({ "id": id, "path": file, "model": "test/director" })));
        assert_eq!(
            result.text_content(),
            [
                format!("Video job {id} started in the background with test/director. It will be saved to {file}; if the model returns more than one video, the others are saved next to it with -1, -2, … added to the name."),
                "You will be notified automatically when it finishes or fails. Videos usually take a few minutes.".into(),
                "DO NOT sleep, poll, check whether the file exists, or request the same video again while it runs.".into(),
                "Work on other tasks, or briefly tell the user what you started and end your response.".into(),
            ]
            .join("\n")
        );
        assert_eq!(
            fake.starts.lock()[0],
            json!({
                "model": { "providerID": "test", "id": "director" },
                "prompt": "the kite takes off",
                "start": { "type": "bytes", "data": PNG, "mediaType": "image/png" },
                "end": { "type": "url", "url": "https://example.test/last.png" },
                "references": [
                    { "type": "url", "url": "https://example.test/voice.mp3?sig=1", "mediaType": "audio/mpeg" },
                    { "type": "url", "url": "https://example.test/motion.mp4", "mediaType": "video/mp4" },
                ],
                "duration": 6,
                "aspect": "16:9",
                "resolution": "720p",
                "sound": true,
            })
        );
        assert_eq!(
            fake.tracks.lock()[0],
            VideoTrack { session_id: "ses_test".into(), video_id: id.into(), path: file, title: Some("the kite takes off".into()), notification_id: None }
        );
        let asked: Vec<(String, Vec<String>)> = fake.asked.lock().iter().map(|r| (r.action.clone(), r.resources.clone())).collect();
        assert_eq!(
            asked,
            [
                ("generate_video".to_string(), vec!["test/director".to_string()]),
                ("edit".into(), vec!["kite.mp4".into()]),
                ("read".into(), vec!["first.png".into()]),
                ("webfetch".into(), vec!["https://example.test/last.png".into()]),
                ("webfetch".into(), vec!["https://example.test/voice.mp3?sig=1".into()]),
                ("webfetch".into(), vec!["https://example.test/motion.mp4".into()]),
            ]
        );
    }

    #[tokio::test]
    async fn defaults_to_the_session_temp_folder_and_names_only_a_picked_model() {
        let fake = Arc::new(Fake::default());
        *fake.models.lock() = vec![model("other", "reel"), model("test", "director")];
        let (_dir, ctx) = setup(fake.clone(), Some(ConfiguredModel { provider_id: "test".into(), model: "director".into() }));
        let result = run(&ctx, json!({ "prompt": "waves" })).await.unwrap();
        let file = ctx.global.tmp.join("videos").join("ses_test").join("video_0123456789abABCDEFGHIJKLMN.mp4").display().to_string();
        assert_eq!(result.output.as_ref().unwrap()["path"], json!(file));
        assert_eq!(result.output.as_ref().unwrap()["model"], json!("test/director"));
        let explicit = run(&ctx, json!({ "prompt": "waves", "model": "other/reel" })).await.unwrap();
        assert!(explicit.text_content().starts_with("Video job video_0123456789abABCDEFGHIJKLMN started in the background. It will"));
    }

    #[tokio::test]
    async fn rejects_bad_input_before_starting() {
        let fake = Arc::new(Fake::default());
        let (dir, ctx) = setup(fake.clone(), None);
        assert_eq!(run(&ctx, json!({ "prompt": "x" })).await.unwrap_err().message, "No video model is available.");
        *fake.models.lock() = vec![model("test", "director")];
        assert_eq!(run(&ctx, json!({ "prompt": "x", "model": "director" })).await.unwrap_err().message, "Invalid model \"director\". Use \"providerID/modelID\".");
        assert_eq!(
            run(&ctx, json!({ "prompt": "x", "model": "test/nope" })).await.unwrap_err().message,
            "Model \"test/nope\" is not an available video model. Use the models tool with type: \"video\" to see what is available."
        );
        assert_eq!(
            run(&ctx, json!({ "prompt": "x", "aspect": "wide" })).await.unwrap_err().message,
            "Invalid aspect \"wide\". Use \"width:height\", such as \"16:9\", or \"auto\"."
        );
        std::fs::write(dir.path().join("clip.mp4"), MP4).unwrap();
        assert_eq!(run(&ctx, json!({ "prompt": "x", "start": "clip.mp4" })).await.unwrap_err().message, "clip.mp4 must be image.");
        std::fs::write(dir.path().join("notes.txt"), "hello").unwrap();
        assert_eq!(
            run(&ctx, json!({ "prompt": "x", "references": ["notes.txt"] })).await.unwrap_err().message,
            "notes.txt is not a supported image, video or audio file."
        );
        assert_eq!(run(&ctx, json!({ "prompt": "x", "end": "missing.png" })).await.unwrap_err().message, "File not found: missing.png");
        assert!(fake.starts.lock().is_empty());
        *fake.reject.lock() = Some("Model not found: test/director".into());
        let rejected = run(&ctx, json!({ "prompt": "x" })).await.unwrap_err();
        assert_eq!(rejected.message, LEAF);
        assert!(fake.tracks.lock().is_empty());
    }

    #[test]
    fn notices() {
        assert_eq!(
            notice("video_1", "completed", "done"),
            "<background-result kind=\"video\" id=\"video_1\" state=\"completed\">\ndone\n</background-result>"
        );
        assert_eq!(finished_text(&["/a.mp4".into()]), "Video request finished. Saved video: /a.mp4.");
        assert_eq!(finished_text(&["/a.mp4".into(), "/a-1.mp4".into()]), "Video request finished. Saved 2 videos: /a.mp4, /a-1.mp4.");
    }
}
