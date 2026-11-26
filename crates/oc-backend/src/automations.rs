//! `automation.*`, `generate.video.*` and `blob.get` in the `server.experimental` group.
//!
//! Automations (`server/src/handlers/automation.ts`) are on by default; `automations: false` in the config of an
//! automation's directory turns its routes into `403`s. An automation whose folder is gone stays manageable, so it can
//! be inspected and deleted. The checks live in the core service (2.4.1, #65747), which plugins call too.
//! This module also gives the automation service its prompt path (the session API) and starts its loop. Video
//! generation (`handlers/generate.ts`) answers `403` behind `experimental.video_generate`, which ocd does not port.
//! Blobs (`handlers/blob.ts`, `core/src/blob-store.ts`) are files under `<data>/blobs/`, served without a flag; the
//! server deletes video blobs older than a week at startup and hourly.

use std::sync::{Arc, Weak};

use futures::future::BoxFuture;
use oc_core::automation::{AutomationError, CreateInput, ListInput};
use oc_server::generated as api;
use oc_server::{ApiError, BlobFile};
use rusqlite::OptionalExtension;
use serde_json::{Map, Value};

use crate::engine::Engine;
use crate::util::{defect, from_json};

/// `Automation.DisabledError`'s message.
pub const AUTOMATIONS_DISABLED: &str = oc_core::automation::DISABLED;
/// What `generate.video.*` answers while `experimental.video_generate` is off.
pub const VIDEO_DISABLED: &str = "Video generation is disabled; enable experimental.video_generate";
/// What `machine.list`, `machine.add` and `machine.remove` answer while the `machines` flag is off (2.4.1
/// `handlers/machine.ts`). ocd has no machines yet, so it answers this whatever the flag says.
pub const MACHINES_DISABLED: &str = "Machines are disabled; enable experimental.machines";
/// `BlobStore.DIRECTORY`, under the data folder.
pub const BLOB_DIRECTORY: &str = "blobs";
/// `BlobStore.RETENTION`: blobs are deleted 7 days after they're written.
pub const BLOB_RETENTION: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 3600);

/// `BlobStore.layer`'s cleanup, which belongs to the server: at startup and hourly, video blobs (`video_*`, and their
/// leftover `.part` files) older than the retention are deleted from `<data>/blobs/`.
pub(crate) fn start_blob_cleanup(engine: &std::sync::Arc<Engine>) {
    let directory = engine.app.global.data.join(BLOB_DIRECTORY);
    tokio::spawn(async move {
        loop {
            cleanup_blobs(&directory).await;
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        }
    });
}

/// One pass of the blob cleanup.
pub async fn cleanup_blobs(directory: &std::path::Path) {
    let mut files = Vec::new();
    if let Ok(mut entries) = tokio::fs::read_dir(directory).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            if entry.file_name().to_string_lossy().starts_with("video_") {
                files.push(entry.path());
            }
        }
    }
    oc_git::file_retention::cleanup(files, BLOB_RETENTION).await;
}


/// `sessions.prompt` for an automation run: the session API's prompt (plugin activation, the `session.prompt` hook,
/// admission and the wake), delivered as a steer.
struct EnginePrompter(Weak<Engine>);

impl oc_core::automation::Prompter for EnginePrompter {
    fn prompt(&self, session_id: String, id: String, text: String, metadata: Map<String, Value>) -> BoxFuture<'static, Result<(), AutomationError>> {
        let engine = self.0.upgrade();
        Box::pin(async move {
            let other = |e: String| AutomationError::Other(e);
            let engine = engine.ok_or_else(|| other("The server is shutting down".into()))?;
            let session = engine.session(&session_id).map_err(|e| other(e.to_string()))?;
            // A new admission needs the session's location (`instances.provide(session)`); one already admitted or
            // delivered is reconciled without it.
            let known = engine
                .app
                .db
                .with(|c| {
                    c.query_row("SELECT 1 FROM session_inbox WHERE id = ?1 UNION ALL SELECT 1 FROM session_message WHERE id = ?1", [&id], |_| Ok(()))
                        .optional()
                })
                .ok()
                .flatten()
                .is_some();
            if !known && !std::path::Path::new(&session.location.directory).is_dir() {
                return Err(AutomationError::DirectoryNotFound(session.location.directory));
            }
            let body = api::SessionPromptBody {
                id: Some(id),
                text,
                files: None,
                agents: None,
                skills: None,
                metadata: Some(from_json(Value::Object(metadata)).map_err(|e| other(e.to_string()))?),
                delivery: Some(from_json(Value::String("steer".into())).map_err(|e| other(e.to_string()))?),
                resume: None,
            };
            engine.prompt(&session_id, body).await.map(|_| ()).map_err(|e| other(e.to_string()))
        })
    }

    fn wake(&self, session_id: &str) {
        if let Some(engine) = self.0.upgrade() {
            engine.wake(session_id, oc_core::inbox::Scope::Input);
        }
    }

    /// Reading config boots the directory's location (`locations.get`), announcing it like any first use.
    fn boot(&self, directory: &str) -> BoxFuture<'static, ()> {
        let engine = self.0.upgrade();
        let directory = directory.to_string();
        Box::pin(async move {
            if let Some(engine) = engine {
                engine.automation_boot(&directory).await;
            }
        })
    }
}

/// Connects the automation service to this server, and runs its loop in the background (the lease makes one
/// process per database run them).
pub fn start(engine: &Arc<Engine>) {
    connect(engine);
    tokio::spawn(engine.automations.clone().run());
}

/// Routes the automation service's prompts through this server's session API (without starting the loop).
pub fn connect(engine: &Arc<Engine>) {
    engine.automations.set_prompter(Arc::new(EnginePrompter(Arc::downgrade(engine))));
}

fn disabled() -> ApiError {
    ApiError::forbidden(AUTOMATIONS_DISABLED)
}

fn invalid(message: String) -> ApiError {
    ApiError::invalid_request(message).with_kind("automation")
}

/// `managed(effect)`: a missing automation is a 404, one whose directory has automations off a 403.
fn managed(error: AutomationError, id: &str) -> ApiError {
    match error {
        AutomationError::Disabled => disabled(),
        _ => ApiError::automation_not_found(id),
    }
}

fn info(value: Value) -> Result<api::AutomationInfo, ApiError> {
    from_json(value)
}

impl Engine {
    /// Reading config boots the directory's location (`locations.get`), announcing it like any first use.
    pub(crate) async fn automation_boot(&self, directory: &str) {
        if oc_core::automation::is_absolute(directory) && std::path::Path::new(directory).is_dir() {
            let _ = self.locations.get(directory).await;
        }
    }

    /// `create`: stateless HTTP callers have no guaranteed UI listener to answer an interactive ask, so asks are
    /// settled unattended.
    pub async fn automation_create(&self, payload: &api::AutomationCreatePayload) -> Result<api::AutomationInfo, ApiError> {
        let input = CreateInput::from_value(&serde_json::to_value(payload).map_err(defect)?);
        let directory = match (&input.session_id, &input.directory) {
            (Some(id), _) => self.session(id).ok().map(|s| s.location.directory),
            (None, Some(directory)) => Some(directory.clone()),
            (None, None) => Some(self.app.global.home.display().to_string()),
        };
        if let Some(directory) = directory {
            self.automation_boot(&directory).await;
        }
        match oc_core::permission::unattended(Vec::new(), self.automations.create(input, None)).await {
            Ok(value) => info(value),
            Err(AutomationError::Invalid(message)) => Err(invalid(message)),
            Err(AutomationError::Disabled) => Err(disabled()),
            Err(AutomationError::Blocked(message)) | Err(AutomationError::Corrected(message)) => Err(ApiError::forbidden(message)),
            Err(AutomationError::SessionNotFound(id)) => Err(ApiError::session_not_found(&id)),
            Err(other) => Err(defect(other.message())),
        }
    }

    /// `list`: the core service checks the filters (a session must exist and an off directory answers 403) and leaves
    /// out automations whose directory has automations off.
    pub async fn automation_list(&self, req: &api::ExperimentalAutomationListRequest) -> Result<Vec<api::AutomationInfo>, ApiError> {
        let input = ListInput { session_id: req.session_id.clone(), project_id: None, directory: req.directory.clone() };
        match self.automations.list(&input).await {
            Ok(rows) => rows.into_iter().map(info).collect(),
            Err(AutomationError::Invalid(message)) => Err(invalid(message)),
            Err(AutomationError::Disabled) => Err(disabled()),
            Err(AutomationError::SessionNotFound(id)) => Err(ApiError::session_not_found(&id)),
            Err(other) => Err(defect(other.message())),
        }
    }

    pub async fn automation_get(&self, id: &str) -> Result<api::AutomationInfo, ApiError> {
        info(self.automations.get(id).await.map_err(|e| managed(e, id))?)
    }

    pub async fn automation_pause(&self, id: &str) -> Result<api::AutomationInfo, ApiError> {
        info(self.automations.pause(id).await.map_err(|e| managed(e, id))?)
    }

    pub async fn automation_resume(&self, id: &str) -> Result<api::AutomationInfo, ApiError> {
        info(self.automations.resume(id).await.map_err(|e| managed(e, id))?)
    }

    pub async fn automation_delete(&self, id: &str) -> Result<(), ApiError> {
        self.automations.remove(id).await.map_err(|e| managed(e, id))
    }

    /// `BlobStore.read`: the file named by the ID, its size, and the type read from its first 64 bytes.
    pub async fn blob_read(&self, id: &str) -> Result<BlobFile, ApiError> {
        let path = self.app.global.data.join(BLOB_DIRECTORY).join(id);
        let missing = || ApiError::blob_not_found(id);
        let meta = tokio::fs::metadata(&path).await.map_err(|_| missing())?;
        if !meta.is_file() {
            return Err(missing());
        }
        let head = {
            use tokio::io::AsyncReadExt;
            let mut file = tokio::fs::File::open(&path).await.map_err(|_| missing())?;
            let mut buf = Vec::with_capacity(64);
            (&mut file).take(64).read_to_end(&mut buf).await.map_err(|_| missing())?;
            buf
        };
        let mime = oc_ai::media::detect_media_type(&head).unwrap_or("application/octet-stream").to_string();
        Ok(BlobFile { path, mime, bytes: meta.len() })
    }
}
