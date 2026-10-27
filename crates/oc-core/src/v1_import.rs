//! V1 → V2 session history import (`core/src/database/v1-migration.bun.ts`, spec 02 §2.8).
//!
//! When the database still has the v1 `session` table, the server imports v1 sessions, messages and parts into
//! `session_v2` / `session_message` / `event_sequence` in the background (one transaction per session, resumable
//! through the KV cursor `migration.v1-v2`). Nothing is gated while it runs; the status is exposed through
//! `GET /api/experimental/migration/v1` ([`Importer::status`]).
//!
//! Output is byte-compatible with upstream: message `data` is serialized like `JSON.stringify` (JS number
//! formatting, integer-like keys first), IDs are reused from v1 and synthetic ones derived with the same SHA-256
//! scheme, and V1 rows are validated against the same `SessionV1.Info` / `SessionV1.Part` schemas.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use anyhow::{anyhow, Context as _};
use base64::Engine as _;
use oc_storage::Database;
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

pub const MIGRATION_STATE_KEY: &str = "migration.v1-v2";
const EVENT_DELETE_BATCH_SIZE: i64 = 1_000;
const GLOBAL_PROJECT: &str = "global";

// ---------------------------------------------------------------------------------------------------------------
// Status / runtime state
// ---------------------------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct Progress {
    pub label: String,
    pub numerator: Option<u64>,
    pub denominator: Option<u64>,
}

impl Progress {
    fn label(label: &str) -> Self {
        Progress { label: label.into(), numerator: None, denominator: None }
    }
    fn migrating(numerator: u64, denominator: u64) -> Self {
        Progress { label: "Migrating sessions".into(), numerator: Some(numerator), denominator: Some(denominator) }
    }
}

/// `V1Migration.Status`.
#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Required,
    Completed,
    Running(Progress),
    Error(String),
}

impl Status {
    /// Wire shape of `experimental.migration.v1.status`.
    pub fn to_json(&self) -> Value {
        match self {
            Status::Required => json!({"status": "required"}),
            Status::Completed => json!({"status": "completed"}),
            Status::Running(p) => {
                let mut progress = Map::new();
                progress.insert("label".into(), Value::from(p.label.clone()));
                if let Some(n) = p.numerator {
                    progress.insert("numerator".into(), Value::from(n));
                }
                if let Some(d) = p.denominator {
                    progress.insert("denominator".into(), Value::from(d));
                }
                json!({"status": "running", "progress": progress})
            }
            Status::Error(e) => json!({"status": "error", "error": e}),
        }
    }
}

#[derive(Clone, Debug)]
enum RuntimeState {
    Idle,
    Running(Progress),
    Error(String),
}

#[derive(Clone, Debug, PartialEq)]
enum MigrationState {
    Sessions { cursor: Option<String> },
    Completed,
}

/// Options for [`Importer::run`].
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// The pre-split V2 database (`<data>/opencode-next.db` by default).
    pub next_database_path: Option<PathBuf>,
}

/// Module-level state of upstream's `v1-migration.bun.ts` (`runtimeState` + the run semaphore).
pub struct Importer {
    runtime: Mutex<RuntimeState>,
    lock: Mutex<()>,
}

impl Default for Importer {
    fn default() -> Self {
        Importer { runtime: Mutex::new(RuntimeState::Idle), lock: Mutex::new(()) }
    }
}

impl Importer {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The process-wide importer (upstream keeps the state at module level).
    pub fn global() -> Arc<Importer> {
        static GLOBAL: OnceLock<Arc<Importer>> = OnceLock::new();
        GLOBAL.get_or_init(Importer::new).clone()
    }

    /// `V1Migration.status()`.
    pub fn status(&self, db: &Database) -> anyhow::Result<Status> {
        if !db.with(has_legacy_sessions)? {
            return Ok(Status::Completed);
        }
        let state = db.with(|c| Ok(read_state(c)))??;
        match &*self.runtime.lock() {
            RuntimeState::Running(p) => return Ok(Status::Running(p.clone())),
            RuntimeState::Error(e) => return Ok(Status::Error(e.clone())),
            RuntimeState::Idle => {}
        }
        if state == Some(MigrationState::Completed) {
            return Ok(Status::Completed);
        }
        Ok(Status::Required)
    }

    /// `V1Migration.layer`: mark the import running and run it on a background thread. Never blocks the caller.
    pub fn start(self: &Arc<Self>, db: Database, data_dir: PathBuf) -> std::thread::JoinHandle<()> {
        *self.runtime.lock() = RuntimeState::Running(Progress::label("Clearing old events"));
        let this = self.clone();
        std::thread::Builder::new()
            .name("v1-import".into())
            .spawn(move || {
                let options = Options { next_database_path: default_next_path(&data_dir) };
                match this.run(&db, &data_dir, &options) {
                    Ok(()) => *this.runtime.lock() = RuntimeState::Idle,
                    Err(e) => {
                        let text = error_text(&e);
                        tracing::error!(cause = %text, "V1 migration failed");
                        *this.runtime.lock() = RuntimeState::Error(text);
                    }
                }
            })
            .expect("spawn v1-import thread")
    }

    fn update_progress(&self, progress: Progress) {
        let mut rt = self.runtime.lock();
        if matches!(*rt, RuntimeState::Running(_)) {
            *rt = RuntimeState::Running(progress);
        }
    }

    fn increment_progress(&self, denominator: u64) {
        let mut rt = self.runtime.lock();
        if let RuntimeState::Running(p) = &*rt {
            let numerator = p.numerator.unwrap_or(0) + 1;
            *rt = RuntimeState::Running(Progress::migrating(numerator, denominator));
        }
    }

    /// `V1Migration.run()`: import everything that is left. `data_dir` is `Global.data`.
    pub fn run(&self, db: &Database, data_dir: &Path, options: &Options) -> anyhow::Result<()> {
        let _permit = self.lock.lock();
        let state = db.with(|c| Ok(read_state(c)))??;
        if state == Some(MigrationState::Completed) {
            return Ok(());
        }
        if !db.with(has_legacy_sessions)? {
            return Ok(());
        }
        let now = oc_storage::db::now_ms();
        let root = path_root(data_dir);
        db.with(|c| {
            c.execute(
                "INSERT OR IGNORE INTO project (id, worktree, time_created, time_updated, time_active, sandboxes)
                 VALUES (?, ?, ?, ?, ?, '[]')",
                params![GLOBAL_PROJECT, root, now, now, now],
            )
        })?;
        if state.is_none() {
            db.transaction(|tx| {
                loop {
                    let deleted = tx.execute(
                        "DELETE FROM event WHERE rowid IN (SELECT rowid FROM event LIMIT ?)",
                        [EVENT_DELETE_BATCH_SIZE],
                    )? as i64;
                    if deleted < EVENT_DELETE_BATCH_SIZE {
                        break;
                    }
                }
                let now = oc_storage::db::now_ms();
                tx.execute(
                    "INSERT INTO kv (key, value, time_created, time_updated) VALUES (?, ?, ?, ?)",
                    params![MIGRATION_STATE_KEY, r#"{"phase":"sessions"}"#, now, now],
                )?;
                Ok(())
            })?;
        }
        let next_path = options.next_database_path.as_deref();
        let source_total = count_next_sessions(next_path)?;
        let legacy_total: u64 = db.with(|c| c.query_row("SELECT COUNT(*) FROM session", [], |r| r.get::<_, i64>(0)))? as u64;
        let cursor = match &state {
            Some(MigrationState::Sessions { cursor }) => cursor.clone(),
            _ => None,
        };
        let migrated: u64 = match &cursor {
            Some(cursor) => db.with(|c| {
                c.query_row("SELECT COUNT(*) FROM session WHERE id >= ?", [cursor], |r| r.get::<_, i64>(0))
            })? as u64,
            None => 0,
        };
        let denominator = source_total + legacy_total;
        self.update_progress(Progress::migrating(migrated, denominator));
        import_next_database(db, next_path, &mut |completed| {
            self.update_progress(Progress::migrating(migrated + completed, denominator));
        })?;
        self.update_progress(Progress::migrating(migrated + source_total, denominator));
        let mut projects: HashMap<String, String> = db.with(|c| {
            let mut s = c.prepare("SELECT id, worktree FROM project")?;
            let rows = s
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .collect::<rusqlite::Result<HashMap<_, _>>>();
            rows
        })?;
        let mut directories: HashMap<String, Option<Owner>> = HashMap::new();
        let mut lookup_timed_out = false;
        loop {
            let state = db.with(|c| Ok(read_state(c)))??;
            let cursor = match state {
                Some(MigrationState::Sessions { cursor }) => cursor,
                _ => None,
            };
            let next: Option<NextSession> = db.with(|c| {
                let sql = format!(
                    "SELECT id, project_id, workspace_id, directory, path, time_updated,
                       EXISTS (SELECT 1 FROM session_v2 WHERE session_v2.id = session.id) AS imported
                     FROM session {} ORDER BY id DESC LIMIT 1",
                    if cursor.is_some() { "WHERE id < ?1" } else { "" }
                );
                let read = |r: &rusqlite::Row| {
                    Ok(NextSession {
                        id: r.get(0)?,
                        project_id: r.get(1)?,
                        workspace_id: r.get(2)?,
                        directory: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                        path: r.get(4)?,
                        time_updated: r.get(5)?,
                        imported: r.get::<_, i64>(6)? != 0,
                    })
                };
                match &cursor {
                    None => c.query_row(&sql, [], read),
                    Some(cursor) => c.query_row(&sql, [cursor], read),
                }
                .optional()
            })?;
            let Some(next) = next else { break };
            let project_id =
                if projects.contains_key(&next.project_id) { next.project_id.clone() } else { GLOBAL_PROJECT.to_string() };
            // Legacy ACP sessions can store a cwd relative to an unknown process directory. Use the project
            // worktree instead, and leave the original V1 row intact.
            let directory = if next.directory.is_empty() || stored_absolute_path(&next.directory).is_some() {
                next.directory.clone()
            } else {
                let worktree = projects.get(&project_id).cloned().unwrap_or_else(|| path_root(data_dir));
                stored_absolute_path(&worktree).unwrap_or_default()
            };
            let unowned =
                project_id == GLOBAL_PROJECT && next.workspace_id.is_none() && !next.imported && directory == next.directory;
            if unowned && !lookup_timed_out && !directories.contains_key(&directory) {
                let resolved = resolve_directory(&directory);
                // One hung directory (e.g. a dead network mount) usually means more; leave the rest under global.
                if resolved.is_none() {
                    lookup_timed_out = true;
                    tracing::warn!(directory = %directory, "Stopped resolving V1 session directories after a timeout");
                }
                directories.insert(directory.clone(), resolved.flatten());
            }
            let owner = if unowned { directories.get(&directory).cloned().flatten() } else { None };
            let target = SessionTarget { directory: &directory, owner: owner.as_ref(), projects: &projects };
            db.transaction(|tx| import_session(tx, &next, &target))?;
            if let Some(owner) = owner
                && !projects.contains_key(&owner.id)
            {
                projects.insert(owner.id.clone(), owner.canonical.clone());
            }
            self.increment_progress(denominator);
            std::thread::yield_now();
        }
        db.transaction(|tx| {
            let now = oc_storage::db::now_ms();
            tx.execute(
                "INSERT INTO kv (key, value, time_created, time_updated) VALUES (?1, ?2, ?3, ?3)
                 ON CONFLICT (key) DO UPDATE SET value = ?2, time_updated = ?3",
                params![MIGRATION_STATE_KEY, r#"{"phase":"completed"}"#, now],
            )?;
            Ok(())
        })?;
        Ok(())
    }
}

/// `errorText`: the message followed by `Caused by:` lines.
fn error_text(e: &anyhow::Error) -> String {
    let mut out = e.to_string();
    for cause in e.chain().skip(1) {
        out.push_str("\nCaused by: ");
        out.push_str(&cause.to_string());
    }
    out
}

/// `nextPath`: `<data>/opencode-next.db` unless the database is in memory.
pub fn default_next_path(data_dir: &Path) -> Option<PathBuf> {
    if std::env::var("OPENCODE_DB").ok().as_deref() == Some(":memory:") {
        return None;
    }
    Some(data_dir.join("opencode-next.db"))
}

/// `path.parse(dir).root`.
fn path_root(dir: &Path) -> String {
    use std::path::Component;
    let mut root = String::new();
    for c in dir.components() {
        match c {
            Component::Prefix(p) => root.push_str(&p.as_os_str().to_string_lossy()),
            Component::RootDir => {
                root.push(std::path::MAIN_SEPARATOR);
                break;
            }
            _ => break,
        }
    }
    root
}

fn has_legacy_sessions(c: &Connection) -> rusqlite::Result<bool> {
    c.query_row("SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'session'", [], |_| Ok(()))
        .optional()
        .map(|r| r.is_some())
}

/// `readState` + `parseState`. A value that is not valid JSON fails like drizzle's JSON column decoding.
fn read_state(c: &Connection) -> anyhow::Result<Option<MigrationState>> {
    let raw: Option<String> =
        c.query_row("SELECT value FROM kv WHERE key = ?", [MIGRATION_STATE_KEY], |r| r.get(0)).optional()?;
    let Some(raw) = raw else { return Ok(None) };
    let value: Value = serde_json::from_str(&raw).context("JSON Parse error")?;
    let Some(obj) = value.as_object() else { return Ok(None) };
    let Some(phase) = obj.get("phase") else { return Ok(None) };
    if phase == "completed" {
        return Ok(Some(MigrationState::Completed));
    }
    if phase != "sessions" {
        return Ok(None);
    }
    match obj.get("cursor") {
        None => Ok(Some(MigrationState::Sessions { cursor: None })),
        Some(Value::String(s)) => Ok(Some(MigrationState::Sessions { cursor: Some(s.clone()) })),
        Some(_) => Ok(None),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// One V1 session
// ---------------------------------------------------------------------------------------------------------------

/// A `session_v2` row as far as [`transform_session`] needs it.
#[derive(Clone, Debug, Default)]
pub struct SessionRow {
    pub id: String,
    pub agent: Option<String>,
    /// The parsed `model` JSON column.
    pub model: Option<Value>,
    pub time_updated: i64,
}

#[derive(Clone, Debug)]
pub struct SourceMessage {
    pub id: String,
    pub session_id: String,
    pub time_created: i64,
    pub time_updated: i64,
    pub data: String,
}

#[derive(Clone, Debug)]
pub struct SourcePart {
    pub id: String,
    pub message_id: String,
    pub session_id: String,
    pub time_created: i64,
    pub time_updated: i64,
    pub data: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MessageRow {
    pub id: String,
    pub session_id: String,
    pub r#type: String,
    pub seq: i64,
    pub time_created: i64,
    pub time_updated: i64,
    pub data: Value,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionUpdate {
    pub agent: Option<String>,
    pub model: Option<Value>,
    pub cost: f64,
    pub tokens_input: f64,
    pub tokens_output: f64,
    pub tokens_reasoning: f64,
    pub tokens_cache_read: f64,
    pub tokens_cache_write: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Warning {
    pub reason: &'static str,
    pub session_id: String,
    pub message_id: Option<String>,
    pub part_id: Option<String>,
    pub observed_type: Option<String>,
}

#[derive(Clone, Debug)]
pub struct TransformResult {
    pub messages: Vec<MessageRow>,
    pub session: SessionUpdate,
    pub watermark: i64,
    pub warnings: Vec<Warning>,
}

const SESSION_JSON_COLUMNS: [&str; 6] = ["fork_boundary", "summary_diffs", "metadata", "revert", "permission", "model"];

/// The next legacy session row to import.
struct NextSession {
    id: String,
    project_id: String,
    workspace_id: Option<String>,
    directory: String,
    path: Option<String>,
    time_updated: i64,
    /// Already stored in `session_v2` (an earlier V2 import kept it).
    imported: bool,
}

/// The V2 project that owns a V1 global session's directory, found without persisting it.
#[derive(Clone, Debug)]
struct Owner {
    id: String,
    canonical: String,
    vcs: Option<&'static str>,
    /// The directory relative to the project's worktree, `/`-separated.
    path: String,
}

struct SessionTarget<'a> {
    /// The session directory (a relative V1 cwd replaced by the project worktree).
    directory: &'a str,
    owner: Option<&'a Owner>,
    projects: &'a HashMap<String, String>,
}

/// `storedAbsolutePath` (`database/path.ts`): absolute POSIX, `C:/` or `//server` paths, which can move
/// between operating systems during migration.
fn stored_absolute_path(input: &str) -> Option<String> {
    let result = if cfg!(windows) { input.replace('\\', "/") } else { input.to_string() };
    let bytes = result.as_bytes();
    let windows = (bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'/')
        || result.starts_with("//");
    (result.starts_with('/') || windows).then_some(result)
}

/// `resolveDirectory`: V1 grouped non-git sessions under global; V2 resolves an owner for each directory.
/// Discovery does not persist or announce the project. Any failure keeps the session under global. The
/// lookup runs detached so a filesystem or git call that never returns cannot stall the migration;
/// `None` means it timed out (5 s).
fn resolve_directory(directory: &str) -> Option<Option<Owner>> {
    let directory = directory.to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let owner = (|| -> Option<Owner> {
            if directory.is_empty() {
                return None;
            }
            let absolute = oc_config::discovery::absolute(Path::new(&directory));
            let normalized = std::fs::canonicalize(absolute).ok()?;
            if !normalized.is_dir() {
                return None;
            }
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().ok()?;
            let project = runtime.block_on(crate::project::resolve(&normalized));
            let path = normalized
                .strip_prefix(&project.directory)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            Some(Owner {
                id: project.id.clone(),
                canonical: project.canonical.display().to_string(),
                vcs: project.vcs.as_ref().map(|v| v.kind()),
                path,
            })
        })();
        let _ = tx.send(owner);
    });
    rx.recv_timeout(std::time::Duration::from_secs(5)).ok()
}

fn import_session(tx: &Connection, next: &NextSession, target: &SessionTarget) -> anyhow::Result<()> {
    let session_id = next.id.as_str();
    let v1_project_id = next.project_id.as_str();
    let now = oc_storage::db::now_ms();
    let state = json!({"phase": "sessions", "cursor": session_id});
    tx.execute(
        "INSERT INTO kv (key, value, time_created, time_updated) VALUES (?1, ?2, ?3, ?3)
         ON CONFLICT (key) DO UPDATE SET value = ?2, time_updated = ?3",
        params![MIGRATION_STATE_KEY, js_stringify(&state), now],
    )?;
    let project_id = if target.projects.contains_key(v1_project_id) { v1_project_id } else { GLOBAL_PROJECT };
    if project_id != v1_project_id {
        tracing::warn!(session_id, project_id = v1_project_id, "Reassigned V1 session with missing project");
    }
    if target.directory != next.directory {
        tracing::warn!(session_id, directory = %next.directory, resolved = %target.directory, "Replaced relative V1 session directory");
    }
    if let Some(owner) = target.owner
        && !target.projects.contains_key(&owner.id)
    {
        tx.execute(
            "INSERT INTO project (id, worktree, vcs, sandboxes, time_created, time_updated, time_active)
             VALUES (?1, ?2, ?3, '[]', ?4, ?4, ?5) ON CONFLICT DO NOTHING",
            params![owner.id, owner.canonical, owner.vcs, now, next.time_updated],
        )?;
    }
    let project_id = target.owner.map(|o| o.id.as_str()).unwrap_or(project_id);
    let path = match target.owner {
        Some(owner) => Some(owner.path.clone()),
        None => next.path.clone(),
    };
    tx.execute(
        "INSERT OR IGNORE INTO session_v2 (
           id, project_id, workspace_id, parent_id, slug, directory, path, title, version, share_url,
           summary_additions, summary_deletions, summary_files, summary_diffs, metadata, cost,
           tokens_input, tokens_output, tokens_reasoning, tokens_cache_read, tokens_cache_write,
           revert, permission, agent, model, time_created, time_updated, time_compacting, time_archived
         )
         SELECT
           id, ?1, workspace_id, parent_id, slug, ?3, ?4, title, version, share_url,
           summary_additions, summary_deletions, summary_files, summary_diffs, metadata, cost,
           tokens_input, tokens_output, tokens_reasoning, tokens_cache_read, tokens_cache_write,
           revert, NULL, agent, model, time_created, time_updated, time_compacting, time_archived
         FROM session
         WHERE id = ?2",
        params![project_id, session_id, target.directory, path],
    )?;
    let next = read_session_row(tx, session_id)?
        .ok_or_else(|| anyhow!("Failed to copy V1 session {session_id}"))?;
    let messages: Vec<SourceMessage> = {
        let mut s =
            tx.prepare("SELECT id, session_id, time_created, time_updated, data FROM message WHERE session_id = ?")?;
        let rows = s
            .query_map([&next.id], |r| {
                Ok(SourceMessage {
                    id: r.get(0)?,
                    session_id: r.get(1)?,
                    time_created: r.get(2)?,
                    time_updated: r.get(3)?,
                    data: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    let parts: Vec<SourcePart> = {
        let mut s = tx.prepare(
            "SELECT id, message_id, session_id, time_created, time_updated, data FROM part WHERE session_id = ?",
        )?;
        let rows = s
            .query_map([&next.id], |r| {
                Ok(SourcePart {
                    id: r.get(0)?,
                    message_id: r.get(1)?,
                    session_id: r.get(2)?,
                    time_created: r.get(3)?,
                    time_updated: r.get(4)?,
                    data: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    let transformed = transform_session(&next, &messages, &parts).map_err(|e| anyhow!(e))?;
    for w in &transformed.warnings {
        tracing::warn!(
            reason = w.reason,
            session_id = %w.session_id,
            message_id = ?w.message_id,
            part_id = ?w.part_id,
            observed_type = ?w.observed_type,
            "Skipped V1 migration row"
        );
    }
    tx.execute("DELETE FROM session_message WHERE session_id = ?", [&next.id])?;
    {
        let mut insert = tx.prepare(
            "INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )?;
        for m in &transformed.messages {
            insert.execute(params![
                m.id,
                m.session_id,
                m.r#type,
                m.seq,
                m.time_created,
                m.time_updated,
                js_stringify(&m.data)
            ])?;
        }
    }
    let s = &transformed.session;
    tx.execute(
        "UPDATE session_v2 SET agent = ?, model = ?, cost = ?, tokens_input = ?, tokens_output = ?,
           tokens_reasoning = ?, tokens_cache_read = ?, tokens_cache_write = ?, revert = NULL,
           time_compacting = NULL, time_updated = ?
         WHERE id = ?",
        params![
            s.agent,
            s.model.as_ref().map(js_stringify),
            s.cost,
            sql_number(s.tokens_input),
            sql_number(s.tokens_output),
            sql_number(s.tokens_reasoning),
            sql_number(s.tokens_cache_read),
            sql_number(s.tokens_cache_write),
            next.time_updated,
            next.id
        ],
    )?;
    tx.execute(
        "INSERT INTO event_sequence (aggregate_id, seq) VALUES (?1, ?2)
         ON CONFLICT (aggregate_id) DO UPDATE SET seq = ?2, owner_id = NULL",
        params![next.id, transformed.watermark],
    )?;
    Ok(())
}

/// How bun:sqlite binds a JS number: safe integers as INTEGER, everything else as REAL.
fn sql_number(n: f64) -> rusqlite::types::Value {
    if n.fract() == 0.0 && n.abs() <= 9_007_199_254_740_991.0 {
        rusqlite::types::Value::Integer(n as i64)
    } else {
        rusqlite::types::Value::Real(n)
    }
}

fn read_session_row(tx: &Connection, id: &str) -> anyhow::Result<Option<SessionRow>> {
    let cols = SESSION_JSON_COLUMNS.join(", ");
    let row = tx
        .query_row(&format!("SELECT id, agent, time_updated, {cols} FROM session_v2 WHERE id = ?"), [id], |r| {
            let mut json_cols = Vec::new();
            for i in 0..SESSION_JSON_COLUMNS.len() {
                json_cols.push(r.get::<_, Option<String>>(3 + i)?);
            }
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, i64>(2)?, json_cols))
        })
        .optional()?;
    let Some((id, agent, time_updated, json_cols)) = row else { return Ok(None) };
    // Drizzle decodes every JSON column of the selected row; a malformed one fails the session.
    let mut model = None;
    for (name, raw) in SESSION_JSON_COLUMNS.iter().zip(json_cols) {
        if let Some(raw) = raw {
            let v: Value = serde_json::from_str(&raw).with_context(|| format!("JSON Parse error in session_v2.{name}"))?;
            if *name == "model" {
                model = Some(v);
            }
        }
    }
    Ok(Some(SessionRow { id, agent, model, time_updated }))
}

// ---------------------------------------------------------------------------------------------------------------
// transformSession
// ---------------------------------------------------------------------------------------------------------------

struct Msg<'a> {
    row: &'a SourceMessage,
    value: Map<String, Value>,
}

impl Msg<'_> {
    fn role(&self) -> &str {
        self.value.get("role").and_then(Value::as_str).unwrap_or("")
    }
}

struct Prt<'a> {
    row: &'a SourcePart,
    value: Map<String, Value>,
}

fn ptype(p: &Map<String, Value>) -> &str {
    p.get("type").and_then(Value::as_str).unwrap_or("")
}

fn get_str<'a>(m: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    m.get(key).and_then(Value::as_str)
}

fn get_bool(m: &Map<String, Value>, key: &str) -> bool {
    m.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn get_obj<'a>(m: &'a Map<String, Value>, key: &str) -> Option<&'a Map<String, Value>> {
    m.get(key).and_then(Value::as_object)
}

fn num_f64(v: Option<&Value>) -> f64 {
    v.and_then(Value::as_f64).unwrap_or(0.0)
}

fn utf16_locale_cmp(a: &str, b: &str) -> Ordering {
    let a: Vec<u16> = a.encode_utf16().collect();
    let b: Vec<u16> = b.encode_utf16().collect();
    let r = oc_codemode::stdlib::string::locale_compare(&a, &b);
    if r < 0.0 {
        Ordering::Less
    } else if r > 0.0 {
        Ordering::Greater
    } else {
        Ordering::Equal
    }
}

/// `transformSession`: V1 messages + parts → V2 `session_message` rows and the recomputed session columns.
/// Errors only where upstream throws (a malformed percent-encoded `data:` URL).
pub fn transform_session(
    session: &SessionRow,
    input_messages: &[SourceMessage],
    input_parts: &[SourcePart],
) -> Result<TransformResult, String> {
    let mut warnings = Vec::new();
    let mut messages: Vec<Msg> = Vec::new();
    for row in input_messages {
        let decoded = match serde_json::from_str::<Value>(&row.data) {
            Ok(Value::Object(mut value)) => {
                value.insert("id".into(), Value::from(row.id.clone()));
                value.insert("sessionID".into(), Value::from(row.session_id.clone()));
                if schema::info(&value) { Some(value) } else { None }
            }
            _ => None,
        };
        match decoded {
            Some(value) => messages.push(Msg { row, value }),
            None => warnings.push(Warning {
                reason: "invalid-message",
                session_id: session.id.clone(),
                message_id: Some(row.id.clone()),
                part_id: None,
                observed_type: None,
            }),
        }
    }
    messages.sort_by(|a, b| a.row.time_created.cmp(&b.row.time_created).then_with(|| utf16_locale_cmp(&a.row.id, &b.row.id)));
    let message_ids: HashSet<&str> = input_messages.iter().map(|m| m.id.as_str()).collect();
    let mut parts: Vec<Prt> = Vec::new();
    for row in input_parts {
        let parsed = serde_json::from_str::<Value>(&row.data).ok();
        let observed_type = match &parsed {
            Some(Value::Object(o)) => o.get("type").map(js_string_of),
            // `"type" in value` is also true for arrays/strings only when it is an object; JSON scalars are not.
            _ => None,
        };
        if !message_ids.contains(row.message_id.as_str()) {
            warnings.push(Warning {
                reason: "orphan-part",
                session_id: session.id.clone(),
                message_id: Some(row.message_id.clone()),
                part_id: Some(row.id.clone()),
                observed_type,
            });
            continue;
        }
        let decoded = match parsed {
            Some(Value::Object(mut value)) => {
                value.insert("id".into(), Value::from(row.id.clone()));
                value.insert("messageID".into(), Value::from(row.message_id.clone()));
                value.insert("sessionID".into(), Value::from(row.session_id.clone()));
                if schema::part(&value) { Some(value) } else { None }
            }
            _ => None,
        };
        match decoded {
            Some(value) => parts.push(Prt { row, value }),
            None => warnings.push(Warning {
                reason: "invalid-part",
                session_id: session.id.clone(),
                message_id: Some(row.message_id.clone()),
                part_id: Some(row.id.clone()),
                observed_type,
            }),
        }
    }
    parts.sort_by(|a, b| utf16_locale_cmp(&a.row.id, &b.row.id));
    let mut by_message: HashMap<&str, Vec<&Map<String, Value>>> = HashMap::new();
    for p in &parts {
        by_message.entry(p.row.message_id.as_str()).or_default().push(&p.value);
    }
    let empty: Vec<&Map<String, Value>> = Vec::new();
    let owned_of = |id: &str| -> &Vec<&Map<String, Value>> { by_message.get(id).unwrap_or(&empty) };
    let mut paired: HashSet<String> = HashSet::new();
    let mut used: HashSet<String> = messages.iter().map(|m| m.row.id.clone()).collect();

    let mut projected: Vec<MessageRow> = Vec::new();
    for (index, item) in messages.iter().enumerate() {
        if paired.contains(&item.row.id) {
            continue;
        }
        let owned = owned_of(&item.row.id);
        if item.role() == "user" {
            if let Some(compaction) = owned.iter().find(|p| ptype(p) == "compaction") {
                let summary = messages.iter().find(|c| {
                    c.role() == "assistant"
                        && get_str(&c.value, "parentID") == Some(item.row.id.as_str())
                        && c.value.get("summary") == Some(&Value::Bool(true))
                });
                let Some(summary) = summary else { continue };
                paired.insert(summary.row.id.clone());
                let completed = get_obj(&summary.value, "time").is_some_and(|t| t.contains_key("completed"));
                if summary.value.contains_key("error") || !completed {
                    continue;
                }
                let summary_text = owned_of(&summary.row.id)
                    .iter()
                    .filter(|p| ptype(p) == "text" && !get_str(p, "text").unwrap_or("").is_empty())
                    .map(|p| get_str(p, "text").unwrap_or(""))
                    .collect::<Vec<_>>()
                    .join("\n\n");
                let tail_index = get_str(compaction, "tail_start_id")
                    .and_then(|tail| messages.iter().position(|c| c.row.id == tail));
                let tail: &[Msg] = match tail_index {
                    Some(t) if t <= index => &messages[t..index],
                    _ => &[],
                };
                let mut data = Map::new();
                data.insert("status".into(), "completed".into());
                data.insert("reason".into(), if get_bool(compaction, "auto") { "auto" } else { "manual" }.into());
                data.insert("summary".into(), summary_text.into());
                // V1 kept these messages beside the summary; they replay as text, like compactions before v2.1.
                let recent = serialize_recent(tail, &owned_of);
                if !recent.is_empty() {
                    data.insert("metadata".into(), json!({ "recent": recent }));
                }
                data.insert("time".into(), json!({"created": item.row.time_created}));
                let mut row = make_row(item.row, item.row.id.clone(), "compaction", data);
                row.time_updated = item.row.time_updated.max(summary.row.time_updated);
                projected.push(row);
                continue;
            }
            let subtasks = owned.iter().filter(|p| ptype(p) == "subtask").count();
            let visible: Vec<&&Map<String, Value>> =
                owned.iter().filter(|p| ptype(p) == "text" && !get_bool(p, "ignored")).collect();
            let files: Vec<&&Map<String, Value>> = owned.iter().filter(|p| ptype(p) == "file").collect();
            let agents: Vec<&&Map<String, Value>> = owned.iter().filter(|p| ptype(p) == "agent").collect();
            if subtasks > 0 && visible.is_empty() && files.is_empty() && agents.is_empty() {
                continue;
            }
            let ordinary = visible.iter().filter(|p| !get_bool(p, "synthetic")).count();
            let synthetic: Vec<&str> = visible
                .iter()
                .filter(|p| get_bool(p, "synthetic"))
                .map(|p| get_str(p, "text").unwrap_or(""))
                .collect();
            let mut attachments: Vec<Value> = Vec::new();
            for f in &files {
                attachments.extend(migrate_file(f)?);
            }
            let unavailable = files.iter().filter(|p| !get_str(p, "url").unwrap_or("").starts_with("data:")).count();
            let text = owned
                .iter()
                .filter_map(|p| match ptype(p) {
                    "text" if !get_bool(p, "ignored") && !get_bool(p, "synthetic") => {
                        Some(get_str(p, "text").unwrap_or("").to_string())
                    }
                    "file" if !get_str(p, "url").unwrap_or("").starts_with("data:") => Some(unavailable_file(p)),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n\n");
            let agent_attachments: Vec<Value> = agents
                .iter()
                .map(|p| {
                    let mut a = Map::new();
                    a.insert("name".into(), p.get("name").cloned().unwrap_or(Value::Null));
                    if let Some(source) = get_obj(p, "source") {
                        a.insert(
                            "mention".into(),
                            json!({
                                "text": source.get("value").cloned().unwrap_or(Value::Null),
                                "start": source.get("start").cloned().unwrap_or(Value::Null),
                                "end": source.get("end").cloned().unwrap_or(Value::Null),
                            }),
                        );
                    }
                    Value::Object(a)
                })
                .collect();
            let time = json!({"created": item.row.time_created});
            if ordinary == 0
                && unavailable == 0
                && !synthetic.is_empty()
                && attachments.is_empty()
                && agent_attachments.is_empty()
            {
                let mut data = Map::new();
                data.insert("text".into(), synthetic.join("\n\n").into());
                data.insert("time".into(), time);
                projected.push(make_row(item.row, item.row.id.clone(), "synthetic", data));
                continue;
            }
            let mut data = Map::new();
            data.insert("text".into(), text.into());
            if !attachments.is_empty() {
                data.insert("files".into(), Value::Array(attachments));
            }
            if !agent_attachments.is_empty() {
                data.insert("agents".into(), Value::Array(agent_attachments));
            }
            data.insert("time".into(), time.clone());
            projected.push(make_row(item.row, item.row.id.clone(), "user", data));
            if !synthetic.is_empty() {
                let mut data = Map::new();
                data.insert("text".into(), synthetic.join("\n\n").into());
                data.insert("time".into(), time);
                let id = synthetic_id(&item.row.id, &mut used);
                projected.push(make_row(item.row, id, "synthetic", data));
            }
            continue;
        }
        if item.role() != "assistant" {
            continue;
        }
        let a = &item.value;
        let parent_id = get_str(a, "parentID").unwrap_or("");
        let parent = messages.iter().find(|c| c.row.id == parent_id);
        let parent_parts = parent.map(|p| owned_of(&p.row.id)).unwrap_or(&empty);
        if parent_parts.iter().any(|p| ptype(p) == "subtask")
            && owned.iter().any(|p| ptype(p) == "tool" && get_str(p, "tool") == Some("task"))
        {
            continue;
        }
        let mut content: Vec<Value> = Vec::new();
        for p in owned.iter() {
            match ptype(p) {
                "text" => {
                    let mut c = Map::new();
                    c.insert("type".into(), "text".into());
                    c.insert("text".into(), p.get("text").cloned().unwrap_or(Value::Null));
                    if let Some(m) = p.get("metadata") {
                        c.insert("native".into(), m.clone());
                    }
                    content.push(Value::Object(c));
                }
                "reasoning" => {
                    let mut c = Map::new();
                    c.insert("type".into(), "reasoning".into());
                    c.insert("text".into(), p.get("text").cloned().unwrap_or(Value::Null));
                    if let Some(m) = p.get("metadata") {
                        c.insert("native".into(), m.clone());
                    }
                    let t = get_obj(p, "time");
                    let mut time = Map::new();
                    time.insert("created".into(), t.and_then(|t| t.get("start")).cloned().unwrap_or(Value::Null));
                    if let Some(end) = t.and_then(|t| t.get("end")) {
                        time.insert("completed".into(), end.clone());
                    }
                    c.insert("time".into(), Value::Object(time));
                    content.push(Value::Object(c));
                }
                "tool" => content.push(migrate_tool(p, item.row.time_created)),
                _ => {}
            }
        }
        // `a ?? b ?? c`: only a missing value falls through; an empty string stops the chain.
        let start: Option<&str> = owned
            .iter()
            .filter(|p| ptype(p) == "step-start")
            .filter_map(|p| get_str(p, "snapshot").filter(|s| !s.is_empty()))
            .next()
            .or_else(|| owned.iter().filter(|p| ptype(p) == "snapshot").filter_map(|p| get_str(p, "snapshot")).next())
            .or_else(|| owned.iter().filter(|p| ptype(p) == "patch").filter_map(|p| get_str(p, "hash")).next());
        let end: Option<&str> = owned
            .iter()
            .filter(|p| ptype(p) == "step-finish")
            .filter_map(|p| get_str(p, "snapshot").filter(|s| !s.is_empty()))
            .last();
        let mut snapshot_files: Vec<&str> = Vec::new();
        for p in owned.iter().filter(|p| ptype(p) == "patch") {
            for f in p.get("files").and_then(Value::as_array).into_iter().flatten() {
                if let Some(f) = f.as_str() {
                    if !snapshot_files.contains(&f) {
                        snapshot_files.push(f);
                    }
                }
            }
        }
        let start = start.filter(|s| !s.is_empty());
        let end = end.filter(|s| !s.is_empty());
        let finish = normalize_finish(get_str(a, "finish"));
        let mut data = Map::new();
        data.insert("agent".into(), a.get("agent").cloned().unwrap_or(Value::Null));
        data.insert(
            "model".into(),
            json!({
                "providerID": a.get("providerID").cloned().unwrap_or(Value::Null),
                "id": a.get("modelID").cloned().unwrap_or(Value::Null),
                "variant": a.get("variant").cloned().unwrap_or_else(|| "default".into()),
            }),
        );
        data.insert("content".into(), Value::Array(content));
        if start.is_some() || end.is_some() || !snapshot_files.is_empty() {
            let mut s = Map::new();
            if let Some(start) = start {
                s.insert("start".into(), start.into());
            }
            if let Some(end) = end {
                s.insert("end".into(), end.into());
            }
            if !snapshot_files.is_empty() {
                s.insert("files".into(), snapshot_files.iter().map(|f| Value::from(*f)).collect());
            }
            data.insert("snapshot".into(), Value::Object(s));
        }
        if let Some(finish) = finish {
            data.insert("finish".into(), finish.into());
        }
        data.insert("cost".into(), a.get("cost").cloned().unwrap_or(Value::Null));
        let tokens = get_obj(a, "tokens");
        let tk = |k: &str| tokens.and_then(|t| t.get(k)).cloned().unwrap_or(Value::Null);
        let cache = tokens.and_then(|t| get_obj(t, "cache"));
        let ck = |k: &str| cache.and_then(|c| c.get(k)).cloned().unwrap_or(Value::Null);
        data.insert(
            "tokens".into(),
            json!({
                "input": tk("input"),
                "output": tk("output"),
                "reasoning": tk("reasoning"),
                "cache": {"read": ck("read"), "write": ck("write")},
            }),
        );
        if let Some(error) = get_obj(a, "error") {
            data.insert("error".into(), migrate_error(error));
        }
        let mut time = Map::new();
        time.insert("created".into(), item.row.time_created.into());
        if get_obj(a, "time").is_some_and(|t| t.contains_key("completed")) {
            time.insert("completed".into(), item.row.time_updated.into());
        }
        data.insert("time".into(), Value::Object(time));
        projected.push(make_row(item.row, item.row.id.clone(), "assistant", data));
    }
    for (seq, m) in projected.iter_mut().enumerate() {
        m.seq = seq as i64;
    }
    let mut migrated = projected;
    if let Some(last) = migrated.last().cloned() {
        if let Some(notice) = legacy_tool_notice(&migrated) {
            let mut data = Map::new();
            data.insert("text".into(), notice.into());
            data.insert("time".into(), json!({"created": last.time_created}));
            let seq = migrated.len() as i64;
            migrated.push(MessageRow {
                id: synthetic_id(&last.id, &mut used),
                session_id: last.session_id.clone(),
                r#type: "system".into(),
                seq,
                time_created: last.time_created,
                time_updated: last.time_updated,
                data: Value::Object(data),
            });
        }
    }
    let assistants: Vec<&Map<String, Value>> =
        messages.iter().filter(|m| m.role() == "assistant").map(|m| &m.value).collect();
    let latest_user = messages.iter().rev().find(|m| {
        if m.role() != "user" {
            return false;
        }
        let owned = owned_of(&m.row.id);
        if owned.iter().any(|p| ptype(p) == "compaction") {
            return false;
        }
        !owned.iter().any(|p| ptype(p) == "subtask") || !owned.iter().all(|p| ptype(p) == "subtask")
    });
    let agent = session
        .agent
        .clone()
        .or_else(|| latest_user.and_then(|u| get_str(&u.value, "agent")).map(str::to_string));
    let model = match &session.model {
        Some(m) if !m.is_null() => Some(m.clone()),
        _ => latest_user.map(|u| {
            let model = get_obj(&u.value, "model");
            let f = |k: &str| model.and_then(|m| m.get(k)).cloned();
            json!({
                "id": f("modelID").unwrap_or(Value::Null),
                "providerID": f("providerID").unwrap_or(Value::Null),
                "variant": f("variant").unwrap_or_else(|| "default".into()),
            })
        }),
    };
    let sum = |f: &dyn Fn(&Map<String, Value>) -> f64| assistants.iter().fold(0.0, |total, a| total + f(a));
    let token = |a: &Map<String, Value>, k: &str| num_f64(get_obj(a, "tokens").and_then(|t| t.get(k)));
    let cache = |a: &Map<String, Value>, k: &str| {
        num_f64(get_obj(a, "tokens").and_then(|t| get_obj(t, "cache")).and_then(|c| c.get(k)))
    };
    let session_update = SessionUpdate {
        agent,
        model,
        cost: sum(&|a| num_f64(a.get("cost"))),
        tokens_input: sum(&|a| token(a, "input")),
        tokens_output: sum(&|a| token(a, "output")),
        tokens_reasoning: sum(&|a| token(a, "reasoning")),
        tokens_cache_read: sum(&|a| cache(a, "read")),
        tokens_cache_write: sum(&|a| cache(a, "write")),
    };
    let watermark = migrated.len() as i64 - 1;
    Ok(TransformResult { messages: migrated, session: session_update, watermark, warnings })
}

/// `String(value)` for the warning's observed type.
fn js_string_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => js_number(n),
        Value::Array(_) => "[array]".into(),
        Value::Object(_) => "[object Object]".into(),
    }
}

fn make_row(source: &SourceMessage, id: String, ty: &str, data: Map<String, Value>) -> MessageRow {
    MessageRow {
        id,
        session_id: source.session_id.clone(),
        r#type: ty.into(),
        seq: 0,
        time_created: source.time_created,
        time_updated: source.time_updated,
        data: Value::Object(data),
    }
}

const RENAMED_TOOLS: [(&str, &str); 3] = [("bash", "shell"), ("task", "subagent"), ("apply_patch", "patch")];
const PATH_TOOLS: [&str; 3] = ["read", "edit", "write"];
const REMOVED_TOOLS: [&str; 1] = ["todowrite"];

/// `legacyToolNotice`: tells the model about V1 tools in the still-visible history whose V2 names or arguments
/// differ.
fn legacy_tool_notice(messages: &[MessageRow]) -> Option<String> {
    let start = messages.iter().rposition(|m| m.r#type == "compaction").map(|i| i + 1).unwrap_or(0);
    let mut called: HashSet<&str> = HashSet::new();
    for m in &messages[start..] {
        if m.r#type != "assistant" {
            continue;
        }
        for item in m.data.get("content").and_then(Value::as_array).into_iter().flatten() {
            if item.get("type").and_then(Value::as_str) == Some("tool") {
                if let Some(name) = item.get("name").and_then(Value::as_str) {
                    called.insert(name);
                }
            }
        }
    }
    let list = |names: &[&str]| names.iter().map(|n| format!("`{n}`")).collect::<Vec<_>>().join(", ");
    let renamed: Vec<(&str, &str)> = RENAMED_TOOLS.iter().copied().filter(|(n, _)| called.contains(n)).collect();
    let paths: Vec<&str> = PATH_TOOLS.iter().copied().filter(|n| called.contains(n)).collect();
    let removed: Vec<&str> = REMOVED_TOOLS.iter().copied().filter(|n| called.contains(n)).collect();
    let mut parts: Vec<String> = Vec::new();
    if renamed.len() == 1 {
        parts.push(format!("The `{}` tool is now `{}` and must be called by that name.", renamed[0].0, renamed[0].1));
    } else if renamed.len() > 1 {
        parts.push(format!(
            "The following tools were renamed and must be called by their new names: {}.",
            renamed.iter().map(|(o, n)| format!("`{o}` is now `{n}`")).collect::<Vec<_>>().join("; ")
        ));
    }
    if called.contains("task") {
        parts.push("The `subagent` tool takes `agent` instead of `subagent_type` and `sessionID` instead of `task_id`.".into());
    }
    if paths.len() == 1 {
        parts.push(format!("The `{}` tool now takes `path` instead of `filePath`.", paths[0]));
    } else if paths.len() > 1 {
        parts.push(format!("The following tools now take `path` instead of `filePath`: {}.", list(&paths)));
    }
    if called.contains("skill") {
        parts.push("The `skill` tool now takes `id` instead of `name`.".into());
    }
    if removed.len() == 1 {
        parts.push(format!("The `{}` tool is no longer available by default. Only call it if it is in your tool list.", removed[0]));
    } else if removed.len() > 1 {
        parts.push(format!(
            "The following tools are no longer available by default. Only call them if they are in your tool list: {}.",
            list(&removed)
        ));
    }
    if parts.is_empty() {
        return None;
    }
    let mut all = vec!["The available tools have changed.".to_string()];
    all.extend(parts);
    Some(all.join("\n\n"))
}

fn migrate_tool(part: &Map<String, Value>, fallback: i64) -> Value {
    let mut out = Map::new();
    out.insert("type".into(), "tool".into());
    out.insert("id".into(), part.get("callID").cloned().unwrap_or(Value::Null));
    out.insert("name".into(), part.get("tool").cloned().unwrap_or(Value::Null));
    if let Some(m) = part.get("metadata") {
        out.insert("native".into(), m.clone());
    }
    let empty = Map::new();
    let state = get_obj(part, "state").unwrap_or(&empty);
    let status = get_str(state, "status").unwrap_or("");
    let input = state.get("input").cloned().unwrap_or(Value::Null);
    let time = get_obj(state, "time");
    let t = |k: &str| time.and_then(|t| t.get(k)).cloned().unwrap_or(Value::Null);
    let mut s = Map::new();
    match status {
        "completed" => {
            s.insert("status".into(), "completed".into());
            s.insert("input".into(), input);
            let compacted = time.is_some_and(|t| t.contains_key("compacted"));
            let content: Vec<Value> = if !compacted {
                let mut c = vec![json!({"type": "text", "text": state.get("output").cloned().unwrap_or(Value::Null)})];
                for file in state.get("attachments").and_then(Value::as_array).into_iter().flatten() {
                    let Some(file) = file.as_object() else { continue };
                    let mut f = Map::new();
                    f.insert("type".into(), "file".into());
                    f.insert("uri".into(), file.get("url").cloned().unwrap_or(Value::Null));
                    f.insert("mime".into(), file.get("mime").cloned().unwrap_or(Value::Null));
                    if let Some(name) = get_str(file, "filename").filter(|n| !n.is_empty()) {
                        f.insert("name".into(), name.into());
                    }
                    c.push(Value::Object(f));
                }
                c
            } else {
                vec![json!({"type": "text", "text": "[Old tool result content cleared]"})]
            };
            s.insert("content".into(), Value::Array(content));
            s.insert("metadata".into(), state.get("metadata").cloned().unwrap_or(Value::Null));
            out.insert("state".into(), Value::Object(s));
            out.insert("time".into(), json!({"created": t("start"), "completed": t("end")}));
        }
        "error" => {
            s.insert("status".into(), "error".into());
            s.insert("input".into(), input);
            s.insert(
                "error".into(),
                json!({"type": "tool.execution", "message": state.get("error").cloned().unwrap_or(Value::Null)}),
            );
            let metadata = get_obj(state, "metadata");
            if let Some(output) = metadata.and_then(|m| m.get("output")).filter(|o| o.is_string()) {
                s.insert("content".into(), json!([{"type": "text", "text": output}]));
            }
            if let Some(m) = state.get("metadata") {
                s.insert("metadata".into(), m.clone());
            }
            out.insert("state".into(), Value::Object(s));
            out.insert("time".into(), json!({"created": t("start"), "completed": t("end")}));
        }
        _ => {
            s.insert("status".into(), "error".into());
            s.insert("input".into(), input);
            s.insert(
                "error".into(),
                json!({"type": "tool.interrupted", "message": "Tool execution was interrupted before V2 migration"}),
            );
            let running = status == "running";
            if running {
                if let Some(m) = state.get("metadata") {
                    s.insert("metadata".into(), m.clone());
                }
            }
            out.insert("state".into(), Value::Object(s));
            let created = if running { t("start") } else { Value::from(fallback) };
            out.insert("time".into(), json!({"created": created}));
        }
    }
    Value::Object(out)
}

fn migrate_error(error: &Map<String, Value>) -> Value {
    let name = get_str(error, "name").unwrap_or("");
    let data = get_obj(error, "data");
    let message = match data.and_then(|d| d.get("message")) {
        Some(m) if data.is_some_and(|d| d.contains_key("message")) => m.clone(),
        _ if name == "MessageOutputLengthError" => "The model exceeded its output limit".into(),
        _ => name.into(),
    };
    let ty = match name {
        "ProviderAuthError" => "provider.auth",
        "ContentFilterError" => "provider.content-filter",
        "ContextOverflowError" => "provider.invalid-request",
        "StructuredOutputError" | "MessageOutputLengthError" => "provider.invalid-output",
        "MessageAbortedError" => "aborted",
        "APIError" => "provider.error",
        _ => "unknown",
    };
    json!({"type": ty, "message": message})
}

fn normalize_finish(finish: Option<&str>) -> Option<&'static str> {
    let finish = finish.filter(|f| !f.is_empty())?;
    Some(
        ["stop", "length", "tool-calls", "content-filter", "error", "unknown"]
            .into_iter()
            .find(|v| *v == finish)
            .unwrap_or("unknown"),
    )
}

fn migrate_file(part: &Map<String, Value>) -> Result<Vec<Value>, String> {
    let url = get_str(part, "url").unwrap_or("");
    if !url.starts_with("data:") {
        return Ok(vec![]);
    }
    let Some(comma) = url.find(',') else { return Ok(vec![]) };
    let header = &url[..comma];
    let payload = &url[comma + 1..];
    let bytes = if header.ends_with(";base64") {
        node_base64_decode(payload)
    } else {
        decode_uri_component(payload)?.into_bytes()
    };
    let data = base64::engine::general_purpose::STANDARD.encode(bytes);
    let source = get_obj(part, "source");
    let mut out = Map::new();
    out.insert("data".into(), data.into());
    out.insert("mime".into(), part.get("mime").cloned().unwrap_or(Value::Null));
    out.insert(
        "source".into(),
        match source {
            Some(s) if get_str(s, "type") == Some("resource") => {
                json!({"type": "uri", "uri": s.get("uri").cloned().unwrap_or(Value::Null)})
            }
            _ => json!({"type": "inline"}),
        },
    );
    if let Some(name) = get_str(part, "filename").filter(|n| !n.is_empty()) {
        out.insert("name".into(), name.into());
    }
    if let Some(source) = source {
        let text = get_obj(source, "text");
        let f = |k: &str| text.and_then(|t| t.get(k)).cloned().unwrap_or(Value::Null);
        out.insert("mention".into(), json!({"text": f("value"), "start": f("start"), "end": f("end")}));
    }
    Ok(vec![Value::Object(out)])
}

fn unavailable_file(part: &Map<String, Value>) -> String {
    let source = get_obj(part, "source");
    let label = match get_str(part, "filename") {
        Some(f) => f.to_string(),
        None => match source {
            Some(s) if get_str(s, "type") == Some("resource") => get_str(s, "uri").unwrap_or("").to_string(),
            _ => get_str(part, "url").unwrap_or("").to_string(),
        },
    };
    format!("[Attachment unavailable after migration: {label} ({})]", get_str(part, "mime").unwrap_or(""))
}

/// `syntheticID`: the first 16 characters of `source` plus 14 base62 digits of
/// `sha256("v1-synthetic:<source>[:<salt>]")`, salted until unused.
pub fn synthetic_id(source: &str, used: &mut HashSet<String>) -> String {
    const ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let prefix: String = {
        let units: Vec<u16> = source.encode_utf16().take(16).collect();
        String::from_utf16_lossy(&units)
    };
    let mut salt = 0u64;
    loop {
        let input =
            if salt == 0 { format!("v1-synthetic:{source}") } else { format!("v1-synthetic:{source}:{salt}") };
        let mut value: Vec<u8> = Sha256::digest(input.as_bytes()).to_vec();
        let mut suffix = Vec::with_capacity(14);
        for _ in 0..14 {
            // value /= 62, remainder → digit (big-endian long division).
            let mut rem: u32 = 0;
            for byte in value.iter_mut() {
                let cur = (rem << 8) | *byte as u32;
                *byte = (cur / 62) as u8;
                rem = cur % 62;
            }
            suffix.push(ALPHABET[rem as usize]);
        }
        suffix.reverse();
        let id = format!("{prefix}{}", String::from_utf8(suffix).unwrap());
        if used.insert(id.clone()) {
            return id;
        }
        salt += 1;
    }
}

fn serialize_recent<'a>(messages: &[Msg], owned_of: &dyn Fn(&str) -> &'a Vec<&'a Map<String, Value>>) -> String {
    let mut out: Vec<String> = Vec::new();
    for m in messages {
        let owned = owned_of(&m.row.id);
        if m.role() == "user" {
            let text = owned
                .iter()
                .filter(|p| ptype(p) == "text" && !get_bool(p, "ignored"))
                .map(|p| get_str(p, "text").unwrap_or(""))
                .collect::<Vec<_>>()
                .join("\n\n");
            out.push(format!("[User]: {text}"));
            continue;
        }
        for p in owned {
            match ptype(p) {
                "text" => out.push(format!("[Assistant]: {}", get_str(p, "text").unwrap_or(""))),
                "reasoning" if !get_str(p, "text").unwrap_or("").is_empty() => {
                    out.push(format!("[Assistant reasoning]: {}", get_str(p, "text").unwrap_or("")))
                }
                _ => {}
            }
        }
    }
    out.join("\n\n")
}

/// Node/Bun `Buffer.from(s, "base64")`: both alphabets, whitespace and other characters skipped, stops at `=`.
fn node_base64_decode(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits = 0;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            _ => continue,
        };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
}

/// `decodeURIComponent`; `Err("URI error")` where JS throws a `URIError`.
fn decode_uri_component(s: &str) -> Result<String, String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3).ok_or("URI error")?;
            let hex = std::str::from_utf8(hex).map_err(|_| "URI error")?;
            let b = u8::from_str_radix(hex, 16).map_err(|_| "URI error")?;
            if !hex.bytes().all(|c| c.is_ascii_hexdigit()) {
                return Err("URI error".into());
            }
            out.push(b);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| "URI error".into())
}

// ---------------------------------------------------------------------------------------------------------------
// JSON.stringify
// ---------------------------------------------------------------------------------------------------------------

/// `JSON.stringify(value)` of a value produced by `JSON.parse`: JS number formatting and JS property order
/// (array-index keys first, ascending; then insertion order).
pub fn js_stringify(v: &Value) -> String {
    let mut out = String::new();
    write_js(v, &mut out);
    out
}

fn array_index(key: &str) -> Option<u32> {
    if key.is_empty() || key.len() > 10 || !key.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if key.len() > 1 && key.starts_with('0') {
        return None;
    }
    let n: u64 = key.parse().ok()?;
    if n < u32::MAX as u64 { Some(n as u32) } else { None }
}

fn write_js(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&js_number(n)),
        Value::String(s) => out.push_str(&serde_json::to_string(s).unwrap()),
        Value::Array(a) => {
            out.push('[');
            for (i, item) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_js(item, out);
            }
            out.push(']');
        }
        Value::Object(o) => {
            let mut indexed: Vec<(u32, &String, &Value)> =
                o.iter().filter_map(|(k, v)| array_index(k).map(|i| (i, k, v))).collect();
            indexed.sort_by_key(|(i, _, _)| *i);
            let rest = o.iter().filter(|(k, _)| array_index(k).is_none());
            out.push('{');
            let mut first = true;
            for (k, v) in indexed.into_iter().map(|(_, k, v)| (k, v)).chain(rest) {
                if !first {
                    out.push(',');
                }
                first = false;
                out.push_str(&serde_json::to_string(k).unwrap());
                out.push(':');
                write_js(v, out);
            }
            out.push('}');
        }
    }
}

/// `Number.prototype.toString()` of a parsed JSON number (as JSON.stringify prints it).
pub fn js_number(n: &serde_json::Number) -> String {
    let f = if let Some(i) = n.as_i64() {
        if i.unsigned_abs() <= 9_007_199_254_740_992 {
            return i.to_string();
        }
        i as f64
    } else if let Some(u) = n.as_u64() {
        u as f64
    } else {
        n.as_f64().unwrap_or(f64::NAN)
    };
    js_f64(f)
}

pub fn js_f64(f: f64) -> String {
    if !f.is_finite() {
        return "null".into();
    }
    if f == 0.0 {
        return "0".into();
    }
    let sci = format!("{:e}", f.abs());
    let (mantissa, exp) = sci.split_once('e').unwrap();
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let exp: i32 = exp.parse().unwrap();
    let k = digits.len() as i32;
    let n = exp + 1;
    let body = if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let e = n - 1;
        let sign = if e >= 0 { "+" } else { "-" };
        if k == 1 {
            format!("{digits}e{sign}{}", e.abs())
        } else {
            format!("{}.{}e{sign}{}", &digits[..1], &digits[1..], e.abs())
        }
    };
    if f < 0.0 { format!("-{body}") } else { body }
}

// ---------------------------------------------------------------------------------------------------------------
// opencode-next.db (pre-split V2 database)
// ---------------------------------------------------------------------------------------------------------------

enum Strategy {
    Required,
    Nullable,
    Fallback(&'static str),
}

const NEXT_PROJECT_COLUMNS: [(&str, Strategy); 12] = [
    ("id", Strategy::Required),
    ("worktree", Strategy::Required),
    ("vcs", Strategy::Nullable),
    ("name", Strategy::Nullable),
    ("icon_url", Strategy::Nullable),
    ("icon_url_override", Strategy::Fallback("icon_url")),
    ("icon_color", Strategy::Nullable),
    ("time_created", Strategy::Required),
    ("time_updated", Strategy::Required),
    ("time_initialized", Strategy::Nullable),
    ("sandboxes", Strategy::Required),
    ("commands", Strategy::Nullable),
];

const NEXT_SESSION_COLUMNS: [(&str, Strategy); 32] = [
    ("id", Strategy::Required),
    ("project_id", Strategy::Required),
    ("workspace_id", Strategy::Nullable),
    ("parent_id", Strategy::Nullable),
    ("fork_session_id", Strategy::Nullable),
    ("fork_boundary", Strategy::Nullable),
    ("slug", Strategy::Required),
    ("directory", Strategy::Required),
    ("path", Strategy::Nullable),
    ("title", Strategy::Nullable),
    ("version", Strategy::Required),
    ("share_url", Strategy::Nullable),
    ("summary_additions", Strategy::Nullable),
    ("summary_deletions", Strategy::Nullable),
    ("summary_files", Strategy::Nullable),
    ("summary_diffs", Strategy::Nullable),
    ("metadata", Strategy::Nullable),
    ("cost", Strategy::Required),
    ("tokens_input", Strategy::Required),
    ("tokens_output", Strategy::Required),
    ("tokens_reasoning", Strategy::Required),
    ("tokens_cache_read", Strategy::Required),
    ("tokens_cache_write", Strategy::Required),
    ("revert", Strategy::Nullable),
    ("permission", Strategy::Nullable),
    ("agent", Strategy::Nullable),
    ("model", Strategy::Nullable),
    ("time_created", Strategy::Required),
    ("time_updated", Strategy::Required),
    ("time_compacting", Strategy::Nullable),
    ("time_archived", Strategy::Nullable),
    ("time_suspended", Strategy::Nullable),
];

type SqlRow = Vec<rusqlite::types::Value>;

fn open_next(path: &Path) -> anyhow::Result<Connection> {
    Ok(Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?)
}

fn is_next_database(c: &Connection) -> anyhow::Result<bool> {
    let mut s = c.prepare("SELECT name FROM sqlite_master WHERE type = 'table'")?;
    let tables = s.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<HashSet<_>>>()?;
    Ok(tables.contains("project") && tables.contains("session") && tables.contains("session_message"))
}

fn count_next_sessions(path: Option<&Path>) -> anyhow::Result<u64> {
    let Some(path) = path.filter(|p| p.exists()) else { return Ok(0) };
    let c = open_next(path)?;
    if !is_next_database(&c)? {
        return Ok(0);
    }
    Ok(c.query_row("SELECT COUNT(*) FROM session", [], |r| r.get::<_, i64>(0))? as u64)
}

fn select_next_rows(c: &Connection, table: &str, definition: &[(&str, Strategy)]) -> anyhow::Result<Vec<SqlRow>> {
    let columns: HashSet<String> = {
        let mut s = c.prepare("SELECT name FROM pragma_table_info(?)")?;
        let rows = s.query_map([table], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<HashSet<_>>>()?;
        rows
    };
    let missing: Vec<&str> = definition
        .iter()
        .filter(|(col, strategy)| matches!(strategy, Strategy::Required) && !columns.contains(*col))
        .map(|(col, _)| *col)
        .collect();
    if !missing.is_empty() {
        return Err(anyhow!(
            "Incompatible opencode-next.db: {table} is missing required columns: {}",
            missing.join(", ")
        ));
    }
    let projection: Vec<String> = definition
        .iter()
        .map(|(col, strategy)| {
            if columns.contains(*col) {
                return format!("\"{col}\"");
            }
            if let Strategy::Fallback(fallback) = strategy {
                if columns.contains(*fallback) {
                    return format!("\"{fallback}\" AS \"{col}\"");
                }
            }
            format!("NULL AS \"{col}\"")
        })
        .collect();
    let order = if table == "session" { " ORDER BY \"id\" DESC" } else { "" };
    let mut s = c.prepare(&format!("SELECT {} FROM \"{table}\"{order}", projection.join(", ")))?;
    let n = definition.len();
    let rows = s
        .query_map([], |r| (0..n).map(|i| r.get::<_, rusqlite::types::Value>(i)).collect::<rusqlite::Result<SqlRow>>())?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn text_of(v: &rusqlite::types::Value) -> String {
    match v {
        rusqlite::types::Value::Text(s) => s.clone(),
        rusqlite::types::Value::Integer(i) => i.to_string(),
        rusqlite::types::Value::Real(f) => js_f64(*f),
        _ => String::new(),
    }
}

fn import_next_database(db: &Database, path: Option<&Path>, on_progress: &mut dyn FnMut(u64)) -> anyhow::Result<()> {
    let Some(path) = path.filter(|p| p.exists()) else { return Ok(()) };
    let source = open_next(path)?;
    if !is_next_database(&source)? {
        tracing::warn!(path = %path.display(), "Skipped incompatible opencode-next.db");
        return Ok(());
    }
    source.execute_batch("BEGIN")?;
    let result = (|| -> anyhow::Result<()> {
        let projects: HashMap<String, SqlRow> = select_next_rows(&source, "project", &NEXT_PROJECT_COLUMNS)?
            .into_iter()
            .map(|p| (text_of(&p[0]), p))
            .collect();
        let sessions = select_next_rows(&source, "session", &NEXT_SESSION_COLUMNS)?;
        for (index, session) in sessions.iter().enumerate() {
            let session_id = text_of(&session[0]);
            let project_id = text_of(&session[1]);
            let project = projects.get(&project_id);
            if project.is_none() {
                tracing::warn!(session_id, project_id, "Reassigned previous V2 session with missing project");
            }
            let messages: Vec<SqlRow> = {
                let mut s = source.prepare(
                    "SELECT id, session_id, type, seq, time_created, time_updated, data FROM session_message WHERE session_id = ? ORDER BY seq",
                )?;
                let rows = s
                    .query_map([&session_id], |r| {
                        (0..7).map(|i| r.get::<_, rusqlite::types::Value>(i)).collect::<rusqlite::Result<SqlRow>>()
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                rows
            };
            db.transaction(|tx| {
                if let Some(p) = project {
                    // time_active = time_updated
                    tx.execute(
                        "INSERT OR IGNORE INTO project (
                           id, worktree, vcs, name, icon_url, icon_url_override, icon_color,
                           time_created, time_updated, time_initialized, time_active, sandboxes, commands
                         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                        params![p[0], p[1], p[2], p[3], p[4], p[5], p[6], p[7], p[8], p[9], p[8], p[10], p[11]],
                    )?;
                }
                let existing = tx
                    .query_row("SELECT id FROM session_v2 WHERE id = ?", [&session_id], |r| r.get::<_, String>(0))
                    .optional()?;
                if existing.is_some() {
                    return Ok(());
                }
                let mut values: SqlRow = session.clone();
                if project.is_none() {
                    values[1] = rusqlite::types::Value::Text(GLOBAL_PROJECT.into());
                }
                let cols = NEXT_SESSION_COLUMNS.iter().map(|(c, _)| *c).collect::<Vec<_>>().join(", ");
                let marks = vec!["?"; NEXT_SESSION_COLUMNS.len()].join(", ");
                tx.execute(
                    &format!("INSERT INTO session_v2 ({cols}) VALUES ({marks})"),
                    rusqlite::params_from_iter(values.iter()),
                )?;
                let mut insert = tx.prepare(
                    "INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data)
                     VALUES (?, ?, ?, ?, ?, ?, ?)",
                )?;
                for m in &messages {
                    insert.execute(rusqlite::params_from_iter(m.iter()))?;
                }
                let last_seq: i64 = match messages.last().map(|m| &m[3]) {
                    Some(rusqlite::types::Value::Integer(i)) => *i,
                    Some(rusqlite::types::Value::Real(f)) => *f as i64,
                    _ => -1,
                };
                tx.execute(
                    "INSERT INTO event_sequence (aggregate_id, seq) VALUES (?1, ?2)
                     ON CONFLICT (aggregate_id) DO UPDATE SET seq = ?2, owner_id = NULL",
                    params![session_id, last_seq],
                )?;
                Ok(())
            })?;
            on_progress(index as u64 + 1);
        }
        Ok(())
    })();
    match result {
        Ok(()) => {
            source.execute_batch("COMMIT")?;
            Ok(())
        }
        Err(e) => {
            let _ = source.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// SessionV1 schema validation (`@opencode/schema/session-v1`), decode success only
// ---------------------------------------------------------------------------------------------------------------

mod schema {
    use serde_json::{Map, Value};

    type Obj = Map<String, Value>;

    fn string(v: &Value) -> bool {
        v.is_string()
    }
    fn boolean(v: &Value) -> bool {
        v.is_boolean()
    }
    fn finite(v: &Value) -> bool {
        v.as_f64().is_some_and(f64::is_finite)
    }
    fn non_negative_int(v: &Value) -> bool {
        v.as_f64().is_some_and(|f| f.fract() == 0.0 && f.abs() <= 9_007_199_254_740_991.0 && f >= 0.0)
    }
    fn any(_: &Value) -> bool {
        true
    }
    fn record(v: &Value) -> bool {
        v.is_object()
    }
    fn record_of(v: &Value, f: fn(&Value) -> bool) -> bool {
        v.as_object().is_some_and(|o| o.values().all(f))
    }
    fn starts(v: &Value, prefix: &str) -> bool {
        v.as_str().is_some_and(|s| s.starts_with(prefix))
    }
    fn literal(v: Option<&Value>, lit: &str) -> bool {
        v.and_then(Value::as_str) == Some(lit)
    }
    fn req(o: &Obj, key: &str, f: impl Fn(&Value) -> bool) -> bool {
        o.get(key).is_some_and(f)
    }
    fn opt(o: &Obj, key: &str, f: impl Fn(&Value) -> bool) -> bool {
        o.get(key).is_none_or(f)
    }
    fn obj(v: &Value, f: impl Fn(&Obj) -> bool) -> bool {
        v.as_object().is_some_and(f)
    }
    fn array(v: &Value, f: impl Fn(&Value) -> bool) -> bool {
        v.as_array().is_some_and(|a| a.iter().all(f))
    }

    fn part_base(o: &Obj) -> bool {
        req(o, "id", |v| starts(v, "prt")) && req(o, "sessionID", |v| starts(v, "ses")) && req(o, "messageID", |v| starts(v, "msg"))
    }

    fn file_diff(v: &Value) -> bool {
        obj(v, |o| {
            opt(o, "file", string)
                && opt(o, "patch", string)
                && req(o, "additions", finite)
                && req(o, "deletions", finite)
                && opt(o, "status", |s| matches!(s.as_str(), Some("added" | "deleted" | "modified")))
        })
    }

    fn format(v: &Value) -> bool {
        obj(v, |o| match o.get("type").and_then(Value::as_str) {
            Some("text") => true,
            Some("json_schema") => req(o, "schema", record) && opt(o, "retryCount", non_negative_int),
            _ => false,
        })
    }

    fn user(o: &Obj) -> bool {
        req(o, "time", |t| obj(t, |t| req(t, "created", |c| finite(c) && c.as_f64().unwrap() >= 0.0)))
            && opt(o, "format", format)
            && opt(o, "summary", |s| {
                obj(s, |s| opt(s, "title", string) && opt(s, "body", string) && req(s, "diffs", |d| array(d, file_diff)))
            })
            && req(o, "agent", string)
            && req(o, "model", |m| {
                obj(m, |m| req(m, "providerID", string) && req(m, "modelID", string) && opt(m, "variant", string))
            })
            && opt(o, "system", string)
            && opt(o, "tools", |t| record_of(t, boolean))
    }

    fn api_error_data(d: &Obj) -> bool {
        req(d, "message", string)
            && opt(d, "statusCode", non_negative_int)
            && req(d, "isRetryable", boolean)
            && opt(d, "responseHeaders", |h| record_of(h, string))
            && opt(d, "responseBody", string)
            && opt(d, "metadata", |h| record_of(h, string))
    }

    fn assistant_error(v: &Value) -> bool {
        obj(v, |e| {
            let data = |f: &dyn Fn(&Obj) -> bool| req(e, "data", |d| obj(d, f));
            match e.get("name").and_then(Value::as_str) {
                Some("ProviderAuthError") => data(&|d| req(d, "providerID", string) && req(d, "message", string)),
                Some("UnknownError") => data(&|d| req(d, "message", string) && opt(d, "ref", string)),
                Some("MessageOutputLengthError") => data(&|_| true),
                Some("MessageAbortedError") => data(&|d| req(d, "message", string)),
                Some("StructuredOutputError") => data(&|d| req(d, "message", string) && req(d, "retries", non_negative_int)),
                Some("ContextOverflowError") => data(&|d| req(d, "message", string) && opt(d, "responseBody", string)),
                Some("ContentFilterError") => data(&|d| req(d, "message", string)),
                Some("APIError") => data(&api_error_data),
                _ => false,
            }
        })
    }

    fn tokens(v: &Value) -> bool {
        obj(v, |t| {
            opt(t, "total", finite)
                && req(t, "input", finite)
                && req(t, "output", finite)
                && req(t, "reasoning", finite)
                && req(t, "cache", |c| obj(c, |c| req(c, "read", finite) && req(c, "write", finite)))
        })
    }

    fn assistant(o: &Obj) -> bool {
        req(o, "time", |t| obj(t, |t| req(t, "created", non_negative_int) && opt(t, "completed", non_negative_int)))
            && opt(o, "error", assistant_error)
            && req(o, "parentID", |v| starts(v, "msg"))
            && req(o, "modelID", string)
            && req(o, "providerID", string)
            && req(o, "mode", string)
            && req(o, "agent", string)
            && req(o, "path", |p| obj(p, |p| req(p, "cwd", string) && req(p, "root", string)))
            && opt(o, "summary", boolean)
            && req(o, "cost", finite)
            && req(o, "tokens", tokens)
            && opt(o, "structured", any)
            && opt(o, "variant", string)
            && opt(o, "finish", string)
    }

    /// `SessionV1.Info` (the row's `id` / `sessionID` already merged in).
    pub fn info(o: &Obj) -> bool {
        if !(req(o, "id", |v| starts(v, "msg")) && req(o, "sessionID", |v| starts(v, "ses"))) {
            return false;
        }
        match o.get("role").and_then(Value::as_str) {
            Some("user") => user(o),
            Some("assistant") => assistant(o),
            _ => false,
        }
    }

    fn source_text(o: &Obj) -> bool {
        req(o, "text", |t| obj(t, |t| req(t, "value", string) && req(t, "start", finite) && req(t, "end", finite)))
    }

    fn position(v: &Value) -> bool {
        obj(v, |p| req(p, "line", non_negative_int) && req(p, "character", non_negative_int))
    }

    fn file_source(v: &Value) -> bool {
        obj(v, |s| {
            source_text(s)
                && match s.get("type").and_then(Value::as_str) {
                    Some("file") => req(s, "path", string),
                    Some("symbol") => {
                        req(s, "path", string)
                            && req(s, "range", |r| obj(r, |r| req(r, "start", position) && req(r, "end", position)))
                            && req(s, "name", string)
                            && req(s, "kind", non_negative_int)
                    }
                    Some("resource") => req(s, "clientName", string) && req(s, "uri", string),
                    _ => false,
                }
        })
    }

    fn file_part(o: &Obj) -> bool {
        part_base(o)
            && literal(o.get("type"), "file")
            && req(o, "mime", string)
            && opt(o, "filename", string)
            && req(o, "url", string)
            && opt(o, "source", file_source)
    }

    fn tool_state(v: &Value) -> bool {
        obj(v, |s| {
            let input = req(s, "input", record);
            match s.get("status").and_then(Value::as_str) {
                Some("pending") => input && req(s, "raw", string),
                Some("running") => {
                    input
                        && opt(s, "title", string)
                        && opt(s, "metadata", record)
                        && req(s, "time", |t| obj(t, |t| req(t, "start", non_negative_int)))
                }
                Some("completed") => {
                    input
                        && req(s, "output", string)
                        && req(s, "title", string)
                        && req(s, "metadata", record)
                        && req(s, "time", |t| {
                            obj(t, |t| {
                                req(t, "start", non_negative_int)
                                    && req(t, "end", non_negative_int)
                                    && opt(t, "compacted", non_negative_int)
                            })
                        })
                        && opt(s, "attachments", |a| array(a, |f| obj(f, file_part)))
                }
                Some("error") => {
                    input
                        && req(s, "error", string)
                        && opt(s, "metadata", record)
                        && req(s, "time", |t| obj(t, |t| req(t, "start", non_negative_int) && req(t, "end", non_negative_int)))
                }
                _ => false,
            }
        })
    }

    fn time_start_end(v: &Value) -> bool {
        obj(v, |t| req(t, "start", non_negative_int) && opt(t, "end", non_negative_int))
    }

    /// `SessionV1.Part` (the row's `id` / `messageID` / `sessionID` already merged in).
    pub fn part(o: &Obj) -> bool {
        if !part_base(o) {
            return false;
        }
        match o.get("type").and_then(Value::as_str) {
            Some("text") => {
                req(o, "text", string)
                    && opt(o, "synthetic", boolean)
                    && opt(o, "ignored", boolean)
                    && opt(o, "time", time_start_end)
                    && opt(o, "metadata", record)
            }
            Some("subtask") => {
                req(o, "prompt", string)
                    && req(o, "description", string)
                    && req(o, "agent", string)
                    && opt(o, "model", |m| obj(m, |m| req(m, "providerID", string) && req(m, "modelID", string)))
                    && opt(o, "command", string)
            }
            Some("reasoning") => req(o, "text", string) && opt(o, "metadata", record) && req(o, "time", time_start_end),
            Some("file") => file_part(o),
            Some("tool") => {
                req(o, "callID", string) && req(o, "tool", string) && req(o, "state", tool_state) && opt(o, "metadata", record)
            }
            Some("step-start") => opt(o, "snapshot", string),
            Some("step-finish") => {
                req(o, "reason", string) && opt(o, "snapshot", string) && req(o, "cost", finite) && req(o, "tokens", tokens)
            }
            Some("snapshot") => req(o, "snapshot", string),
            Some("patch") => req(o, "hash", string) && req(o, "files", |f| array(f, string)),
            Some("agent") => {
                req(o, "name", string)
                    && opt(o, "source", |s| {
                        obj(s, |s| req(s, "value", string) && req(s, "start", non_negative_int) && req(s, "end", non_negative_int))
                    })
            }
            Some("retry") => {
                req(o, "attempt", non_negative_int)
                    && req(o, "error", |e| {
                        obj(e, |e| literal(e.get("name"), "APIError") && req(e, "data", |d| obj(d, api_error_data)))
                    })
                    && req(o, "time", |t| obj(t, |t| req(t, "created", non_negative_int)))
            }
            Some("compaction") => {
                req(o, "auto", boolean) && opt(o, "overflow", boolean) && opt(o, "tail_start_id", |v| starts(v, "msg"))
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `v1-migration.test.ts`: the legacy tool notice, `todowrite` among the removed tools.
    #[test]
    fn legacy_tool_notice_lists_todowrite_as_removed() {
        let row = |r#type: &str, data: Value| MessageRow {
            id: "msg_1".into(),
            session_id: "ses_test".into(),
            r#type: r#type.into(),
            seq: 0,
            time_created: 1,
            time_updated: 1,
            data,
        };
        let calls: Vec<Value> = ["bash", "read", "glob", "todowrite", "task", "apply_patch"]
            .iter()
            .map(|name| serde_json::json!({ "type": "tool", "id": name, "name": name }))
            .collect();
        let notice = legacy_tool_notice(&[row("user", serde_json::json!({})), row("assistant", serde_json::json!({ "content": calls }))]);
        assert_eq!(
            notice.as_deref(),
            Some(
                [
                    "The available tools have changed.",
                    "The following tools were renamed and must be called by their new names: `bash` is now `shell`; `task` is now `subagent`; `apply_patch` is now `patch`.",
                    "The `subagent` tool takes `agent` instead of `subagent_type` and `sessionID` instead of `task_id`.",
                    "The `read` tool now takes `path` instead of `filePath`.",
                    "The `todowrite` tool is no longer available by default. Only call it if it is in your tool list.",
                ]
                .join("\n\n")
                .as_str()
            )
        );
        let todo = [row("assistant", serde_json::json!({ "content": [{ "type": "tool", "id": "t", "name": "todowrite" }] }))];
        assert_eq!(
            legacy_tool_notice(&todo).as_deref(),
            Some("The available tools have changed.\n\nThe `todowrite` tool is no longer available by default. Only call it if it is in your tool list.")
        );
    }

    #[test]
    fn js_numbers() {
        let n = |s: &str| js_number(&serde_json::from_str::<serde_json::Number>(s).unwrap());
        assert_eq!(n("0.00020555"), "0.00020555");
        assert_eq!(n("1.0"), "1");
        assert_eq!(n("-0"), "0");
        assert_eq!(n("1e-7"), "1e-7");
        assert_eq!(n("1.5e-7"), "1.5e-7");
        assert_eq!(n("0.000001"), "0.000001");
        assert_eq!(n("1e21"), "1e+21");
        assert_eq!(n("123456789012345678901"), "123456789012345680000");
        assert_eq!(n("9007199254740993"), "9007199254740992");
        assert_eq!(n("12.5"), "12.5");
        assert_eq!(n("-3.25e30"), "-3.25e+30");
    }

    #[test]
    fn js_key_order() {
        let v: Value = serde_json::from_str(r#"{"b":1,"10":2,"a":{"2":true,"1":null,"x":"\u0001"},"01":3,"4294967295":4,"0":5}"#).unwrap();
        assert_eq!(
            js_stringify(&v),
            r#"{"0":5,"10":2,"b":1,"a":{"1":null,"2":true,"x":"\u0001"},"01":3,"4294967295":4}"#
        );
    }

    #[test]
    fn synthetic_ids_are_salted() {
        let mut used = HashSet::new();
        let a = synthetic_id("msg_13271bf900019zAFj6LFkgRxuo", &mut used);
        assert!(a.starts_with("msg_13271bf90001"));
        assert_eq!(a.len(), 30);
        let mut again = HashSet::new();
        assert_eq!(synthetic_id("msg_13271bf900019zAFj6LFkgRxuo", &mut again), a);
        let b = synthetic_id("msg_13271bf900019zAFj6LFkgRxuo", &mut used);
        assert_ne!(a, b);
    }

    #[test]
    fn data_urls() {
        assert_eq!(node_base64_decode("aGk"), b"hi");
        assert_eq!(node_base64_decode("aG k=\n"), b"hi");
        assert_eq!(decode_uri_component("a%20b%C3%A9").unwrap(), "a bé");
        assert!(decode_uri_component("%E0%A4%A").is_err());
        assert!(decode_uri_component("%C3").is_err());
    }

    #[test]
    fn finish_and_errors() {
        assert_eq!(normalize_finish(Some("tool-calls")), Some("tool-calls"));
        assert_eq!(normalize_finish(Some("other")), Some("unknown"));
        assert_eq!(normalize_finish(Some("")), None);
        let e: Map<String, Value> = serde_json::from_str(r#"{"name":"MessageOutputLengthError","data":{}}"#).unwrap();
        assert_eq!(migrate_error(&e), json!({"type":"provider.invalid-output","message":"The model exceeded its output limit"}));
    }

    #[test]
    fn root_of_data_dir() {
        assert_eq!(path_root(Path::new("/home/x/.local/share/opencode")), "/");
    }
}
