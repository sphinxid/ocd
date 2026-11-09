//! `experimental_image_generate` (`core/src/tool/plugin/image-generate.ts`): generate or edit one image as a
//! background job. Core registers it only while an image model is available and `experimental.image_generate`
//! isn't `false`; the generation itself goes through the host (`Generate.image`).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use futures::StreamExt;
use oc_schema::Metadata;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::file_access::{self, Kind, Target};
use crate::host::{JobRecovery, JobStart, JobStatus, ModelSummary, PermissionError, SyntheticMessage, ToolHost};
use crate::tool::{DefinitionContext, ErrorCause, Tool, ToolContext, ToolDefinition, ToolError, ToolOptions, ToolResult, decode, meta};

pub const NAME: &str = "experimental_image_generate";
pub const MAX_INPUT_BYTES: usize = 20 * 1024 * 1024;
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30);
const GENERATE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const INPUT_TYPES: &[&str] = &["image/png", "image/jpeg", "image/webp", "image/gif"];

pub const DESCRIPTION: &str = "Generates a new image, or edits existing ones.\nOnly call this when the user asks you to create or edit an image. If you think an image would help but the user didn't ask for one, ask them first instead of generating it.\nImages you pass are used as the source to edit, or as references for style and subject.\nEach call makes one image. For several images, call it once per image.\nIt runs in the background: the call starts a job and returns right away, and you are notified automatically when the image is saved, with its path. Never poll, sleep, or check for the file while you wait.\nDon't repeat a request unless the user asks.";

const SCHEMA: &str = include_str!("../../schemas/experimental_image_generate.json");
/// A failure that is not a `ToolFailure` of the tool's own (`Unable to generate image`).
const LEAF: &str = "Unable to generate image";

fn extension(mime: &str) -> Option<&'static str> {
    match mime {
        "image/png" => Some(".png"),
        "image/jpeg" => Some(".jpg"),
        "image/webp" => Some(".webp"),
        "image/gif" => Some(".gif"),
        _ => None,
    }
}

/// `ImageNotice.text`: the completion notice of a background image job, tagged with its job ID like `<shell id=…>`
/// notices.
pub fn notice(id: &str, state: &str, body: &str) -> String {
    format!("<image id=\"{id}\" state=\"{state}\">\n{body}\n</image>")
}

/// The preferred model from `experimental.image_generate.model`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredModel {
    pub provider_id: String,
    pub model: String,
    pub variant: Option<String>,
}

#[derive(Deserialize)]
struct Input {
    prompt: String,
    images: Option<Vec<String>>,
    model: Option<String>,
    path: Option<String>,
}

struct Resolved {
    model: ModelSummary,
    variant: Option<String>,
    note: Option<String>,
}

fn accepts_images(model: &ModelSummary) -> bool {
    model.input.iter().any(|i| i.starts_with("image"))
}

/// The newest image model from the session's provider, otherwise the newest available one.
pub fn default_model<'a>(available: &'a [ModelSummary], provider_id: Option<&str>, images: bool) -> Option<&'a ModelSummary> {
    let candidates: Vec<&ModelSummary> = available.iter().filter(|m| !images || accepts_images(m)).collect();
    candidates.iter().find(|m| Some(m.provider_id.as_str()) == provider_id).or(candidates.first()).copied()
}

/// `Model.Ref.parse`: `providerID/modelID[#variant]`.
fn parse_ref(input: &str) -> Option<(String, String, Option<String>)> {
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
    Some((provider.to_string(), id.to_string(), variant.map(String::from)))
}

fn reference(model: &ModelSummary, variant: Option<&str>) -> String {
    format!("{}/{}{}", model.provider_id, model.id, variant.map(|v| format!("#{v}")).unwrap_or_default())
}

pub struct ImageGenerateTool {
    http: reqwest::Client,
}

impl ImageGenerateTool {
    pub fn new(http: reqwest::Client) -> Self {
        Self { http }
    }

    async fn resolve_model(&self, ctx: &ToolContext, input: Option<&str>, images: bool) -> Result<Resolved, ToolError> {
        let available = ctx.host.models_list(&["image".to_string()]).await.map_err(|m| ToolError::caused_by(LEAF, m))?;
        let Some(input) = input else {
            // A configured default is a preference: when it can't serve this call, pick automatically and say so.
            let configured = ctx.config.image_model.clone();
            let preferred = configured.as_ref().and_then(|c| available.iter().find(|m| m.provider_id == c.provider_id && m.id == c.model));
            let variant_missing = match (&configured, preferred) {
                (Some(c), Some(p)) => c.variant.as_ref().is_some_and(|v| !p.variants.contains(v)),
                _ => false,
            };
            if let Some(p) = preferred
                && !variant_missing
                && (!images || accepts_images(p))
            {
                return Ok(Resolved { model: p.clone(), variant: configured.and_then(|c| c.variant), note: None });
            }
            let session = ctx.host.session_get(&ctx.session_id).await.map_err(|m| ToolError::caused_by(LEAF, m))?;
            let provider_id = session.model.map(|m| m.provider_id);
            let Some(model) = default_model(&available, provider_id.as_deref(), images) else {
                return Err(ToolError::new(if images {
                    "None of the available image models accept input images."
                } else {
                    "No image model is available."
                }));
            };
            let Some(configured) = configured else { return Ok(Resolved { model: model.clone(), variant: None, note: None }) };
            let wanted = format!(
                "{}/{}{}",
                configured.provider_id,
                configured.model,
                configured.variant.as_ref().map(|v| format!("#{v}")).unwrap_or_default()
            );
            let reason = if preferred.is_none() {
                "isn't available".to_string()
            } else if variant_missing {
                format!("has no variant \"{}\"", configured.variant.unwrap_or_default())
            } else {
                "doesn't accept input images".to_string()
            };
            let note = format!(" {wanted} {reason}; used {}/{}.", model.provider_id, model.id);
            return Ok(Resolved { model: model.clone(), variant: None, note: Some(note) });
        };
        let Some((provider_id, id, variant)) = parse_ref(input) else {
            return Err(ToolError::new(format!("Invalid model \"{input}\". Use \"providerID/modelID\" or \"providerID/modelID#variant\".")));
        };
        let Some(model) = available.iter().find(|m| m.provider_id == provider_id && m.id == id) else {
            return Err(ToolError::new(format!(
                "Model \"{provider_id}/{id}\" is not an available image model. Use the models tool with type: \"image\" to see what is available."
            )));
        };
        if let Some(v) = &variant
            && !model.variants.contains(v)
        {
            return Err(ToolError::new(if model.variants.is_empty() {
                format!("Model \"{provider_id}/{id}\" has no variants. Omit the variant.")
            } else {
                format!("Variant \"{v}\" is not available for \"{provider_id}/{id}\". Available: {}.", model.variants.join(", "))
            }));
        }
        if images && !accepts_images(model) {
            let capable: Vec<String> =
                available.iter().filter(|m| accepts_images(m)).take(3).map(|m| format!("{}/{}", m.provider_id, m.id)).collect();
            return Err(ToolError::new(if capable.is_empty() {
                format!("Model \"{provider_id}/{id}\" does not accept input images, and no available image model does.")
            } else {
                format!("Model \"{provider_id}/{id}\" does not accept input images. Image models that do: {}.", capable.join(", "))
            }));
        }
        Ok(Resolved { model: model.clone(), variant, note: None })
    }

    /// `readImage`: a local file or an http(s) URL as `{data, mime, name}`.
    async fn read_image(&self, ctx: &ToolContext, source: &str) -> Result<Value, ToolError> {
        let bytes = self.read_bytes(ctx, source).await?;
        let mime = crate::mime::detect(&bytes);
        if !INPUT_TYPES.contains(&mime) {
            return Err(ToolError::new(format!("{source} is not a PNG, JPEG, WebP or GIF image.")));
        }
        let name = Path::new(source).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        Ok(json!({ "data": base64::engine::general_purpose::STANDARD.encode(&bytes), "mime": mime, "name": name }))
    }

    async fn read_bytes(&self, ctx: &ToolContext, source: &str) -> Result<Vec<u8>, ToolError> {
        let lower = source.to_ascii_lowercase();
        if lower.starts_with("http://") || lower.starts_with("https://") {
            // Downloads follow the same rule as the webfetch tool, so denying web access also covers image URLs.
            let mut metadata = Metadata::new();
            metadata.insert("url".into(), json!(source));
            ctx.host
                .ask_permission(ctx.permission("webfetch", vec![source.to_string()], Some(vec!["*".into()]), Some(metadata)))
                .await
                .map_err(|e| e.into_tool_error(LEAF))?;
            let too_large = || ToolError::new(format!("{source} is larger than 20 MiB."));
            let download = async {
                let response = self.http.get(source).send().await.map_err(|e| e.to_string())?;
                let status = response.status();
                if !status.is_success() {
                    return Err(format!("StatusCode: non 2xx status code ({} GET {source})", status.as_u16()));
                }
                if response.content_length().is_some_and(|l| l as usize > MAX_INPUT_BYTES) {
                    return Ok(Err(too_large()));
                }
                let mut body = Vec::new();
                let mut stream = response.bytes_stream();
                while let Some(chunk) = stream.next().await {
                    let chunk = chunk.map_err(|e| e.to_string())?;
                    if body.len() + chunk.len() > MAX_INPUT_BYTES {
                        return Ok(Err(too_large()));
                    }
                    body.extend_from_slice(&chunk);
                }
                Ok(Ok(body))
            };
            return match tokio::time::timeout(DOWNLOAD_TIMEOUT, download).await {
                Err(_) => Err(ToolError::new(format!("Timed out downloading {source}."))),
                Ok(Err(cause)) => Err(ToolError::caused_by(format!("Unable to download {source}"), cause)),
                Ok(Ok(result)) => result,
            };
        }
        let target = file_access::authorize_read(ctx, source, None).await.map_err(|e| e.into_tool_error(LEAF))?;
        let Ok(info) = tokio::fs::metadata(&target.absolute).await else {
            return Err(ToolError::new(format!("File not found: {source}")));
        };
        if info.len() as usize > MAX_INPUT_BYTES {
            return Err(ToolError::new(format!("{source} is larger than 20 MiB.")));
        }
        tokio::fs::read(&target.absolute).await.map_err(|e| ToolError::caused_by(LEAF, e.to_string()))
    }
}

fn permission_message(error: PermissionError) -> String {
    match error {
        PermissionError::Declined | PermissionError::LocationClosed => String::new(),
        PermissionError::Corrected { feedback } => feedback,
        PermissionError::Blocked { message } | PermissionError::Other { message } => message,
    }
}

/// The background generation: call the image model, then save the image with the extension of its real format.
#[allow(clippy::too_many_arguments)]
async fn run(
    host: Arc<dyn ToolHost>,
    ctx: ToolContext,
    request: Value,
    reference: String,
    stem: PathBuf,
    target: Option<Target>,
) -> Result<String, String> {
    let generated = tokio::time::timeout(GENERATE_TIMEOUT, host.generate_image(request))
        .await
        .map_err(|_| "Image generation timed out after 30 minutes.".to_string())??;
    let Some(image) = generated.get("images").and_then(Value::as_array).and_then(|a| a.first()) else {
        return Err(format!("{reference} returned no image."));
    };
    let bytes = base64::engine::general_purpose::STANDARD.decode(image["data"].as_str().unwrap_or_default()).unwrap_or_default();
    let file = PathBuf::from(format!("{}{}", stem.display(), extension(crate::mime::detect(&bytes)).unwrap_or(".png")));
    // The edit prompt covered the requested path. Replacing a different existing file needs its own approval; a new
    // file only has to pass the deny rules.
    if let Some(target) = &target
        && file != target.absolute
    {
        let actual = file_access::resolve(&ctx, &file.to_string_lossy(), Some(Kind::File));
        let replaces = tokio::fs::metadata(&file).await.is_ok();
        let request = ctx.permission("edit", vec![actual.resource], None, None);
        let result = if replaces {
            host.ask_permission(crate::host::PermissionRequest { save: Some(vec!["*".into()]), ..request }).await
        } else {
            host.check_permission(request).await
        };
        result.map_err(permission_message)?;
    }
    if let Some(parent) = file.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|e| e.to_string())?;
    }
    tokio::fs::write(&file, &bytes).await.map_err(|e| e.to_string())?;
    Ok(format!("Image request finished. Saved image: {} ({reference}).\nUse the read tool on this path to view the image.", file.display()))
}

/// `notifyWhenDone`: the job's outcome as an `<image>` notice in the session, then the background record settles.
fn notify_when_done(host: Arc<dyn ToolHost>, session_id: String, id: String, file: String) {
    tokio::spawn(async move {
        let Ok(Some(info)) = host.job_wait(&id).await else { return };
        if info.status == JobStatus::Running {
            return;
        }
        let body = match info.status {
            JobStatus::Completed => info.output.clone().unwrap_or_default(),
            JobStatus::Error => info.error.clone().unwrap_or_else(|| format!("Image request failed for {file}.")),
            _ => format!("Image request for {file} was cancelled."),
        };
        let state = info.status.as_str();
        let _ = host
            .session_synthetic(SyntheticMessage {
                id: info.notification_id.clone(),
                session_id,
                description: "Image generation".into(),
                text: notice(&id, state, &body),
                metadata: meta(vec![("source", json!(NAME)), ("jobID", json!(id)), ("state", json!(state)), ("path", json!(file))]),
            })
            .await;
        if let Some(n) = &info.notification_id {
            let _ = host.job_complete_background(n).await;
        }
    });
}

#[async_trait]
impl Tool for ImageGenerateTool {
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
        let input: Input = decode(raw)?;
        let resolved = self.resolve_model(&ctx, input.model.as_deref(), input.images.is_some()).await?;
        let model = &resolved.model;
        let variant = resolved.variant.clone();
        let reference = reference(model, variant.as_deref());
        let target = input.path.as_deref().map(|p| file_access::resolve(&ctx, p, Some(Kind::File)));
        // The extension the image is saved with follows its real format, whatever the requested path ends in.
        let stem = match &target {
            Some(t) => {
                let absolute = t.absolute.to_string_lossy().into_owned();
                let ext = t.absolute.extension().map(|e| e.len() + 1).unwrap_or(0);
                // `path.extname` ignores a leading dot (`.env` has no extension).
                let ext = if t.absolute.file_name().is_some_and(|n| n.len() == ext) { 0 } else { ext };
                PathBuf::from(&absolute[..absolute.len() - ext])
            }
            None => ctx
                .global
                .tmp
                .join("images")
                .join(&ctx.session_id)
                .join(format!("image_{}", oc_schema::id::create(false, None))),
        };

        let mut metadata = Metadata::new();
        metadata.insert("prompt".into(), json!(input.prompt));
        if let Some(images) = &input.images {
            metadata.insert("images".into(), json!(images));
        }
        if input.model.is_some() {
            metadata.insert("model".into(), json!(reference));
        }
        if let Some(path) = &input.path {
            metadata.insert("path".into(), json!(path));
        }
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
        let mut images = vec![];
        for source in input.images.iter().flatten() {
            images.push(self.read_image(&ctx, source).await?);
        }

        // Everything that needs the user or can fail fast happened above; the paid call runs as a background job.
        let mut model_ref = serde_json::Map::new();
        model_ref.insert("providerID".into(), json!(model.provider_id));
        model_ref.insert("id".into(), json!(model.id));
        if let Some(v) = &variant {
            model_ref.insert("variant".into(), json!(v));
        }
        let mut request = serde_json::Map::new();
        request.insert("model".into(), Value::Object(model_ref));
        request.insert("prompt".into(), json!(input.prompt));
        request.insert("n".into(), json!(1));
        if !images.is_empty() || input.images.is_some() {
            request.insert("images".into(), Value::Array(images));
        }
        let stem_text = stem.display().to_string();
        let host = ctx.host.clone();
        let failed = stem_text.clone();
        let job_run = run(host.clone(), ctx.clone(), Value::Object(request), reference.clone(), stem.clone(), target.clone());
        // The job keeps this text, so a restart can still deliver the outcome.
        let job_run = async move { job_run.await.map_err(|message| format!("Image request failed for {failed}: {message}")) };
        let id = format!("image_{}", oc_schema::id::create(false, None));
        let job = host
            .job_start(JobStart {
                id: id.clone(),
                kind: NAME.into(),
                title: Some(input.prompt.clone()),
                metadata: meta(vec![("sessionID", json!(ctx.session_id)), ("path", json!(stem_text)), ("model", json!(reference))]),
                recovery: Some(JobRecovery::Image { session_id: ctx.session_id.clone(), path: stem_text.clone(), model: reference.clone() }),
                run: Box::pin(job_run),
            })
            .await
            .map_err(|m| ToolError::with_cause(LEAF, ErrorCause::Error(m)))?;
        let _ = host.job_background(&job.id).await;
        notify_when_done(host, ctx.session_id.clone(), id.clone(), stem_text.clone());
        let content = [
            format!(
                "Image job {id} started in the background with {reference}. It will be saved as {stem_text} with the extension of its real format (.png, .jpg, .webp or .gif); the notice gives the exact path.{}",
                resolved.note.unwrap_or_default()
            ),
            "You will be notified automatically when it finishes or fails.".to_string(),
            "DO NOT sleep, poll, check whether the file exists, or request the same image again while it runs.".to_string(),
            "Work on other tasks, or briefly tell the user what you started and end your response.".to_string(),
        ]
        .join("\n");
        Ok(ToolResult::text(content)
            .with_output(json!({ "id": id, "path": stem_text, "model": reference }))
            .with_metadata(meta(vec![("path", json!(stem_text)), ("model", json!(reference)), ("jobID", json!(id))])))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(provider: &str, id: &str, input: &[&str]) -> ModelSummary {
        ModelSummary {
            provider_id: provider.into(),
            id: id.into(),
            name: id.into(),
            family: None,
            released: 0.0,
            variants: vec![],
            cost: json!([]),
            status: "active".into(),
            input: input.iter().map(|s| s.to_string()).collect(),
        }
    }

    // `test/tool/image-generate.test.ts` "defaultModel".
    #[test]
    fn default_model_prefers_the_session_provider() {
        let available = [model("openai", "gpt-image-2", &["text", "image"]), model("google", "imagen", &["text"])];
        assert_eq!(default_model(&available, Some("google"), false).unwrap().id, "imagen");
        assert_eq!(default_model(&available, Some("xai"), false).unwrap().id, "gpt-image-2");
        assert_eq!(default_model(&available, Some("google"), true).unwrap().id, "gpt-image-2");
        assert!(default_model(&[model("google", "imagen", &["text"])], None, true).is_none());
    }

    #[test]
    fn parses_model_refs() {
        assert_eq!(parse_ref("openai/gpt-image-2"), Some(("openai".into(), "gpt-image-2".into(), None)));
        assert_eq!(parse_ref("openrouter/google/gemini#high"), Some(("openrouter".into(), "google/gemini".into(), Some("high".into()))));
        assert_eq!(parse_ref("openai"), None);
        assert_eq!(parse_ref("/x"), None);
        assert_eq!(parse_ref("a/b#"), None);
    }

    #[test]
    fn notices() {
        assert_eq!(notice("image_1", "completed", "done"), "<image id=\"image_1\" state=\"completed\">\ndone\n</image>");
    }

    // ---- the tool against a fake host (`test/tool-image-generate.test.ts`) ----

    use crate::host::{JobInfo, PermissionRequest, SessionSummary};
    use parking_lot::Mutex;
    use std::collections::HashMap;

    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
    const JPEG: &str = "/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDAAgGBgcGBQgHBwcJCQgKDBQNDAsLDBkSEw8UHRofHh0aHBwgJC4nICIsIxwcKDcpLDAxNDQ0Hyc5PTgyPC4zNDL/2Q==";

    #[derive(Default)]
    struct Fake {
        models: Mutex<Vec<ModelSummary>>,
        session_provider: Mutex<Option<String>>,
        asked: Mutex<Vec<PermissionRequest>>,
        checked: Mutex<Vec<PermissionRequest>>,
        deny: Mutex<Option<String>>,
        requests: Mutex<Vec<Value>>,
        result: Mutex<Option<Result<Value, String>>>,
        jobs: Mutex<HashMap<String, tokio::sync::watch::Receiver<Option<JobInfo>>>>,
        notices: Mutex<Vec<SyntheticMessage>>,
        notified: tokio::sync::Notify,
    }

    #[async_trait]
    impl ToolHost for Fake {
        async fn ask_permission(&self, request: PermissionRequest) -> Result<(), PermissionError> {
            let denied = self.deny.lock().as_deref() == Some(request.action.as_str());
            self.asked.lock().push(request);
            if denied { Err(PermissionError::Blocked { message: "denied".into() }) } else { Ok(()) }
        }
        async fn check_permission(&self, request: PermissionRequest) -> Result<(), PermissionError> {
            self.checked.lock().push(request);
            Ok(())
        }
        async fn models_list(&self, _types: &[String]) -> Result<Vec<ModelSummary>, String> {
            Ok(self.models.lock().clone())
        }
        async fn session_get(&self, session_id: &str) -> Result<SessionSummary, String> {
            let model = self.session_provider.lock().clone().map(|p| oc_schema::ModelRef { id: "m".into(), provider_id: p, variant: None });
            Ok(SessionSummary { id: session_id.into(), parent_id: None, agent: None, model })
        }
        async fn generate_image(&self, request: Value) -> Result<Value, String> {
            self.requests.lock().push(request);
            self.result.lock().clone().unwrap_or_else(|| Ok(json!({ "images": [{ "type": "image", "data": PNG, "mime": "image/png" }] })))
        }
        async fn job_start(&self, start: JobStart) -> Result<JobInfo, String> {
            let info = JobInfo {
                id: start.id.clone(),
                kind: start.kind,
                title: start.title,
                status: JobStatus::Running,
                started_at: 0,
                completed_at: None,
                output: None,
                error: None,
                notification_id: Some(format!("msg_{}", start.id)),
            };
            let (tx, rx) = tokio::sync::watch::channel(None);
            self.jobs.lock().insert(start.id.clone(), rx);
            let mut done = info.clone();
            let run = start.run;
            tokio::spawn(async move {
                match run.await {
                    Ok(output) => (done.status, done.output) = (JobStatus::Completed, Some(output)),
                    Err(error) => (done.status, done.error) = (JobStatus::Error, Some(error)),
                }
                let _ = tx.send(Some(done));
            });
            Ok(info)
        }
        async fn job_wait(&self, id: &str) -> Result<Option<JobInfo>, String> {
            let Some(mut rx) = self.jobs.lock().get(id).cloned() else { return Ok(None) };
            loop {
                if let Some(info) = rx.borrow().clone() {
                    return Ok(Some(info));
                }
                if rx.changed().await.is_err() {
                    return Ok(None);
                }
            }
        }
        async fn session_synthetic(&self, message: SyntheticMessage) -> Result<(), String> {
            self.notices.lock().push(message);
            self.notified.notify_one();
            Ok(())
        }
    }

    fn image_model(provider: &str, id: &str, input: &[&str], variants: &[&str]) -> ModelSummary {
        ModelSummary { variants: variants.iter().map(|v| v.to_string()).collect(), ..model(provider, id, input) }
    }

    fn setup(fake: Arc<Fake>, configured: Option<ConfiguredModel>) -> (tempfile::TempDir, ToolContext) {
        let dir = tempfile::tempdir().unwrap();
        let mut global = oc_util::Global::resolve();
        global.tmp = dir.path().join(".tmp");
        global.home = dir.path().join(".home");
        let ctx = ToolContext {
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
            progress: Arc::new(|_| {}),
            config: Arc::new(crate::tool::ToolConfig { image_model: configured, ..Default::default() }),
            host: fake,
        };
        (dir, ctx)
    }

    async fn run(ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let tool = ImageGenerateTool::new(reqwest::Client::new());
        crate::registry::execute_tool(&tool, NAME, input, ctx.clone(), &crate::registry::NoHooks).await
    }

    async fn notice_of(fake: &Fake, result: &ToolResult) -> String {
        let id = result.output.as_ref().unwrap()["id"].as_str().unwrap().to_string();
        loop {
            if let Some(n) = fake.notices.lock().iter().find(|n| n.metadata["jobID"] == json!(id)) {
                return n.text.clone();
            }
            fake.notified.notified().await;
        }
    }

    fn configured(provider: &str, model: &str, variant: Option<&str>) -> Option<ConfiguredModel> {
        Some(ConfiguredModel { provider_id: provider.into(), model: model.into(), variant: variant.map(String::from) })
    }

    #[tokio::test]
    async fn reads_input_images_asks_permission_and_saves_the_result() {
        let fake = Arc::new(Fake::default());
        *fake.models.lock() = vec![image_model("test", "painter", &["text", "image"], &[])];
        let (dir, ctx) = setup(fake.clone(), None);
        std::fs::write(dir.path().join("in.png"), base64::engine::general_purpose::STANDARD.decode(PNG).unwrap()).unwrap();
        let result = run(&ctx, json!({ "prompt": "make it blue", "images": ["in.png"], "path": "out.png" })).await.unwrap();
        let file = dir.path().join("out.png");
        let stem = dir.path().join("out").display().to_string();
        let id = result.output.as_ref().unwrap()["id"].as_str().unwrap().to_string();
        assert!(id.starts_with("image_"));
        assert_eq!(result.output, Some(json!({ "id": id, "path": stem, "model": "test/painter" })));
        assert_eq!(
            result.text_content(),
            [
                format!("Image job {id} started in the background with test/painter. It will be saved as {stem} with the extension of its real format (.png, .jpg, .webp or .gif); the notice gives the exact path."),
                "You will be notified automatically when it finishes or fails.".into(),
                "DO NOT sleep, poll, check whether the file exists, or request the same image again while it runs.".into(),
                "Work on other tasks, or briefly tell the user what you started and end your response.".into(),
            ]
            .join("\n")
        );
        assert_eq!(
            notice_of(&fake, &result).await,
            format!("<image id=\"{id}\" state=\"completed\">\nImage request finished. Saved image: {} (test/painter).\nUse the read tool on this path to view the image.\n</image>", file.display())
        );
        assert_eq!(base64::engine::general_purpose::STANDARD.encode(std::fs::read(&file).unwrap()), PNG);
        assert_eq!(
            fake.requests.lock()[0],
            json!({ "model": { "providerID": "test", "id": "painter" }, "prompt": "make it blue", "n": 1, "images": [{ "data": PNG, "mime": "image/png", "name": "in.png" }] })
        );
        let asked: Vec<(String, Vec<String>)> = fake.asked.lock().iter().map(|r| (r.action.clone(), r.resources.clone())).collect();
        assert_eq!(
            asked,
            [
                ("experimental_image_generate".to_string(), vec!["test/painter".to_string()]),
                ("edit".into(), vec!["out.png".into()]),
                ("read".into(), vec!["in.png".into()]),
            ]
        );
    }

    #[tokio::test]
    async fn saves_with_the_extension_of_the_images_real_format() {
        let fake = Arc::new(Fake::default());
        *fake.models.lock() = vec![image_model("test", "painter", &["text"], &[])];
        *fake.result.lock() = Some(Ok(json!({ "images": [{ "type": "image", "data": JPEG, "mime": "image/jpeg" }] })));
        let (dir, ctx) = setup(fake.clone(), None);
        let result = run(&ctx, json!({ "prompt": "a cat" })).await.unwrap();
        let path = result.output.as_ref().unwrap()["path"].as_str().unwrap().to_string();
        assert_eq!(Path::new(&path).parent().unwrap(), ctx.global.tmp.join("images").join("ses_test"));
        assert!(notice_of(&fake, &result).await.contains(&format!("Saved image: {path}.jpg (test/painter).")));
        assert!(Path::new(&format!("{path}.jpg")).exists());
        let explicit = run(&ctx, json!({ "prompt": "a cat", "path": "cat.png" })).await.unwrap();
        assert!(notice_of(&fake, &explicit).await.contains(&format!("Saved image: {} (test/painter).", dir.path().join("cat.jpg").display())));
        let edits = |list: &Mutex<Vec<PermissionRequest>>| -> Vec<Vec<String>> {
            list.lock().iter().filter(|r| r.action == "edit").map(|r| r.resources.clone()).collect()
        };
        assert_eq!(edits(&fake.asked), [vec!["cat.png".to_string()]]);
        assert_eq!(edits(&fake.checked), [vec!["cat.jpg".to_string()]]);
        // Replacing an existing file under the real extension asks for its own edit approval.
        fake.asked.lock().clear();
        fake.checked.lock().clear();
        let again = run(&ctx, json!({ "prompt": "a cat", "path": "cat.png" })).await.unwrap();
        notice_of(&fake, &again).await;
        assert_eq!(edits(&fake.asked), [vec!["cat.png".to_string()], vec!["cat.jpg".to_string()]]);
        assert!(fake.checked.lock().is_empty());
    }

    #[tokio::test]
    async fn reports_a_failed_generation_in_the_completion_notice() {
        let fake = Arc::new(Fake::default());
        *fake.models.lock() = vec![image_model("test", "painter", &["text"], &[])];
        *fake.result.lock() = Some(Err("The prompt was rejected".into()));
        let (dir, ctx) = setup(fake.clone(), None);
        let result = run(&ctx, json!({ "prompt": "x", "path": "out.png" })).await.unwrap();
        let id = result.output.as_ref().unwrap()["id"].as_str().unwrap().to_string();
        assert_eq!(
            notice_of(&fake, &result).await,
            format!("<image id=\"{id}\" state=\"error\">\nImage request failed for {}: The prompt was rejected\n</image>", dir.path().join("out").display())
        );
        assert!(!dir.path().join("out.png").exists());
    }

    #[tokio::test]
    async fn starts_no_job_when_a_permission_is_denied() {
        let fake = Arc::new(Fake::default());
        *fake.models.lock() = vec![image_model("test", "painter", &["text", "image"], &[])];
        *fake.deny.lock() = Some("edit".into());
        let (_dir, ctx) = setup(fake.clone(), None);
        assert!(run(&ctx, json!({ "prompt": "x", "path": "out.png" })).await.is_err());
        let actions: Vec<String> = fake.asked.lock().iter().map(|r| r.action.clone()).collect();
        assert_eq!(actions, ["experimental_image_generate", "edit"]);
        *fake.deny.lock() = Some("webfetch".into());
        fake.asked.lock().clear();
        assert!(run(&ctx, json!({ "prompt": "x", "images": ["https://example.invalid/cat.png"] })).await.is_err());
        let asked: Vec<(String, Vec<String>)> = fake.asked.lock().iter().map(|r| (r.action.clone(), r.resources.clone())).collect();
        assert_eq!(
            asked,
            [("experimental_image_generate".to_string(), vec!["test/painter".to_string()]), ("webfetch".into(), vec!["https://example.invalid/cat.png".into()])]
        );
        assert!(fake.requests.lock().is_empty());
    }

    #[tokio::test]
    async fn validates_an_explicit_model_its_variant_and_its_input_images() {
        let fake = Arc::new(Fake::default());
        *fake.models.lock() = vec![
            image_model("test", "text-only", &["text"], &[]),
            image_model("other", "editor", &["text", "image"], &[]),
            image_model("test", "painter", &["text"], &["hd"]),
        ];
        let (dir, ctx) = setup(fake.clone(), None);
        std::fs::write(dir.path().join("in.png"), base64::engine::general_purpose::STANDARD.decode(PNG).unwrap()).unwrap();
        std::fs::write(dir.path().join("notes.txt"), "hello").unwrap();
        let failure = |r: Result<ToolResult, ToolError>| r.unwrap_err().message;
        assert_eq!(
            failure(run(&ctx, json!({ "prompt": "x", "model": "test/missing" })).await),
            "Model \"test/missing\" is not an available image model. Use the models tool with type: \"image\" to see what is available."
        );
        assert_eq!(
            failure(run(&ctx, json!({ "prompt": "x", "model": "test/text-only", "images": ["in.png"] })).await),
            "Model \"test/text-only\" does not accept input images. Image models that do: other/editor."
        );
        assert_eq!(failure(run(&ctx, json!({ "prompt": "x", "model": "test/painter#bogus" })).await), "Variant \"bogus\" is not available for \"test/painter\". Available: hd.");
        assert_eq!(failure(run(&ctx, json!({ "prompt": "x", "model": "test/text-only#hd" })).await), "Model \"test/text-only\" has no variants. Omit the variant.");
        assert_eq!(failure(run(&ctx, json!({ "prompt": "x", "images": ["notes.txt"] })).await), "notes.txt is not a PNG, JPEG, WebP or GIF image.");
        assert_eq!(failure(run(&ctx, json!({ "prompt": "x", "images": ["missing.png"] })).await), "File not found: missing.png");
        let steered = run(&ctx, json!({ "prompt": "x", "images": ["in.png"] })).await.unwrap();
        assert_eq!(steered.output.as_ref().unwrap()["model"], "other/editor");
        let explicit = run(&ctx, json!({ "prompt": "x", "model": "test/painter#hd" })).await.unwrap();
        assert_eq!(explicit.output.as_ref().unwrap()["model"], "test/painter#hd");
        notice_of(&fake, &steered).await;
        notice_of(&fake, &explicit).await;
        assert_eq!(fake.requests.lock().last().unwrap()["model"], json!({ "providerID": "test", "id": "painter", "variant": "hd" }));
    }

    #[tokio::test]
    async fn uses_the_configured_model_unless_the_agent_picks_one_and_falls_back_with_a_note() {
        let fake = Arc::new(Fake::default());
        *fake.models.lock() = vec![image_model("test", "painter", &["text", "image"], &["hd"]), image_model("other", "text-only", &["text"], &[])];
        let (dir, ctx) = setup(fake.clone(), configured("other", "text-only", None));
        std::fs::write(dir.path().join("in.png"), base64::engine::general_purpose::STANDARD.decode(PNG).unwrap()).unwrap();
        let first = run(&ctx, json!({ "prompt": "x" })).await.unwrap();
        assert_eq!(first.output.as_ref().unwrap()["model"], "other/text-only");
        assert!(!first.text_content().contains("used"));
        assert_eq!(run(&ctx, json!({ "prompt": "x", "model": "test/painter" })).await.unwrap().output.unwrap()["model"], "test/painter");
        let edit = run(&ctx, json!({ "prompt": "x", "images": ["in.png"] })).await.unwrap();
        assert_eq!(edit.output.as_ref().unwrap()["model"], "test/painter");
        assert!(edit.text_content().contains("other/text-only doesn't accept input images; used test/painter."));
        let (_dir, ctx) = setup(fake.clone(), configured("gone", "model", None));
        assert!(run(&ctx, json!({ "prompt": "x" })).await.unwrap().text_content().contains("gone/model isn't available; used test/painter."));
        let (_dir, ctx) = setup(fake.clone(), configured("test", "painter", Some("hd")));
        assert_eq!(run(&ctx, json!({ "prompt": "x" })).await.unwrap().output.unwrap()["model"], "test/painter#hd");
        let (_dir, ctx) = setup(fake.clone(), configured("test", "painter", Some("bogus")));
        assert!(run(&ctx, json!({ "prompt": "x" })).await.unwrap().text_content().contains("test/painter#bogus has no variant \"bogus\"; used "));
    }
}
