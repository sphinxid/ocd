//! Durable video jobs (`core/src/video-generation.ts`, 2.4.1).
//!
//! Each job is a record in kv under `video:<id>`, written once the provider accepted it, so a restart resumes
//! polling. Every 5 seconds each server drops records 24 hours after their job finished and, while jobs are running,
//! tries for the lease (`video-leader`, outside the record prefix): the one holding it checks each running job once,
//! and stores a finished job's videos as blobs. Any server submits jobs and answers reads.

use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::Duration;

use parking_lot::Mutex;
use rusqlite::OptionalExtension;
use serde_json::{json, Value};

use oc_schema::types::{LocationRef, ModelRef};

use crate::generate::{GenerateError, VideoInput};
use crate::runner::Engine;

const INTERVAL: Duration = Duration::from_secs(5);
const DEADLINE_MS: i64 = 2 * 3600 * 1000;
/// How long a finished job's record is kept. Its videos are blobs, which keep their own week-long retention.
const RETENTION_MS: i64 = 24 * 3600 * 1000;
const PREFIX: &str = "video:";
const LEADER: &str = "video-leader";
const TTL_MS: i64 = 60 * 1000;

/// `Generate.VideoErrorReason`.
pub const REASONS: [&str; 4] = ["failed", "cancelled", "expired", "unknown"];

/// `VideoGeneration.NotFoundError`.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{message}")]
pub struct NotFound {
    pub id: String,
    pub message: String,
}

impl NotFound {
    fn new(id: &str) -> Self {
        Self { id: id.to_string(), message: format!("Video not found: {id}") }
    }
}

/// `Generate.VideoJob`.
#[derive(Debug, Clone, PartialEq)]
pub enum VideoJob {
    Running { id: String },
    Completed { id: String, blobs: Vec<crate::blob::Info> },
    Error { id: String, reason: String, error: String },
}

impl VideoJob {
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running { .. })
    }

    /// The API shape (`Generate.VideoRunning | VideoCompleted | VideoError`).
    pub fn to_value(&self) -> Value {
        match self {
            Self::Running { id } => json!({ "id": id, "status": "running" }),
            Self::Completed { id, blobs } => json!({
                "id": id,
                "status": "completed",
                "blobs": blobs.iter().map(|b| json!({ "id": b.id, "mime": b.mime, "bytes": b.bytes })).collect::<Vec<_>>(),
            }),
            Self::Error { id, reason, error } => json!({ "id": id, "status": "error", "error": error, "reason": reason }),
        }
    }
}

/// `VideoID`: strict, because the server names files after it.
pub fn valid_id(id: &str) -> bool {
    let Some(rest) = id.strip_prefix("video_") else { return false };
    rest.len() == 26
        && rest[..12].bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && rest[12..].bytes().all(|b| b.is_ascii_alphanumeric())
}

/// A stored job record. `remote` is the provider's ID for the job; it never leaves this service.
#[derive(Debug, Clone, PartialEq)]
struct Record {
    status: String,
    remote: Option<String>,
    blobs: Vec<String>,
    reason: Option<String>,
    error: Option<String>,
    finished: Option<i64>,
    model: ModelRef,
    location: LocationRef,
    created: i64,
}

impl Record {
    fn decode(value: &Value) -> Option<Self> {
        let o = value.as_object()?;
        let model: ModelRef = serde_json::from_value(o.get("model")?.clone()).ok()?;
        let location: LocationRef = serde_json::from_value(o.get("location")?.clone()).ok()?;
        let created = o.get("created")?.as_f64()? as i64;
        let status = o.get("status")?.as_str()?.to_string();
        let finished = o.get("finished").and_then(Value::as_f64).map(|f| f as i64);
        let record = Self { status: status.clone(), remote: None, blobs: vec![], reason: None, error: None, finished, model, location, created };
        match status.as_str() {
            "running" => Some(Self { remote: Some(o.get("remote")?.as_str()?.to_string()), ..record }),
            "completed" => {
                let blobs = o.get("blobs")?.as_array()?.iter().map(|b| b.as_str().filter(|id| crate::blob::valid_id(id)).map(String::from)).collect::<Option<Vec<_>>>()?;
                finished?;
                Some(Self { blobs, ..record })
            }
            "error" => {
                let reason = o.get("reason")?.as_str().filter(|r| REASONS.contains(r))?.to_string();
                let error = o.get("error")?.as_str()?.to_string();
                finished?;
                Some(Self { reason: Some(reason), error: Some(error), ..record })
            }
            _ => None,
        }
    }

    fn encode(&self) -> Value {
        let mut out = serde_json::Map::new();
        out.insert("status".into(), json!(self.status));
        match self.status.as_str() {
            "running" => {
                out.insert("remote".into(), json!(self.remote));
            }
            "completed" => {
                out.insert("blobs".into(), json!(self.blobs));
            }
            _ => {
                out.insert("reason".into(), json!(self.reason));
                out.insert("error".into(), json!(self.error));
            }
        }
        if let Some(f) = self.finished {
            out.insert("finished".into(), json!(f));
        }
        out.insert("model".into(), serde_json::to_value(&self.model).unwrap_or(Value::Null));
        out.insert("location".into(), serde_json::to_value(&self.location).unwrap_or(Value::Null));
        out.insert("created".into(), json!(self.created));
        Value::Object(out)
    }

    /// A job that never recorded finishing is treated as finished at its deadline.
    fn expired(&self, now: i64) -> bool {
        let finished = if self.status == "running" { self.created + DEADLINE_MS } else { self.finished.unwrap_or(0) };
        finished + RETENTION_MS <= now
    }
}

enum Outcome {
    Completed(Vec<String>),
    Error { reason: String, error: String },
}

fn now_ms() -> i64 {
    oc_schema::id::now_ms() as i64
}

/// `VideoGeneration.Service`.
pub struct VideoGeneration {
    engine: Weak<Engine>,
    owner: String,
    /// The check running for each job (`FiberMap`).
    checks: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
    started: std::sync::atomic::AtomicBool,
}

impl VideoGeneration {
    pub fn new(engine: Weak<Engine>) -> Self {
        Self { engine, owner: uuid_like(), checks: Mutex::new(HashMap::new()), started: std::sync::atomic::AtomicBool::new(false) }
    }

    fn engine(&self) -> Option<Arc<Engine>> {
        self.engine.upgrade()
    }

    fn db(&self) -> Option<oc_storage::Database> {
        Some(self.engine()?.app.db.clone())
    }

    fn data(&self) -> Option<std::path::PathBuf> {
        Some(self.engine()?.app.global.data.clone())
    }

    /// The same base-config Location the generate routes use.
    pub fn base(&self) -> Option<LocationRef> {
        Some(LocationRef { directory: self.engine()?.app.global.config.to_string_lossy().into_owned(), workspace_id: None })
    }

    fn load(&self, id: &str) -> Option<Record> {
        let value = self.db()?.kv_get(&format!("{PREFIX}{id}")).ok()??;
        Record::decode(&value)
    }

    /// Takes the lease if it's free, expired or already ours.
    fn claim(&self) -> anyhow::Result<bool> {
        let db = self.db().ok_or_else(|| anyhow::anyhow!("shutting down"))?;
        let now = now_ms();
        let value = json!({ "owner": self.owner, "until": now + TTL_MS }).to_string();
        let claimed = db.with(|c| {
            c.query_row(
                "INSERT INTO kv (key, value, time_created, time_updated) VALUES (?1, ?2, ?3, ?3) \
                 ON CONFLICT (key) DO UPDATE SET value = ?2, time_updated = ?3 \
                 WHERE json_extract(kv.value, '$.owner') = ?4 OR json_extract(kv.value, '$.until') <= ?3 \
                 RETURNING key",
                rusqlite::params![LEADER, value, now, self.owner],
                |_| Ok(()),
            )
            .optional()
        })?;
        Ok(claimed.is_some())
    }

    /// Drops the lease if this server holds it (the layer's finalizer).
    pub fn release(&self) {
        if let Some(db) = self.db() {
            let _ = db.with(|c| c.execute("DELETE FROM kv WHERE key = ?1 AND json_extract(value, '$.owner') = ?2", rusqlite::params![LEADER, self.owner]));
        }
    }

    /// Writes the outcome only if the job is still running; one cancelled meanwhile, possibly by another server, keeps
    /// its record and drops what this one downloaded.
    async fn finish(&self, id: &str, job: &Record, outcome: Outcome) {
        let Some(db) = self.db() else { return };
        let now = now_ms();
        let mut record = Record { finished: Some(now), remote: None, ..job.clone() };
        let downloaded = match outcome {
            Outcome::Completed(blobs) => {
                record.status = "completed".into();
                record.blobs = blobs.clone();
                blobs
            }
            Outcome::Error { reason, error } => {
                record.status = "error".into();
                record.reason = Some(reason);
                record.error = Some(error);
                vec![]
            }
        };
        let value = record.encode().to_string();
        let key = format!("{PREFIX}{id}");
        let updated = db
            .with(|c| {
                c.query_row(
                    "UPDATE kv SET value = ?1, time_updated = ?2 WHERE key = ?3 AND json_extract(value, '$.status') = 'running' RETURNING key",
                    rusqlite::params![value, now, key],
                    |_| Ok(()),
                )
                .optional()
            })
            .ok()
            .flatten();
        if updated.is_none()
            && let Some(data) = self.data()
        {
            for blob in downloaded {
                crate::blob::remove(&data, &blob).await;
            }
        }
    }

    /// All or nothing: a job never reports only some of its videos.
    async fn download(&self, job: &Record) -> Result<Vec<String>, String> {
        let engine = self.engine().ok_or("The server is shutting down")?;
        let data = engine.app.global.data.clone();
        let remote = job.remote.clone().unwrap_or_default();
        let videos = engine.generate_video_result(&job.location.directory, &job.model, &remote).await.map_err(|e| e.to_string())?;
        let mut written: Vec<String> = vec![];
        for video in &videos {
            let result = async {
                let stream = engine.generate_video_open(&job.model, video).await.map_err(|e| e.to_string())?;
                let info = crate::blob::write(&data, stream).await.map_err(|e| match e {
                    crate::blob::WriteError::Stream(e) => e.to_string(),
                    crate::blob::WriteError::Unavailable(m) => m,
                })?;
                written.push(info.id.clone());
                if info.bytes == 0 {
                    return Err("The provider returned an empty video".to_string());
                }
                Ok(())
            }
            .await;
            if let Err(e) = result {
                for blob in &written {
                    crate::blob::remove(&data, blob).await;
                }
                return Err(e);
            }
        }
        Ok(written)
    }

    /// A status read that fails is tried again at the next check: the provider may be briefly unavailable.
    async fn check(&self, id: &str, job: &Record, now: i64) {
        if now >= job.created + DEADLINE_MS {
            let error = "The provider had no result 2h after submission".to_string();
            return self.finish(id, job, Outcome::Error { reason: "unknown".into(), error }).await;
        }
        let Some(engine) = self.engine() else { return };
        let remote = job.remote.clone().unwrap_or_default();
        let status = match engine.generate_video_status(&job.location.directory, &job.model, &remote).await {
            Ok(status) => status,
            Err(error) => {
                tracing::warn!(id, error = %error, "video status read failed");
                return;
            }
        };
        match status.status {
            oc_ai::video::Status::Running => {}
            oc_ai::video::Status::Error => {
                let reason = status.reason.map(|r| r.as_str()).unwrap_or("failed").to_string();
                let error = format!("The provider reported the job as {reason}");
                self.finish(id, job, Outcome::Error { reason, error }).await;
            }
            oc_ai::video::Status::Completed => {
                // Two retries, exponential from 1 second.
                let mut attempt = 0;
                let outcome = loop {
                    match self.download(job).await {
                        Ok(written) => break Outcome::Completed(written),
                        Err(_) if attempt < 2 => {
                            tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
                            attempt += 1;
                        }
                        Err(error) => break Outcome::Error { reason: "failed".into(), error },
                    }
                };
                self.finish(id, job, outcome).await;
            }
        }
    }

    /// `start`: returns once the provider accepted the job; the server then polls it and stores the videos as blobs.
    /// The submission runs on its own task, so a client that disconnects can't interrupt a paid submission midway.
    pub async fn start(self: &Arc<Self>, input: VideoInput, location: Option<LocationRef>) -> Result<String, GenerateError> {
        let unavailable = || GenerateError::Unavailable { message: "The server is shutting down".into(), service: None };
        let location = location.or_else(|| self.base()).ok_or_else(unavailable)?;
        let this = self.clone();
        let submit = tokio::spawn(async move {
            let engine = this.engine().ok_or_else(unavailable)?;
            let model = input.model.clone();
            let remote = engine.generate_video_start(&location.directory, input).await?;
            let id = oc_schema::id::descending("video");
            let record = Record {
                status: "running".into(),
                remote: Some(remote),
                blobs: vec![],
                reason: None,
                error: None,
                finished: None,
                model,
                location,
                created: now_ms(),
            };
            engine
                .app
                .db
                .kv_set(&format!("{PREFIX}{id}"), &record.encode())
                .map_err(|e| GenerateError::Unavailable { message: e.to_string(), service: None })?;
            Ok(id)
        });
        submit.await.map_err(|e| GenerateError::Unavailable { message: e.to_string(), service: None })?
    }

    /// `get`: the job, or not found 24 hours after it finished or once none of its blobs remain. A deleted blob is
    /// dropped.
    pub async fn get(&self, id: &str) -> Result<VideoJob, NotFound> {
        let Some(job) = self.load(id).filter(|job| !job.expired(now_ms())) else { return Err(NotFound::new(id)) };
        match job.status.as_str() {
            "running" => Ok(VideoJob::Running { id: id.to_string() }),
            "error" => Ok(VideoJob::Error { id: id.to_string(), reason: job.reason.unwrap_or_default(), error: job.error.unwrap_or_default() }),
            _ => {
                let data = self.data().ok_or_else(|| NotFound::new(id))?;
                let mut infos = vec![];
                for blob in &job.blobs {
                    if let Some(content) = crate::blob::read(&data, blob).await {
                        infos.push(crate::blob::Info { id: content.id, mime: content.mime, bytes: content.bytes });
                    }
                }
                if infos.is_empty() {
                    return Err(NotFound::new(id));
                }
                Ok(VideoJob::Completed { id: id.to_string(), blobs: infos })
            }
        }
    }

    /// `wait`: until the job completes or fails, whichever server polls it. Reads the record, so it works anywhere.
    pub async fn wait(&self, id: &str) -> Result<VideoJob, NotFound> {
        loop {
            let job = self.get(id).await?;
            if !job.is_running() {
                return Ok(job);
            }
            tokio::time::sleep(INTERVAL).await;
        }
    }

    /// `cancel`: fails a running job as cancelled, and asks the provider to cancel it if it can.
    pub async fn cancel(&self, id: &str) -> Result<(), NotFound> {
        let Some(job) = self.load(id) else { return Err(NotFound::new(id)) };
        if job.status != "running" {
            return Ok(());
        }
        if let Some(check) = self.checks.lock().remove(id) {
            check.abort();
        }
        self.finish(id, &job, Outcome::Error { reason: "cancelled".into(), error: "The video generation was cancelled".into() }).await;
        // Best effort: some providers can't cancel, and the paid job then runs to completion on their side.
        if let Some(engine) = self.engine() {
            let _ = engine.generate_video_cancel(&job.location.directory, &job.model, job.remote.as_deref().unwrap_or_default()).await;
        }
        Ok(())
    }

    fn rows(&self) -> Vec<(String, Value)> {
        let Some(db) = self.db() else { return vec![] };
        let mut found = vec![];
        let mut after: Option<String> = None;
        loop {
            let Ok((entries, next)) = db.kv_scan(PREFIX, after.as_deref(), Some(500)) else { return found };
            found.extend(entries);
            match next {
                Some(n) => after = Some(n),
                None => return found,
            }
        }
    }

    /// One check pass. Records that can't be read are removed with the expired ones. A claim that fails, such as on a
    /// busy database, counts as not leading, so two servers never poll at once; losing the lease stops this server's
    /// checks.
    pub async fn tick(self: &Arc<Self>) {
        let now = now_ms();
        let mut running = vec![];
        for (key, value) in self.rows() {
            let id = key[PREFIX.len()..].to_string();
            match Record::decode(&value).filter(|job| !job.expired(now)) {
                None => {
                    if let Some(db) = self.db() {
                        let _ = db.kv_remove(&key);
                    }
                }
                Some(job) if job.status == "running" && valid_id(&id) => running.push((id, job)),
                Some(_) => {}
            }
        }
        let leading = !running.is_empty()
            && match self.claim() {
                Ok(claimed) => claimed,
                Err(error) => {
                    tracing::warn!(error = %error, "video leader claim failed");
                    false
                }
            };
        let mut checks = self.checks.lock();
        checks.retain(|_, check| !check.is_finished());
        if !leading {
            for (_, check) in checks.drain() {
                check.abort();
            }
            return;
        }
        for (id, job) in running {
            if checks.contains_key(&id) {
                continue;
            }
            let this = self.clone();
            let key = id.clone();
            checks.insert(key, tokio::spawn(async move { this.check(&id, &job, now).await }));
        }
    }

    /// Starts the check loop once (the layer's forked repeat).
    pub fn run(self: &Arc<Self>) {
        if self.started.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let this = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                let Some(service) = this.upgrade() else { return };
                service.tick().await;
                drop(service);
                tokio::time::sleep(INTERVAL).await;
            }
        });
    }
}

fn uuid_like() -> String {
    let bytes: [u8; 16] = rand::random();
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(status: &str) -> Record {
        Record {
            status: status.into(),
            remote: (status == "running").then(|| "remote_1".into()),
            blobs: vec![],
            reason: None,
            error: None,
            finished: (status != "running").then_some(1_000),
            model: ModelRef { id: "veo".into(), provider_id: "google".into(), variant: None },
            location: LocationRef { directory: "/tmp/x".into(), workspace_id: None },
            created: 0,
        }
    }

    #[test]
    fn records_round_trip_and_expire() {
        let running = record("running");
        assert_eq!(Record::decode(&running.encode()), Some(running.clone()));
        assert!(!running.expired(DEADLINE_MS + RETENTION_MS - 1));
        assert!(running.expired(DEADLINE_MS + RETENTION_MS));
        let done = Record { blobs: vec![crate::blob::create_id()], ..record("completed") };
        assert_eq!(Record::decode(&done.encode()), Some(done.clone()));
        assert!(done.expired(1_000 + RETENTION_MS));
        // A record that can't be read is dropped.
        assert_eq!(Record::decode(&json!({ "status": "completed", "blobs": ["../x"] })), None);
        assert_eq!(Record::decode(&json!({ "status": "nope" })), None);
    }

    #[test]
    fn video_ids_are_strict() {
        assert!(valid_id(&oc_schema::id::descending("video")));
        assert!(!valid_id("video_x"));
        assert!(!valid_id("blob_0123456789abABCDEFGHIJKLMN"));
    }
}

/// Reads a tool's or route's media (`{type: "bytes", data, mediaType}`, `{type: "url", url, mediaType?}`, or a
/// `Generate.File` `{data, mime}`) as an asset.
fn asset(value: &Value) -> Option<oc_ai::MediaAsset> {
    let media_type = value.get("mediaType").or_else(|| value.get("mime")).and_then(Value::as_str).map(String::from);
    match value.get("type").and_then(Value::as_str) {
        Some("url") => Some(oc_ai::MediaAsset::url(value.get("url")?.as_str()?, media_type)),
        _ => Some(oc_ai::MediaAsset::base64(value.get("data")?.as_str()?, media_type.unwrap_or_default())),
    }
}

/// The video tool's request (see `ToolHost::video_start`) as `Generate.VideoInput`.
pub fn tool_input(request: &Value) -> Result<VideoInput, GenerateError> {
    let model: ModelRef = serde_json::from_value(request["model"].clone())
        .map_err(|e| GenerateError::InvalidRequest { message: e.to_string(), field: Some("model".into()) })?;
    Ok(VideoInput {
        model,
        prompt: request["prompt"].as_str().unwrap_or_default().to_string(),
        start: request.get("start").and_then(asset),
        end: request.get("end").and_then(asset),
        references: request.get("references").and_then(Value::as_array).into_iter().flatten().filter_map(asset).collect(),
        duration: request.get("duration").and_then(Value::as_f64),
        aspect: request.get("aspect").and_then(Value::as_str).map(String::from),
        resolution: request.get("resolution").and_then(Value::as_str).map(String::from),
        sound: request.get("sound").and_then(Value::as_bool),
    })
}

const CHUNK: usize = 4 * 1024 * 1024;

/// Copies one blob into the workspace, streamed so a video is never held in memory.
async fn copy(from: &std::path::Path, to: &str) -> std::io::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    if let Some(parent) = std::path::Path::new(to).parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut source = tokio::fs::File::open(from).await?;
    let mut target = tokio::fs::File::create(to).await?;
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = source.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        target.write_all(&buf[..n]).await?;
    }
    target.flush().await
}

impl Engine {
    /// `VideoGenerateTool.track`: wait for a video the server is generating, then copy each of its videos from the blob
    /// store into the workspace, as a background job a restarted server can take over from the same recovery record.
    /// The job's outcome then becomes the session's `<background-result kind="video">` notice.
    pub fn track_video(self: &Arc<Self>, track: oc_tools::host::VideoTrack) -> oc_tools::host::JobInfo {
        use oc_tools::host::{JobRecovery, JobStatus};
        use oc_tools::tools::video_generate::{finished_text, notice, video_path, NAME};
        let videos = self.videos.clone();
        let data = self.app.global.data.clone();
        let (video_id, path) = (track.video_id.clone(), track.path.clone());
        let run = {
            let (video_id, path) = (video_id.clone(), path.clone());
            async move {
                let outcome: Result<String, String> = async {
                    let job = videos.wait(&video_id).await.map_err(|e| e.message)?;
                    let blobs = match job {
                        VideoJob::Error { error, .. } => return Err(error),
                        VideoJob::Completed { blobs, .. } => blobs,
                        VideoJob::Running { .. } => vec![],
                    };
                    let mut files = vec![];
                    for (index, blob) in blobs.iter().enumerate() {
                        let file = video_path(&path, index);
                        // The blob is on the server's disk; the copy is a workspace write.
                        let content = crate::blob::read(&data, &blob.id).await.ok_or_else(|| format!("Blob not found: {}", blob.id))?;
                        copy(&content.path, &file).await.map_err(|e| e.to_string())?;
                        files.push(file);
                    }
                    Ok(finished_text(&files))
                }
                .await;
                // The job keeps this text, so a restart can still deliver the outcome.
                outcome.map_err(|message| format!("Video request failed for {path}: {message}"))
            }
        };
        let started = self.app.jobs.start_input(crate::job::StartInput {
            id: video_id.clone(),
            kind: NAME.into(),
            title: Some(track.title.clone().unwrap_or_else(|| path.clone())),
            metadata_session: Some(track.session_id.clone()),
            recovery: Some(JobRecovery::Video { session_id: track.session_id.clone(), path: path.clone(), video_id: video_id.clone() }),
            notification_id: track.notification_id.clone(),
            background: true,
            run: Box::pin(run),
        });
        let engine = Arc::downgrade(self);
        let session_id = track.session_id.clone();
        tokio::spawn(async move {
            let Some(app) = engine.upgrade().map(|e| e.app.clone()) else { return };
            let Some(info) = app.jobs.wait(&video_id).await else { return };
            if info.status == JobStatus::Running {
                return;
            }
            // Cancelling the job cancels the generation (`cancel: videos.cancel(id)`).
            if info.status == JobStatus::Cancelled
                && let Some(engine) = engine.upgrade()
            {
                let _ = engine.videos.cancel(&video_id).await;
            }
            let body = match info.status {
                JobStatus::Completed => info.output.clone().unwrap_or_default(),
                JobStatus::Error => info.error.clone().unwrap_or_else(|| format!("Video request failed for {path}.")),
                _ => format!("Video request for {path} was cancelled."),
            };
            let state = info.status.as_str();
            let mut metadata = serde_json::Map::new();
            metadata.insert("source".into(), json!(NAME));
            metadata.insert("jobID".into(), json!(video_id));
            metadata.insert("state".into(), json!(state));
            metadata.insert("path".into(), json!(path));
            let admitted = app
                .sessions
                .synthetic(&session_id, info.notification_id.clone(), notice(&video_id, state, &body), Some("Video generation".into()), Some(metadata), None)
                .await;
            if admitted.is_ok()
                && let Some(c) = app.coordinator.get()
            {
                c.wake(&session_id, crate::inbox::Scope::Input);
            }
            if let Some(n) = &info.notification_id {
                app.jobs.complete_background(n);
            }
        });
        started
    }
}
