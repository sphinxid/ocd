//! Background/foreground jobs (`core/src/job.ts`) and subagent jobs
//! (`core/session/subagent-job.ts`, `subagent-completion.ts`).
//!
//! Backgrounded jobs with a recovery descriptor are kept in the SQLite `job` table (`core/src/job/sql.ts`,
//! migration `20261030054400_job`) until their completion notice is admitted, so a restart can settle them.

use std::collections::HashMap;

use futures::future::BoxFuture;
use indexmap::IndexMap;
use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use oc_storage::Database;
use oc_tools::host::{JobBlockResult, JobInfo, JobRecovery, JobStatus};

const COMPLETED_LIMIT: usize = 25;
pub const NO_TEXT: &str = "Subagent completed without a text response.";

struct Active {
    info: JobInfo,
    done: watch::Sender<Option<JobInfo>>,
    backgrounded: watch::Sender<Option<JobInfo>>,
    cancel: CancellationToken,
    generation: u64,
    blocking: HashMap<String, u32>,
    is_bg: bool,
    consumed: bool,
    recovery: Option<JobRecovery>,
    /// `info.metadata.sessionID`, which `matchesSession` checks first.
    metadata_session: Option<String>,
}

/// `ownerSessionID`: the parent session for a subagent.
fn owner_session(recovery: &JobRecovery) -> &str {
    match recovery {
        JobRecovery::Shell { session_id, .. } | JobRecovery::Image { session_id, .. } => session_id,
        JobRecovery::Subagent { parent_session_id, .. } => parent_session_id,
    }
}

/// `recoveryTitle`: the job's own title, else the shell command, subagent description or image path.
fn recovery_title<'a>(job: &'a Active, recovery: &'a JobRecovery) -> &'a str {
    if let Some(title) = &job.info.title {
        return title;
    }
    match recovery {
        JobRecovery::Shell { command, .. } => command,
        JobRecovery::Subagent { description, .. } => description,
        JobRecovery::Image { path, .. } => path,
    }
}

fn recovery_kind(recovery: &JobRecovery) -> &'static str {
    match recovery {
        JobRecovery::Shell { .. } => "shell",
        JobRecovery::Subagent { .. } => "subagent",
        JobRecovery::Image { .. } => "image",
    }
}

/// `matchesSession`
fn matches_session(job: &Active, session: &str) -> bool {
    if job.metadata_session.as_deref() == Some(session) || job.blocking.contains_key(session) {
        return true;
    }
    match &job.recovery {
        None => false,
        Some(JobRecovery::Subagent { parent_session_id, child_session_id, .. }) => {
            parent_session_id == session || child_session_id == session
        }
        Some(JobRecovery::Shell { session_id, .. } | JobRecovery::Image { session_id, .. }) => session_id == session,
    }
}

pub struct Jobs {
    db: Database,
    jobs: Mutex<IndexMap<String, Active>>,
    generation: Mutex<u64>,
}

fn now() -> u64 {
    oc_schema::id::now_ms()
}

async fn await_some(mut rx: watch::Receiver<Option<JobInfo>>) -> JobInfo {
    loop {
        if let Some(v) = rx.borrow().clone() {
            return v;
        }
        if rx.changed().await.is_err() {
            futures::future::pending::<()>().await;
        }
    }
}

impl Jobs {
    pub fn new(db: Database) -> Self {
        Jobs { db, jobs: Mutex::new(IndexMap::new()), generation: Mutex::new(0) }
    }

    /// `persistBackground`: upsert on `notification_id`; a conflict only updates the status, output, error,
    /// recovery and update time.
    fn persist(&self, job: &Active) {
        let (Some(recovery), Some(nid)) = (&job.recovery, &job.info.notification_id) else { return };
        let child = match recovery {
            JobRecovery::Subagent { child_session_id, .. } => Some(child_session_id.as_str()),
            _ => None,
        };
        let recovery_json = serde_json::to_string(recovery).unwrap_or_default();
        let now = now() as i64;
        let result = self.db.with(|c| {
            c.execute(
                "INSERT INTO job (notification_id, id, session_id, child_session_id, kind, title, status, output, error, recovery, time_created, time_updated) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12) \
                 ON CONFLICT (notification_id) DO UPDATE SET status = ?7, output = ?8, error = ?9, recovery = ?10, time_updated = ?12",
                rusqlite::params![
                    nid,
                    job.info.id,
                    owner_session(recovery),
                    child,
                    recovery_kind(recovery),
                    recovery_title(job, recovery),
                    job.info.status.as_str(),
                    job.info.output,
                    job.info.error,
                    recovery_json,
                    job.info.started_at as i64,
                    now,
                ],
            )
        });
        if let Err(e) = result {
            tracing::error!(error = %e, notificationID = nid.as_str(), "failed to persist background job");
        }
    }

    fn consume(&self, id: &str, generation: u64) {
        let mut jobs = self.jobs.lock();
        let Some(job) = jobs.get_mut(id) else { return };
        if job.generation != generation || job.info.status == JobStatus::Running || job.consumed {
            return;
        }
        job.consumed = true;
        let entry = jobs.shift_remove(id).unwrap();
        jobs.insert(id.to_string(), entry);
        let completed: Vec<String> = jobs
            .iter()
            .filter(|(_, j)| j.consumed && j.info.notification_id.is_none())
            .map(|(k, _)| k.clone())
            .collect();
        if completed.len() > COMPLETED_LIMIT {
            for k in &completed[..completed.len() - COMPLETED_LIMIT] {
                jobs.shift_remove(k);
            }
        }
    }

    pub fn get(&self, id: &str) -> Option<JobInfo> {
        self.jobs.lock().get(id).map(|j| j.info.clone())
    }

    pub fn start(
        self: &std::sync::Arc<Self>,
        id: String,
        kind: String,
        title: Option<String>,
        recovery: Option<JobRecovery>,
        run: BoxFuture<'static, Result<String, String>>,
    ) -> JobInfo {
        self.start_with(id, kind, title, None, recovery, run)
    }

    /// [`Jobs::start`] with the `sessionID` of the job's metadata (a shell job's session).
    pub fn start_with(
        self: &std::sync::Arc<Self>,
        id: String,
        kind: String,
        title: Option<String>,
        metadata_session: Option<String>,
        recovery: Option<JobRecovery>,
        run: BoxFuture<'static, Result<String, String>>,
    ) -> JobInfo {
        let mut jobs = self.jobs.lock();
        if let Some(existing) = jobs.get(&id).filter(|j| j.info.status == JobStatus::Running) {
            return existing.info.clone();
        }
        let generation = {
            let mut g = self.generation.lock();
            *g += 1;
            *g
        };
        let info = JobInfo {
            id: id.clone(),
            kind,
            title,
            status: JobStatus::Running,
            started_at: now(),
            completed_at: None,
            output: None,
            error: None,
            notification_id: None,
        };
        let cancel = CancellationToken::new();
        jobs.insert(
            id.clone(),
            Active {
                info: info.clone(),
                done: watch::channel(None).0,
                backgrounded: watch::channel(None).0,
                cancel: cancel.clone(),
                generation,
                blocking: HashMap::new(),
                is_bg: false,
                consumed: false,
                recovery,
                metadata_session,
            },
        );
        drop(jobs);
        let this = self.clone();
        // The job's fiber is forked from the caller's and keeps its trace (a tool's job: the drain's).
        tokio::spawn(oc_util::trace::inherit(async move {
            let exit = tokio::select! {
                r = run => Some(r),
                _ = cancel.cancelled() => None,
            };
            this.settle(&id, generation, exit);
        }));
        info
    }

    fn settle(&self, id: &str, generation: u64, exit: Option<Result<String, String>>) {
        let mut jobs = self.jobs.lock();
        let Some(job) = jobs.get_mut(id) else { return };
        if job.generation != generation || job.info.status != JobStatus::Running {
            return;
        }
        job.blocking.clear();
        job.info.completed_at = Some(now());
        match exit {
            Some(Ok(o)) => {
                job.info.status = JobStatus::Completed;
                job.info.output = Some(o);
            }
            Some(Err(e)) => {
                job.info.status = JobStatus::Error;
                job.info.error = Some(e);
            }
            None => job.info.status = JobStatus::Cancelled,
        }
        let snapshot = job.info.clone();
        let _ = job.done.send_replace(Some(snapshot.clone()));
        if job.info.status != JobStatus::Cancelled {
            let job = &jobs[id];
            self.persist(job);
        }
    }

    pub async fn wait(&self, id: &str) -> Option<JobInfo> {
        let (rx, generation, recoverable) = {
            let jobs = self.jobs.lock();
            let job = jobs.get(id)?;
            if job.info.status != JobStatus::Running {
                let info = job.info.clone();
                let (g, r) = (job.generation, job.recovery.is_some());
                drop(jobs);
                if !r {
                    self.consume(id, g);
                }
                return Some(info);
            }
            (job.done.subscribe(), job.generation, job.recovery.is_some())
        };
        let info = await_some(rx).await;
        if !recoverable {
            self.consume(id, generation);
        }
        Some(info)
    }

    pub async fn block(&self, id: &str, session: &str) -> Option<JobBlockResult> {
        let (done, bg, generation) = {
            let mut jobs = self.jobs.lock();
            let job = jobs.get_mut(id)?;
            if job.info.status != JobStatus::Running {
                let (info, g) = (job.info.clone(), job.generation);
                drop(jobs);
                self.consume(id, g);
                return Some(JobBlockResult::Finished(info));
            }
            if job.is_bg {
                return Some(JobBlockResult::Backgrounded(job.info.clone()));
            }
            *job.blocking.entry(session.to_string()).or_default() += 1;
            (job.done.subscribe(), job.backgrounded.subscribe(), job.generation)
        };
        let outcome = tokio::select! {
            info = await_some(done) => { self.consume(id, generation); JobBlockResult::Finished(info) }
            info = await_some(bg) => JobBlockResult::Backgrounded(info),
        };
        let mut jobs = self.jobs.lock();
        if let Some(job) = jobs.get_mut(id) {
            if job.info.status == JobStatus::Running && !job.is_bg {
                if let Some(c) = job.blocking.get_mut(session) {
                    *c -= 1;
                    if *c == 0 {
                        job.blocking.remove(session);
                    }
                }
            }
        }
        Some(outcome)
    }

    fn mark_background(&self, job: &mut Active) {
        job.is_bg = true;
        job.blocking.clear();
        if job.recovery.is_some() && job.info.notification_id.is_none() {
            job.info.notification_id = Some(oc_schema::id::message());
        }
        self.persist(job);
    }

    /// Reuse a persisted notification id for a recovered job (before `background`).
    pub fn set_notification_id(&self, id: &str, notification_id: &str) {
        if let Some(job) = self.jobs.lock().get_mut(id) {
            job.info.notification_id = Some(notification_id.to_string());
        }
    }

    pub fn background(&self, id: &str) -> Option<JobInfo> {
        let mut jobs = self.jobs.lock();
        let job = jobs.get_mut(id)?;
        if job.info.status != JobStatus::Running && job.recovery.is_none() {
            return None;
        }
        if job.is_bg {
            return Some(job.info.clone());
        }
        self.mark_background(job);
        let _ = job.backgrounded.send_replace(Some(job.info.clone()));
        Some(job.info.clone())
    }

    pub fn background_all(&self, session: &str, kind: Option<&str>) -> Vec<JobInfo> {
        let mut out = Vec::new();
        let mut jobs = self.jobs.lock();
        for job in jobs.values_mut() {
            if job.info.status != JobStatus::Running || job.is_bg || kind.is_some_and(|k| job.info.kind != k) || !job.blocking.contains_key(session) {
                continue;
            }
            self.mark_background(job);
            let _ = job.backgrounded.send_replace(Some(job.info.clone()));
            out.push(job.info.clone());
        }
        out
    }

    pub fn cancel(&self, id: &str) -> Option<JobInfo> {
        self.cancel_internal(id, true)
    }

    fn cancel_internal(&self, id: &str, persist: bool) -> Option<JobInfo> {
        let mut jobs = self.jobs.lock();
        let job = jobs.get_mut(id)?;
        let generation = job.generation;
        if job.info.status != JobStatus::Running {
            let info = job.info.clone();
            drop(jobs);
            self.consume(id, generation);
            return Some(info);
        }
        job.blocking.clear();
        job.info.status = JobStatus::Cancelled;
        job.info.completed_at = Some(now());
        job.cancel.cancel();
        let info = job.info.clone();
        let _ = job.done.send_replace(Some(info.clone()));
        if persist {
            self.persist(job);
        }
        drop(jobs);
        self.consume(id, generation);
        Some(info)
    }

    /// `cancelSession` (on session delete): cancel the running jobs that belong to the session (by metadata,
    /// blocking sessions, or recovery owner or child) without persisting them, delete their `job` rows, and
    /// forget every matching job.
    pub fn cancel_session(&self, session: &str) -> Vec<JobInfo> {
        let ids: Vec<String> = self
            .jobs
            .lock()
            .values()
            .filter(|j| j.info.status == JobStatus::Running && matches_session(j, session))
            .map(|j| j.info.id.clone())
            .collect();
        let cancelled = ids.iter().filter_map(|id| self.cancel_internal(id, false)).collect();
        if let Err(e) =
            self.db.with(|c| c.execute("DELETE FROM job WHERE session_id = ?1 OR child_session_id = ?1", [session]))
        {
            tracing::error!(error = %e, sessionID = session, "failed to delete the session's background jobs");
        }
        self.jobs.lock().retain(|_, j| !matches_session(j, session));
        cancelled
    }

    /// `runningBackground`: recovery records of the backgrounded jobs still running.
    pub fn running_background(&self) -> Vec<JobRecovery> {
        self.jobs
            .lock()
            .values()
            .filter(|j| j.is_bg && j.info.status == JobStatus::Running)
            .filter_map(|j| j.recovery.clone())
            .collect()
    }

    /// `pendingBackground`: the persisted records, oldest first (`time_created`, then `notification_id`), as
    /// `{id, notificationID, recovery, status, output?, error?}`.
    pub fn pending_background(&self) -> Vec<Value> {
        type Row = (String, String, String, String, Option<String>, Option<String>);
        let rows: Vec<Row> = self
            .db
            .with(|c| {
                let mut s = c.prepare(
                    "SELECT id, notification_id, recovery, status, output, error FROM job ORDER BY time_created ASC, notification_id ASC",
                )?;
                let r = s.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?.collect();
                r
            })
            .unwrap_or_default();
        rows.into_iter()
            .filter_map(|(id, nid, recovery, status, output, error)| {
                let mut v = json!({
                    "id": id,
                    "notificationID": nid,
                    "recovery": serde_json::from_str::<Value>(&recovery).ok()?,
                    "status": status,
                });
                if let Some(o) = output {
                    v["output"] = json!(o);
                }
                if let Some(e) = error {
                    v["error"] = json!(e);
                }
                Some(v)
            })
            .collect()
    }

    /// `JobList.read` (`Job.backgroundJobs`): the persisted background jobs, oldest first, with the live job's
    /// status, completion time and title when it is the same generation, then the backgrounded live jobs not stored
    /// yet. Rows whose recovery does not decode are skipped.
    pub fn background_jobs(&self) -> Vec<ListedJob> {
        type Row = (String, String, String, Option<String>, String, i64, i64);
        let rows: Vec<Row> = self
            .db
            .with(|c| {
                let mut s = c.prepare(
                    "SELECT id, notification_id, status, title, recovery, time_created, time_updated FROM job \
                     ORDER BY time_created ASC, notification_id ASC",
                )?;
                let r = s
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?
                    .collect();
                r
            })
            .unwrap_or_default();
        let jobs = self.jobs.lock();
        let mut seen = std::collections::HashSet::new();
        let mut items = Vec::new();
        for (id, notification_id, status, title, recovery, created, updated) in rows {
            let Some(recovery) = decode_recovery(&recovery) else { continue };
            seen.insert(notification_id.clone());
            let live = jobs.get(&id).filter(|live| live.info.notification_id.as_deref() == Some(notification_id.as_str()));
            let (status, completed, title) = match live {
                Some(live) => (live.info.status, live.info.completed_at, live.info.title.clone().or(title)),
                None => {
                    let status = parse_status(&status);
                    (status, (status != JobStatus::Running).then_some(updated as u64), title)
                }
            };
            items.push(ListedJob { id, status, title, started: created as u64, completed, recovery });
        }
        for live in jobs.values() {
            let Some(recovery) = live.recovery.clone().filter(|_| live.is_bg) else { continue };
            if live.info.notification_id.as_ref().is_some_and(|nid| seen.contains(nid)) {
                continue;
            }
            items.push(ListedJob {
                id: live.info.id.clone(),
                status: live.info.status,
                title: live.info.title.clone(),
                started: live.info.started_at,
                completed: live.info.completed_at,
                recovery,
            });
        }
        items
    }

    pub fn complete_background(&self, notification_id: &str) {
        if let Err(e) = self.db.with(|c| c.execute("DELETE FROM job WHERE notification_id = ?", [notification_id])) {
            tracing::error!(error = %e, notificationID = notification_id, "failed to complete background job");
        }
        let mut jobs = self.jobs.lock();
        let key = jobs.iter().find(|(_, j)| j.info.notification_id.as_deref() == Some(notification_id)).map(|(k, j)| (k.clone(), j.info.status));
        if let Some((k, status)) = key {
            if status != JobStatus::Running {
                jobs.shift_remove(&k);
            }
        }
    }
}

/// `RecoverySchema` decoding: the session ids must be `Session.ID`s (starting with `ses`).
fn decode_recovery(text: &str) -> Option<JobRecovery> {
    let recovery: JobRecovery = serde_json::from_str(text).ok()?;
    let ids: Vec<&str> = match &recovery {
        JobRecovery::Shell { session_id, .. } | JobRecovery::Image { session_id, .. } => vec![session_id],
        JobRecovery::Subagent { parent_session_id, child_session_id, .. } => vec![parent_session_id, child_session_id],
    };
    ids.iter().all(|id| id.starts_with("ses")).then_some(recovery)
}

/// A background job as `experimental.job.list` reports it (`JobList.Item`, the schema's `Job.Info`).
#[derive(Debug, Clone, PartialEq)]
pub struct ListedJob {
    pub id: String,
    pub status: JobStatus,
    pub title: Option<String>,
    pub started: u64,
    pub completed: Option<u64>,
    pub recovery: JobRecovery,
}

impl ListedJob {
    pub fn kind(&self) -> &'static str {
        recovery_kind(&self.recovery)
    }

    /// The owning session: a subagent's parent.
    pub fn session_id(&self) -> &str {
        owner_session(&self.recovery)
    }
}

/// `SessionBackground.ListFilter`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ListFilter {
    pub session_id: Option<String>,
    pub kind: Option<String>,
    pub status: Option<JobStatus>,
    pub descendants: bool,
}

/// `SessionBackground.list`: the background jobs matching the kind and status, and with a session, the ones it owns
/// or, with `descendants`, the ones any session descending from it owns. `parent` gives a session's parent.
pub fn list_background(jobs: Vec<ListedJob>, filter: &ListFilter, parent: impl Fn(&str) -> Option<String>) -> Vec<ListedJob> {
    let items = jobs
        .into_iter()
        .filter(|item| filter.kind.as_deref().is_none_or(|kind| item.kind() == kind))
        .filter(|item| filter.status.is_none_or(|status| item.status == status));
    let Some(session) = filter.session_id.as_deref() else { return items.collect() };
    if !filter.descendants {
        return items.filter(|item| item.session_id() == session).collect();
    }
    let mut parents: HashMap<String, Option<String>> = HashMap::new();
    let mut descends = |from: &str| {
        let mut seen = std::collections::HashSet::new();
        let mut current = Some(from.to_string());
        while let Some(id) = current {
            if id == session {
                return true;
            }
            if !seen.insert(id.clone()) {
                return false;
            }
            current = parents.entry(id.clone()).or_insert_with(|| parent(&id)).clone();
        }
        false
    };
    items.filter(|item| descends(item.session_id())).collect()
}

/// `SubagentCompletion.deliver`: admit the `<subagent>` notice in the parent (waking it unless `resume` is false)
/// and complete the background record.
pub async fn deliver_subagent(
    app: &crate::app::App,
    status: JobStatus,
    output: Option<String>,
    error: Option<String>,
    notification_id: Option<String>,
    recovery: &JobRecovery,
    resume: bool,
) {
    let JobRecovery::Subagent { parent_session_id, child_session_id, agent, description } = recovery else { return };
    if status == JobStatus::Running {
        return;
    }
    let text = match status {
        JobStatus::Completed => output.unwrap_or_else(|| NO_TEXT.into()),
        JobStatus::Error => error.unwrap_or_else(|| "Subagent failed".into()),
        _ => "Subagent cancelled".into(),
    };
    let state = status.as_str();
    let mut meta = serde_json::Map::new();
    meta.insert("source".into(), "subagent".into());
    meta.insert("childID".into(), child_session_id.clone().into());
    meta.insert("agent".into(), agent.clone().into());
    meta.insert("state".into(), state.into());
    let admitted = app
        .sessions
        .synthetic(
            parent_session_id,
            notification_id.clone(),
            format!("<subagent sessionID=\"{child_session_id}\" state=\"{state}\" description=\"{description}\">\n{text}\n</subagent>"),
            Some(description.clone()),
            Some(meta),
            None,
        )
        .await;
    if admitted.is_ok() && resume {
        if let Some(c) = app.coordinator.get() {
            c.wake(parent_session_id, crate::inbox::Scope::Input);
        }
    }
    if let Some(n) = &notification_id {
        app.jobs.complete_background(n);
    }
}

pub fn parse_status(s: &str) -> JobStatus {
    match s {
        "running" => JobStatus::Running,
        "completed" => JobStatus::Completed,
        "error" => JobStatus::Error,
        _ => JobStatus::Cancelled,
    }
}

/// `SubagentCompletion.text`
pub fn subagent_text(message: Option<&oc_schema::Message>) -> String {
    let Some(oc_schema::Message::Assistant(a)) = message else { return NO_TEXT.into() };
    let t = a
        .content
        .iter()
        .filter_map(|c| match c {
            oc_schema::message::AssistantContent::Text { text, .. } if !text.is_empty() => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    if t.is_empty() { NO_TEXT.into() } else { t }
}
