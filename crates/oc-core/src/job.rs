//! Background/foreground jobs (`core/src/job.ts`) and subagent jobs
//! (`core/session/subagent-job.ts`, `subagent-completion.ts`).

use std::collections::HashMap;

use futures::future::BoxFuture;
use indexmap::IndexMap;
use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use oc_storage::Database;
use oc_tools::host::{JobBlockResult, JobInfo, JobRecovery, JobStatus};

const BACKGROUND_PREFIX: &str = "job.background/";
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

    fn persist(&self, job: &Active) {
        let (Some(recovery), Some(nid)) = (&job.recovery, &job.info.notification_id) else { return };
        let mut v = json!({
            "id": job.info.id,
            "notificationID": nid,
            "recovery": recovery,
            "status": job.info.status.as_str(),
        });
        if let Some(o) = &job.info.output {
            v["output"] = json!(o);
        }
        if let Some(e) = &job.info.error {
            v["error"] = json!(e);
        }
        let _ = self.db.kv_set(&format!("{BACKGROUND_PREFIX}{nid}"), &v);
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
            },
        );
        drop(jobs);
        let this = self.clone();
        tokio::spawn(async move {
            let exit = tokio::select! {
                r = run => Some(r),
                _ = cancel.cancelled() => None,
            };
            this.settle(&id, generation, exit);
        });
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
        self.persist(job);
        drop(jobs);
        self.consume(id, generation);
        Some(info)
    }

    /// Cancel every job owned by a session (`jobs.cancel(sessionID)` on interrupt).
    pub fn cancel_session(&self, session: &str) {
        let ids: Vec<String> = self
            .jobs
            .lock()
            .iter()
            .filter(|(_, j)| match &j.recovery {
                Some(JobRecovery::Shell { session_id, .. }) => session_id == session,
                Some(JobRecovery::Subagent { parent_session_id, .. }) => parent_session_id == session,
                None => false,
            })
            .map(|(k, _)| k.clone())
            .collect();
        for id in ids {
            self.cancel(&id);
        }
    }

    pub fn pending_background(&self) -> Vec<Value> {
        let mut out = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let Ok((entries, next)) = self.db.kv_scan(BACKGROUND_PREFIX, after.as_deref(), None) else { break };
            out.extend(entries.into_iter().map(|(_, v)| v));
            match next {
                Some(n) => after = Some(n),
                None => break,
            }
        }
        out
    }

    pub fn complete_background(&self, notification_id: &str) {
        let _ = self.db.kv_remove(&format!("{BACKGROUND_PREFIX}{notification_id}"));
        let mut jobs = self.jobs.lock();
        let key = jobs.iter().find(|(_, j)| j.info.notification_id.as_deref() == Some(notification_id)).map(|(k, j)| (k.clone(), j.info.status));
        if let Some((k, status)) = key {
            if status != JobStatus::Running {
                jobs.shift_remove(&k);
            }
        }
    }
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
