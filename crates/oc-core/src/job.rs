//! Background/foreground jobs (`core/src/job.ts`) and subagent jobs
//! (`core/session/subagent-job.ts`, `subagent-completion.ts`).
//!
//! Backgrounded jobs with a recovery descriptor are kept in the SQLite `job` table (`core/src/job/sql.ts`,
//! migration `20261030054400_job`) until their completion notice is admitted, so a restart can settle them.

use std::collections::HashMap;
use std::sync::Arc;

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

type Done = Arc<watch::Sender<Option<JobInfo>>>;

struct Active {
    info: JobInfo,
    done: Done,
    backgrounded: Done,
    cancel: CancellationToken,
    generation: u64,
    blocking: HashMap<String, u32>,
    is_bg: bool,
    /// A subagent generation whose busy period is reporting (`prepareSubagentReport`): its outbox row belongs to the
    /// settlement commit, and a new start begins a fresh generation.
    settling: bool,
    consumed: bool,
    /// The generation already reported once.
    reported: bool,
    /// Stopped by an explicit caller cancel, not by teardown.
    cancelled_by_caller: bool,
    recovery: Option<JobRecovery>,
    /// `info.metadata.sessionID`, which `matchesSession` checks first.
    metadata_session: Option<String>,
}

/// `Job.start` input (`StartInput`).
pub struct StartInput {
    pub id: String,
    pub kind: String,
    pub title: Option<String>,
    /// The `sessionID` of the job's metadata (a shell job's session).
    pub metadata_session: Option<String>,
    pub recovery: Option<JobRecovery>,
    /// Reuse this notification ID (a recovered job's row).
    pub notification_id: Option<String>,
    /// Start in the background: with a recovery, the row is written before the run starts.
    pub background: bool,
    pub run: BoxFuture<'static, Result<String, String>>,
}

/// `Job.SubagentReportState`: whether the busy period that just settled reports for a backgrounded subagent job.
#[derive(Debug, Clone, PartialEq)]
pub enum SubagentReportState {
    Background { notification_id: String, recovery: JobRecovery, started_at: u64 },
    None,
}

/// `Job.SubagentOutboxReport`: the formatted report a settlement commit writes to the job row.
#[derive(Debug, Clone, PartialEq)]
pub struct SubagentOutboxReport {
    pub notification_id: String,
    pub recovery: JobRecovery,
    pub status: JobStatus,
    pub output: Option<String>,
    pub error: Option<String>,
    pub started_at: Option<u64>,
}

/// `ownerSessionID`: the parent session for a subagent.
fn owner_session(recovery: &JobRecovery) -> &str {
    match recovery {
        JobRecovery::Shell { session_id, .. } | JobRecovery::Image { session_id, .. } | JobRecovery::Codemode { session_id, .. } => {
            session_id
        }
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
        JobRecovery::Codemode { .. } => crate::codemode_job::TITLE,
    }
}

fn recovery_kind(recovery: &JobRecovery) -> &'static str {
    match recovery {
        JobRecovery::Shell { .. } => "shell",
        JobRecovery::Subagent { .. } => "subagent",
        JobRecovery::Image { .. } => "image",
        JobRecovery::Codemode { .. } => "codemode",
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
        Some(
            JobRecovery::Shell { session_id, .. } | JobRecovery::Image { session_id, .. } | JobRecovery::Codemode { session_id, .. },
        ) => session_id == session,
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
        // Order history by first consumption, not by start time or later reads.
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
        self: &Arc<Self>,
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
        self: &Arc<Self>,
        id: String,
        kind: String,
        title: Option<String>,
        metadata_session: Option<String>,
        recovery: Option<JobRecovery>,
        run: BoxFuture<'static, Result<String, String>>,
    ) -> JobInfo {
        self.start_input(StartInput { id, kind, title, metadata_session, recovery, notification_id: None, background: false, run })
    }

    /// `Job.start`: a running job with this ID is joined (unless its generation is settling a report); otherwise a
    /// new generation starts. Background work with a recovery is written to the `job` table before its run starts.
    pub fn start_input(self: &Arc<Self>, input: StartInput) -> JobInfo {
        let StartInput { id, kind, title, metadata_session, recovery, notification_id, background, run } = input;
        let mut jobs = self.jobs.lock();
        if let Some(existing) = jobs.get(&id).filter(|j| j.info.status == JobStatus::Running && !j.settling) {
            return existing.info.clone();
        }
        let generation = {
            let mut g = self.generation.lock();
            *g += 1;
            *g
        };
        let notification_id = notification_id.or_else(|| (background && recovery.is_some()).then(oc_schema::id::message));
        let info = JobInfo {
            id: id.clone(),
            kind,
            title,
            status: JobStatus::Running,
            started_at: now(),
            completed_at: None,
            output: None,
            error: None,
            notification_id,
        };
        let cancel = CancellationToken::new();
        let done: Done = Arc::new(watch::channel(None).0);
        let job = Active {
            info: info.clone(),
            done: done.clone(),
            backgrounded: Arc::new(watch::channel(None).0),
            cancel: cancel.clone(),
            generation,
            blocking: HashMap::new(),
            is_bg: background,
            settling: false,
            consumed: false,
            reported: false,
            cancelled_by_caller: false,
            recovery,
            metadata_session,
        };
        // Background work becomes durable before its run starts, so no drain can observe an unmarked period.
        if job.is_bg {
            self.persist(&job);
        }
        jobs.insert(id.clone(), job);
        drop(jobs);
        let this = self.clone();
        let base = info.clone();
        // The job's fiber is forked from the caller's and keeps its trace (a tool's job: the drain's).
        tokio::spawn(oc_util::trace::inherit(async move {
            let exit = tokio::select! {
                r = run => Some(r),
                _ = cancel.cancelled() => None,
            };
            this.settle(&id, generation, &done, base, exit);
        }));
        info
    }

    fn settle(&self, id: &str, generation: u64, done: &Done, base: JobInfo, exit: Option<Result<String, String>>) {
        let completed_at = now();
        let terminal = |info: &mut JobInfo| {
            info.completed_at = Some(completed_at);
            match &exit {
                Some(Ok(o)) => {
                    info.status = JobStatus::Completed;
                    info.output = Some(o.clone());
                }
                Some(Err(e)) => {
                    info.status = JobStatus::Error;
                    info.error = Some(e.clone());
                }
                None => info.status = JobStatus::Cancelled,
            }
        };
        let mut jobs = self.jobs.lock();
        let Some(job) = jobs.get_mut(id).filter(|j| j.generation == generation) else {
            // A newer generation replaced this one. Its waiters still need their result.
            drop(jobs);
            if done.borrow().is_none() {
                let mut info = base;
                terminal(&mut info);
                let _ = done.send_replace(Some(info));
            }
            return;
        };
        if job.info.status != JobStatus::Running {
            return;
        }
        job.blocking.clear();
        terminal(&mut job.info);
        let snapshot = job.info.clone();
        if job.consumed {
            // A consumed job's notification row is gone; persisting here would resurrect it.
            jobs.shift_remove(id);
        } else if snapshot.status != JobStatus::Cancelled && !job.settling {
            // A settling generation's row belongs to the settlement commit.
            let job = &jobs[id];
            self.persist(job);
        }
        drop(jobs);
        let _ = done.send_replace(Some(snapshot));
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
        self.block_under(id, session, None).await
    }

    /// [`Jobs::block`] from work running inside the job `parent` (`Job.BlockingParent`, 2.3.1 #64343): a
    /// session-wide background request releases the outer blocker, while nested waits keep their own deadlines, so
    /// only a wait on the parent itself counts the session as blocking.
    pub async fn block_under(&self, id: &str, session: &str, parent: Option<&str>) -> Option<JobBlockResult> {
        let tracks_session = parent.is_none_or(|parent| parent == id);
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
            if tracks_session {
                *job.blocking.entry(session.to_string()).or_default() += 1;
            }
            (job.done.subscribe(), job.backgrounded.subscribe(), job.generation)
        };
        let outcome = tokio::select! {
            info = await_some(done) => { self.consume(id, generation); JobBlockResult::Finished(info) }
            info = await_some(bg) => JobBlockResult::Backgrounded(info),
        };
        let mut jobs = self.jobs.lock();
        if let Some(job) = jobs.get_mut(id).filter(|_| tracks_session) {
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
        job.cancelled_by_caller = persist;
        job.info.status = JobStatus::Cancelled;
        job.info.completed_at = Some(now());
        let info = job.info.clone();
        // Settling work owns its outbox row: the settlement commit writes or deletes it.
        if persist && !job.settling {
            self.persist(job);
        }
        let (done, cancel) = (job.done.clone(), job.cancel.clone());
        drop(jobs);
        let _ = done.send_replace(Some(info.clone()));
        cancel.cancel();
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

    /// `runningBackground`: recovery records of the backgrounded jobs still running (not settling a report or
    /// already acknowledged).
    pub fn running_background(&self) -> Vec<JobRecovery> {
        self.jobs
            .lock()
            .values()
            .filter(|j| j.is_bg && j.info.status == JobStatus::Running && !j.settling && !j.consumed)
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
            if live.consumed || live.settling {
                continue;
            }
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

    /// `completeBackground`: delete the row once its notice is admitted. Still-running work keeps its entry, but must
    /// not write its row again when it settles.
    pub fn complete_background(&self, notification_id: &str) {
        if let Err(e) = self.db.with(|c| c.execute("DELETE FROM job WHERE notification_id = ?", [notification_id])) {
            tracing::error!(error = %e, notificationID = notification_id, "failed to complete background job");
        }
        let mut jobs = self.jobs.lock();
        let key = jobs.iter().find(|(_, j)| j.info.notification_id.as_deref() == Some(notification_id)).map(|(k, _)| k.clone());
        if let Some(k) = key {
            if jobs[&k].info.status == JobStatus::Running {
                jobs[&k].consumed = true;
            } else {
                jobs.shift_remove(&k);
            }
        }
    }

    /// `currentGeneration`: the job's generation, and whether it is running and not settling a report.
    pub fn current_generation(&self, id: &str) -> Option<(u64, bool)> {
        let jobs = self.jobs.lock();
        let job = jobs.get(id)?;
        Some((job.generation, job.info.status == JobStatus::Running && !job.settling))
    }

    /// `isCancelled`: whether an explicit caller cancellation stopped this job (not teardown).
    pub fn is_cancelled(&self, id: &str) -> bool {
        self.jobs.lock().get(id).is_some_and(|j| j.cancelled_by_caller)
    }

    /// [`Jobs::is_cancelled`] for one generation.
    pub fn is_cancelled_generation(&self, id: &str, generation: u64) -> bool {
        self.jobs.lock().get(id).is_some_and(|j| j.generation == generation && j.cancelled_by_caller)
    }

    /// `prepareSubagentReport`: claim the report of a backgrounded subagent job for the busy period that just settled.
    /// A generation reports at most once. A running generation starts settling; a cancelled one still belongs to this
    /// period when the bound generation matches (or none was bound, for a cancel in the creation tick).
    pub fn prepare_subagent_report(&self, id: &str, bound_generation: Option<u64>) -> SubagentReportState {
        let mut jobs = self.jobs.lock();
        let Some(job) = jobs.get_mut(id) else { return SubagentReportState::None };
        let (Some(notification_id), Some(recovery @ JobRecovery::Subagent { .. })) = (&job.info.notification_id, &job.recovery) else {
            return SubagentReportState::None;
        };
        if !job.is_bg || job.reported {
            return SubagentReportState::None;
        }
        let live = job.info.status == JobStatus::Running && !job.settling;
        let cancelled = job.info.status == JobStatus::Cancelled && bound_generation.is_none_or(|g| g == job.generation);
        if !live && !cancelled {
            return SubagentReportState::None;
        }
        let state = SubagentReportState::Background {
            notification_id: notification_id.clone(),
            recovery: recovery.clone(),
            started_at: job.info.started_at,
        };
        job.reported = true;
        if job.info.status == JobStatus::Running {
            job.settling = true;
        }
        state
    }

    /// `recordSubagentReport`: upsert the formatted report on its notification ID. Runs inside the terminal event's
    /// commit, so it is pure SQL on that connection and never touches the live registry.
    pub fn record_subagent_report(c: &rusqlite::Connection, report: &SubagentOutboxReport) -> rusqlite::Result<()> {
        let JobRecovery::Subagent { parent_session_id, child_session_id, description, .. } = &report.recovery else { return Ok(()) };
        let now = now() as i64;
        let recovery = serde_json::to_string(&report.recovery).unwrap_or_default();
        c.execute(
            "INSERT INTO job (notification_id, id, session_id, child_session_id, kind, title, status, output, error, recovery, time_created, time_updated) \
             VALUES (?1, ?2, ?3, ?2, 'subagent', ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
             ON CONFLICT (notification_id) DO UPDATE SET status = ?5, output = ?6, error = ?7, recovery = ?8, time_updated = ?10",
            rusqlite::params![
                report.notification_id,
                child_session_id,
                parent_session_id,
                description,
                report.status.as_str(),
                report.output,
                report.error,
                recovery,
                report.started_at.map(|t| t as i64).unwrap_or(now),
                now,
            ],
        )?;
        Ok(())
    }

    /// `deleteBackgroundRow`: pure SQL, for use inside a commit.
    pub fn delete_background_row(c: &rusqlite::Connection, notification_id: &str) -> rusqlite::Result<()> {
        c.execute("DELETE FROM job WHERE notification_id = ?", [notification_id])?;
        Ok(())
    }
}

/// `RecoverySchema` decoding: the session ids must be `Session.ID`s (starting with `ses`).
fn decode_recovery(text: &str) -> Option<JobRecovery> {
    let recovery: JobRecovery = serde_json::from_str(text).ok()?;
    let ids: Vec<&str> = match &recovery {
        JobRecovery::Shell { session_id, .. } | JobRecovery::Image { session_id, .. } | JobRecovery::Codemode { session_id, .. } => {
            vec![session_id]
        }
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

const STILL_RUNNING: &str = "\n\nBackground work still running:\n";

fn clean(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `SubagentCompletion.formatBody`: the report body, then the background work still running for the child or its
/// descendants (`SessionBackground.outstanding` items), each from another session marked with it.
pub fn format_body(body: &str, child_session_id: &str, background: &[Value]) -> String {
    if background.is_empty() {
        return body.to_string();
    }
    let field = |item: &Value, key: &str| item.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
    let lines: Vec<String> = background
        .iter()
        .map(|item| {
            let session = field(item, "sessionID");
            let elsewhere = if session != child_session_id { format!(", in {session}") } else { String::new() };
            match item.get("kind").and_then(Value::as_str) {
                Some("codemode") => format!("- Code Mode script ({}{elsewhere})", field(item, "jobID")),
                Some("shell") => format!("- shell ({}{elsewhere}): {}", field(item, "shellID"), clean(&field(item, "command"))),
                Some("image") => format!("- image ({}{elsewhere}): {}", field(item, "model"), field(item, "path")),
                _ => format!(
                    "- subagent ({}, sessionID: {}): {}",
                    field(item, "agent"),
                    field(item, "childSessionID"),
                    clean(&field(item, "description"))
                ),
            }
        })
        .collect();
    format!("{body}{STILL_RUNNING}{}", lines.join("\n"))
}

/// The `<subagent>` notice `SubagentCompletion.deliver` admits in the parent.
#[derive(Debug, Clone, PartialEq)]
pub struct SubagentNotice {
    pub id: Option<String>,
    pub session_id: String,
    pub text: String,
    pub description: String,
    pub metadata: serde_json::Map<String, Value>,
}

/// `SubagentCompletion.deliver`'s notice for a finished period (`None` while running). The body is formatted from the
/// recovery's `outstanding` snapshot, and the model-facing tag says `running` for a completed report with work still
/// outstanding; the metadata keeps the real status. Older rows lack the snapshot and replay their recorded status
/// with their full body (2.3.1, #64026).
pub fn subagent_notice(
    status: JobStatus,
    output: Option<String>,
    error: Option<String>,
    notification_id: Option<String>,
    recovery: &JobRecovery,
) -> Option<SubagentNotice> {
    let JobRecovery::Subagent { parent_session_id, child_session_id, agent, description, outstanding } = recovery else { return None };
    if status == JobStatus::Running {
        return None;
    }
    let body = match status {
        JobStatus::Completed => output.unwrap_or_else(|| NO_TEXT.into()),
        JobStatus::Error => error.unwrap_or_else(|| "Subagent failed".into()),
        _ => output.or(error).unwrap_or_else(|| "Subagent cancelled".into()),
    };
    let outstanding = outstanding.as_deref().unwrap_or_default();
    let state = if status == JobStatus::Completed && !outstanding.is_empty() { "running" } else { status.as_str() };
    let body = format_body(&body, child_session_id, outstanding);
    let mut metadata = serde_json::Map::new();
    metadata.insert("source".into(), "subagent".into());
    metadata.insert("childID".into(), child_session_id.clone().into());
    metadata.insert("agent".into(), agent.clone().into());
    metadata.insert("state".into(), status.as_str().into());
    Some(SubagentNotice {
        id: notification_id,
        session_id: parent_session_id.clone(),
        text: format!("<subagent sessionID=\"{child_session_id}\" state=\"{state}\" description=\"{description}\">\n{body}\n</subagent>"),
        description: description.clone(),
        metadata,
    })
}

/// `SubagentCompletion.deliver`: admit the notice in the parent (without waking it) and acknowledge the row.
pub async fn deliver_subagent(
    app: &crate::app::App,
    status: JobStatus,
    output: Option<String>,
    error: Option<String>,
    notification_id: Option<String>,
    recovery: &JobRecovery,
) -> Result<(), crate::session::SessionError> {
    let Some(notice) = subagent_notice(status, output, error, notification_id.clone(), recovery) else { return Ok(()) };
    app.sessions.synthetic(&notice.session_id, notice.id, notice.text, Some(notice.description), Some(notice.metadata), None).await?;
    if let Some(n) = &notification_id {
        app.jobs.complete_background(n);
    }
    Ok(())
}

/// `SessionBackground.outstanding`: background jobs still running for the session or any of its descendants, as
/// `{kind, sessionID, ...}` items. `active` adds descendants holding an execution right now (such as a subagent
/// mid-turn whose job already settled), unless a running background job already lists them or their job was
/// cancelled.
pub fn outstanding(app: &crate::app::App, session_id: &str, active: &[String]) -> Vec<Value> {
    let mut parents: HashMap<String, Option<String>> = HashMap::new();
    let mut descends = |from: &str| {
        let mut seen = std::collections::HashSet::new();
        let mut current = Some(from.to_string());
        while let Some(id) = current {
            if id == session_id {
                return true;
            }
            if !seen.insert(id.clone()) {
                return false;
            }
            current = parents
                .entry(id.clone())
                .or_insert_with(|| app.db.session_get(&id).ok().flatten().and_then(|s| s.parent_id))
                .clone();
        }
        false
    };
    let mut items: Vec<Value> = app
        .jobs
        .running_background()
        .into_iter()
        .filter(|r| descends(owner_session(r)))
        .map(|r| match r {
            JobRecovery::Codemode { session_id, job_id } => {
                json!({ "kind": "codemode", "sessionID": session_id, "jobID": job_id })
            }
            JobRecovery::Shell { session_id, shell_id, command } => {
                json!({ "kind": "shell", "sessionID": session_id, "shellID": shell_id, "command": command })
            }
            JobRecovery::Image { session_id, path, model } => {
                json!({ "kind": "image", "sessionID": session_id, "path": path, "model": model })
            }
            JobRecovery::Subagent { parent_session_id, child_session_id, agent, description, .. } => json!({
                "kind": "subagent",
                "sessionID": parent_session_id,
                "childSessionID": child_session_id,
                "agent": agent,
                "description": description,
            }),
        })
        .collect();
    if active.is_empty() {
        return items;
    }
    let children: std::collections::HashSet<String> = items
        .iter()
        .filter(|i| i["kind"] == "subagent")
        .filter_map(|i| i["childSessionID"].as_str().map(str::to_string))
        .collect();
    for id in active {
        if id == session_id || children.contains(id) {
            continue;
        }
        // A cancelled child may still be draining while it cleans up.
        if app.jobs.get(id).is_some_and(|j| j.status == JobStatus::Cancelled) {
            continue;
        }
        if !descends(id) {
            continue;
        }
        let Some(info) = app.db.session_get(id).ok().flatten() else { continue };
        let Some(parent) = info.parent_id else { continue };
        items.push(json!({
            "kind": "subagent",
            "sessionID": parent,
            "childSessionID": id,
            "agent": info.agent.unwrap_or_else(|| "subagent".into()),
            "description": info.title.unwrap_or_else(|| "Subagent".into()),
        }));
    }
    items
}

/// `SubagentJob.start`: a background job named after the child whose run is the child's execution
/// (`execution.resume`). The busy period's settled hook owns the report. Only an explicit caller cancel interrupts
/// the child; anything else leaves it resumable.
pub fn start_subagent(app: &Arc<crate::app::App>, recovery: JobRecovery, notification_id: Option<String>) -> JobInfo {
    let JobRecovery::Subagent { child_session_id, description, .. } = &recovery else {
        unreachable!("start_subagent needs a subagent recovery")
    };
    let id = child_session_id.clone();
    let title = description.clone();
    let generation_of = std::sync::Arc::new(std::sync::OnceLock::<u64>::new());
    struct Interrupt {
        app: Arc<crate::app::App>,
        id: String,
        generation: std::sync::Arc<std::sync::OnceLock<u64>>,
        finished: bool,
    }
    impl Drop for Interrupt {
        fn drop(&mut self) {
            if self.finished {
                return;
            }
            let Some(generation) = self.generation.get() else { return };
            if !self.app.jobs.is_cancelled_generation(&self.id, *generation) {
                return;
            }
            if let (Some(c), Ok(handle)) = (self.app.coordinator.get().cloned(), tokio::runtime::Handle::try_current()) {
                let id = self.id.clone();
                handle.spawn(async move {
                    c.interrupt(&id, Some(crate::coordinator::Reason::User), false).await;
                });
            }
        }
    }
    let guard = Interrupt { app: app.clone(), id: id.clone(), generation: generation_of.clone(), finished: false };
    let run_app = app.clone();
    let child = id.clone();
    let run = Box::pin(async move {
        let mut guard = guard;
        let Some(c) = run_app.coordinator.get().cloned() else {
            guard.finished = true;
            return Ok(String::new());
        };
        let exit = c.run(&child).await;
        match exit {
            crate::coordinator::Exit::Success => {
                guard.finished = true;
                Ok(String::new())
            }
            crate::coordinator::Exit::Failure(e) => {
                guard.finished = true;
                Err(e.to_string())
            }
            // The settled hook already cancelled this job, or the server is stopping: either way nothing settles here.
            crate::coordinator::Exit::Interrupted => {
                guard.finished = true;
                futures::future::pending().await
            }
        }
    });
    let info = app.jobs.start_input(StartInput {
        id: id.clone(),
        kind: "subagent".into(),
        title: Some(title),
        metadata_session: None,
        recovery: Some(recovery),
        notification_id,
        background: true,
        run,
    });
    if let Some((generation, _)) = app.jobs.current_generation(&id) {
        let _ = generation_of.set(generation);
    }
    info
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

#[cfg(test)]
mod completion_tests {
    use super::*;

    fn recovery() -> JobRecovery {
        JobRecovery::Subagent {
            parent_session_id: "ses_deliver_parent".into(),
            child_session_id: "ses_deliver_child".into(),
            agent: "explore".into(),
            description: "Deliver report".into(),
            outstanding: None,
        }
    }

    /// subagent-completion.test.ts "leaves the report body unchanged when no background work runs" and "formats
    /// shell, image, and subagent items with descendant session context" (2.3.0).
    #[test]
    fn format_body_lists_running_work() {
        assert_eq!(format_body("Report body", "ses_child", &[]), "Report body");
        let items = [
            json!({"kind": "shell", "sessionID": "ses_child", "shellID": "shell_1", "command": "bun test\n  --watch"}),
            json!({"kind": "shell", "sessionID": "ses_grandchild", "shellID": "shell_2", "command": "  echo   hi  "}),
            json!({"kind": "image", "sessionID": "ses_child", "path": "/tmp/chart.png", "model": "openai/gpt-image-1"}),
            json!({"kind": "image", "sessionID": "ses_other", "path": "/tmp/other.png", "model": "acme/image"}),
            json!({"kind": "subagent", "sessionID": "ses_child", "childSessionID": "ses_grandchild", "agent": "explore", "description": "Audit\n routes"}),
            json!({"kind": "codemode", "sessionID": "ses_child", "jobID": "cm_1"}),
            json!({"kind": "codemode", "sessionID": "ses_grandchild", "jobID": "cm_2"}),
        ];
        assert_eq!(
            format_body("Report body", "ses_child", &items),
            [
                "Report body",
                "",
                "Background work still running:",
                "- shell (shell_1): bun test --watch",
                "- shell (shell_2, in ses_grandchild): echo hi",
                "- image (openai/gpt-image-1): /tmp/chart.png",
                "- image (acme/image, in ses_other): /tmp/other.png",
                "- subagent (explore, sessionID: ses_grandchild): Audit routes",
                "- Code Mode script (cm_1)",
                "- Code Mode script (cm_2, in ses_grandchild)",
            ]
            .join("\n")
        );
    }

    /// "labels a completed report with pending background work as running, only in the model text" and "preserves a
    /// pre-formatted cancelled body so restart replay keeps running-work context".
    #[test]
    fn notice_state_and_body() {
        let r = recovery();
        let text = |status, output: Option<&str>, error: Option<&str>| {
            subagent_notice(status, output.map(str::to_string), error.map(str::to_string), None, &r).map(|n| n.text)
        };
        // 2.3.1 (#64026): the state and the still-running list come from the outstanding-work snapshot.
        let JobRecovery::Subagent { parent_session_id, child_session_id, agent, description, .. } = r.clone() else { unreachable!() };
        let snapshot = JobRecovery::Subagent {
            outstanding: Some(vec![json!({"kind": "shell", "sessionID": child_session_id, "shellID": "shell_1", "command": "bun test"})]),
            parent_session_id,
            child_session_id,
            agent,
            description,
        };
        let running = subagent_notice(JobStatus::Completed, Some("All green".into()), None, Some("msg_n".into()), &snapshot).unwrap();
        assert_eq!(
            running.text,
            "<subagent sessionID=\"ses_deliver_child\" state=\"running\" description=\"Deliver report\">\nAll green\n\nBackground work still running:\n- shell (shell_1): bun test\n</subagent>"
        );
        assert_eq!(running.metadata["state"], "completed");
        assert_eq!((running.id.as_deref(), running.session_id.as_str()), (Some("msg_n"), "ses_deliver_parent"));
        assert_eq!(text(JobStatus::Error, None, Some("boom")).unwrap(), "<subagent sessionID=\"ses_deliver_child\" state=\"error\" description=\"Deliver report\">\nboom\n</subagent>");
        assert_eq!(
            text(JobStatus::Cancelled, None, None).unwrap(),
            "<subagent sessionID=\"ses_deliver_child\" state=\"cancelled\" description=\"Deliver report\">\nSubagent cancelled\n</subagent>"
        );
        // A running job has nothing to report.
        assert!(text(JobStatus::Running, None, None).is_none());
        assert_eq!(text(JobStatus::Completed, Some("All done"), None).unwrap(), "<subagent sessionID=\"ses_deliver_child\" state=\"completed\" description=\"Deliver report\">\nAll done\n</subagent>");
        // An older row without the snapshot replays its recorded status and keeps its full body.
        let old = "All green\n\nBackground work still running:\n- shell (shell_1): bun test";
        assert_eq!(
            text(JobStatus::Completed, Some(old), None).unwrap(),
            format!("<subagent sessionID=\"ses_deliver_child\" state=\"completed\" description=\"Deliver report\">\n{old}\n</subagent>")
        );
        let output = "Subagent cancelled\n\nBackground work still running:\n- shell (shell_1): bun test";
        assert_eq!(
            text(JobStatus::Cancelled, Some(output), None).unwrap(),
            format!("<subagent sessionID=\"ses_deliver_child\" state=\"cancelled\" description=\"Deliver report\">\n{output}\n</subagent>")
        );
        assert_eq!(
            text(JobStatus::Cancelled, None, Some("Cancel failed")).unwrap(),
            "<subagent sessionID=\"ses_deliver_child\" state=\"cancelled\" description=\"Deliver report\">\nCancel failed\n</subagent>"
        );
    }
}
