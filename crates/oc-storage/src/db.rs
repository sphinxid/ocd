use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use rusqlite::Connection;

/// One connection per process, all statements serialized (`core/src/database/sqlite.ts`).
#[derive(Clone)]
pub struct Database {
    conn: Arc<Mutex<Connection>>,
    pub path: PathBuf,
}

pub fn now_ms() -> i64 {
    oc_schema::id::now_ms() as i64
}

#[cfg(unix)]
fn restrict_to_owner(path: &Path) {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    if !path.exists() {
        let _ = std::fs::OpenOptions::new().append(true).create(true).mode(0o600).open(path);
    }
    for suffix in ["", "-wal", "-shm"] {
        let p = PathBuf::from(format!("{}{}", path.display(), suffix));
        if p.exists() {
            let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
        }
    }
}
#[cfg(not(unix))]
fn restrict_to_owner(_: &Path) {}

impl Database {
    pub fn open(path: &Path, data_dir: &Path) -> anyhow::Result<Self> {
        Self::open_in(path, Some(data_dir))
    }

    /// `data_dir` is where the legacy `auth.json` is imported from; `None` imports nothing.
    fn open_in(path: &Path, data_dir: Option<&Path>) -> anyhow::Result<Self> {
        let conn = if path == Path::new(":memory:") {
            Connection::open_in_memory()?
        } else {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            restrict_to_owner(path);
            Connection::open(path)?
        };
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA busy_timeout = 5000;
             PRAGMA cache_size = -64000;",
        )?;
        // 2.4.1 (#64532): no `PRAGMA wal_checkpoint(PASSIVE)` on open.
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        let db = Database { conn: Arc::new(Mutex::new(conn)), path: path.to_path_buf() };
        // 2.3.1: only a database file the user opens imports their legacy `auth.json`; an in-memory one belongs to
        // embedders and tests, which must not pick up the user's credentials.
        let data_dir = data_dir.filter(|_| path != Path::new(":memory:"));
        crate::migrate::apply(&db, data_dir)?;
        if path != Path::new(":memory:") {
            restrict_to_owner(path);
        }
        Ok(db)
    }

    pub fn memory() -> anyhow::Result<Self> {
        Self::open_in(Path::new(":memory:"), None)
    }

    /// Run `f` with exclusive access to the connection.
    pub fn with<T>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> anyhow::Result<T> {
        let conn = self.conn.lock();
        Ok(f(&conn)?)
    }

    /// `BEGIN DEFERRED` transaction.
    pub fn transaction<T>(&self, f: impl FnOnce(&Connection) -> anyhow::Result<T>) -> anyhow::Result<T> {
        self.tx("BEGIN DEFERRED", f)
    }

    /// `BEGIN IMMEDIATE` transaction (durable event commits).
    pub fn immediate<T>(&self, f: impl FnOnce(&Connection) -> anyhow::Result<T>) -> anyhow::Result<T> {
        self.tx("BEGIN IMMEDIATE", f)
    }

    fn tx<T>(&self, begin: &str, f: impl FnOnce(&Connection) -> anyhow::Result<T>) -> anyhow::Result<T> {
        let conn = self.conn.lock();
        conn.execute_batch(begin)?;
        match f(&conn) {
            Ok(v) => {
                if let Err(e) = conn.execute_batch("COMMIT") {
                    // SQLite keeps the transaction open after some commit failures.
                    let _ = conn.execute_batch("ROLLBACK");
                    return Err(e.into());
                }
                Ok(v)
            }
            Err(e) => {
                // After errors such as SQLITE_FULL, SQLite rolls the whole transaction back itself, so this rollback
                // has nothing to undo and fails: the body's error is reported, never the rollback's.
                let _ = conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }
}
