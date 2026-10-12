//! Migration bootstrap/upgrade (`core/src/database/migration.ts`).

use std::collections::HashSet;
use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension};

use crate::db::{now_ms, Database};

pub const SCHEMA: &str = include_str!("../../../assets/sql/schema.sql");
const IDS: &str = include_str!("../../../assets/sql/migration_ids.txt");
const PREVIOUS_V2_MARKER: &str = "20260730195856_optional_session_title";

pub fn ids() -> Vec<&'static str> {
    IDS.lines().map(str::trim).filter(|l| !l.is_empty()).collect()
}

include!(concat!(env!("OUT_DIR"), "/migrations.rs"));

fn statements(sql: &str) -> Vec<&str> {
    sql.split("\n--> statement-breakpoint\n").map(str::trim).filter(|s| !s.is_empty()).collect()
}

fn tables(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' AND substr(name, 1, 1) <> '_'",
    )?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    rows.collect()
}

pub fn apply(db: &Database, data_dir: &Path) -> anyhow::Result<()> {
    let existing = db.with(|c| tables(c))?;
    if existing.iter().any(|t| t == "session" || t == "session_v2") {
        return apply_only(db, data_dir);
    }
    if !existing.is_empty() {
        anyhow::bail!("Database is not empty and has no session table");
    }
    db.transaction(|c| {
        c.execute_batch(SCHEMA)?;
        c.execute_batch("CREATE TABLE `migration` (id TEXT PRIMARY KEY, time_completed INTEGER NOT NULL)")?;
        let now = now_ms();
        for id in ids() {
            c.execute("INSERT INTO migration (id, time_completed) VALUES (?, ?)", params![id, now])?;
        }
        Ok(())
    })
}

fn completed(c: &Connection) -> rusqlite::Result<HashSet<String>> {
    let mut stmt = c.prepare("SELECT id FROM migration")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    rows.collect()
}

fn apply_only(db: &Database, data_dir: &Path) -> anyhow::Result<()> {
    db.with(|c| c.execute_batch("CREATE TABLE IF NOT EXISTS `migration` (id TEXT PRIMARY KEY, time_completed INTEGER NOT NULL)"))?;
    let mut done = db.with(completed)?;
    if done.is_empty() {
        seed_from_drizzle(db)?;
        done = db.with(completed)?;
    }
    for (id, sql) in MIGRATIONS {
        if done.contains(*id) {
            continue;
        }
        let fk_off = *id == "20260823191254_nullable_workspace_binding";
        if fk_off {
            db.with(|c| c.execute_batch("PRAGMA foreign_keys = OFF"))?;
        }
        let result = db.transaction(|c| {
            run_migration(c, id, sql, data_dir)?;
            c.execute("INSERT INTO migration (id, time_completed) VALUES (?, ?)", params![id, now_ms()])?;
            Ok(())
        });
        if fk_off {
            db.with(|c| c.execute_batch("PRAGMA foreign_keys = ON"))?;
        }
        result?;
        tracing::info!(id = *id, "applied migration");
    }
    Ok(())
}

fn seed_from_drizzle(db: &Database) -> anyhow::Result<()> {
    db.transaction(|c| {
        let exists: bool = c
            .query_row("SELECT 1 FROM sqlite_master WHERE type='table' AND name='__drizzle_migrations'", [], |_| Ok(()))
            .optional()?
            .is_some();
        if !exists {
            return Ok(());
        }
        let has_name = {
            let mut s = c.prepare("PRAGMA table_info(`__drizzle_migrations`)")?;
            let cols = s.query_map([], |r| r.get::<_, String>(1))?.collect::<rusqlite::Result<Vec<_>>>()?;
            cols.iter().any(|n| n == "name")
        };
        let now = now_ms();
        if has_name {
            c.execute(
                "INSERT OR IGNORE INTO migration (id, time_completed) SELECT name, ? FROM __drizzle_migrations WHERE name IS NOT NULL",
                params![now],
            )?;
            return Ok(());
        }
        let mut s = c.prepare(
            "SELECT created_at, strftime('%Y%m%d%H%M%S', created_at / 1000, 'unixepoch') AS prefix FROM __drizzle_migrations WHERE created_at IS NOT NULL",
        )?;
        let rows = s
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (created, prefix) in rows {
            let Some(id) = ids().into_iter().find(|id| id.starts_with(&format!("{prefix}_"))) else {
                anyhow::bail!("Legacy migration timestamp {created} does not match any known migration");
            };
            c.execute("INSERT OR IGNORE INTO migration (id, time_completed) VALUES (?, ?)", params![id, now])?;
        }
        Ok(())
    })
}

fn run_migration(c: &Connection, id: &str, sql: &str, data_dir: &Path) -> anyhow::Result<()> {
    let stmts = statements(sql);
    match id {
        "20260511173437_session-metadata" => {
            let mut s = c.prepare("PRAGMA table_info(`session`)")?;
            let cols = s.query_map([], |r| r.get::<_, String>(1))?.collect::<rusqlite::Result<Vec<_>>>()?;
            if cols.iter().any(|n| n == "metadata") {
                return Ok(());
            }
            for st in stmts {
                c.execute_batch(st)?;
            }
        }
        "20260804233008_loose_psylocke" => {
            let marker = c
                .query_row("SELECT id FROM migration WHERE id = ?", [PREVIOUS_V2_MARKER], |_| Ok(()))
                .optional()?
                .is_some();
            let (alt, main) = stmts.split_at(12);
            if marker {
                let v1_only = c
                    .query_row(
                        "SELECT 1 FROM message WHERE NOT EXISTS (SELECT 1 FROM session_message WHERE session_message.session_id = message.session_id) LIMIT 1",
                        [],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some();
                if v1_only {
                    anyhow::bail!("Previous V2 database contains V1-only session history");
                }
                for st in alt {
                    c.execute_batch(st)?;
                }
            } else {
                for st in main {
                    c.execute_batch(st)?;
                }
            }
        }
        "20260805200742_import_legacy_credentials" => import_legacy_credentials(c, &data_dir.join("auth.json"))?,
        _ => {
            for st in stmts {
                c.execute_batch(st)?;
            }
        }
    }
    Ok(())
}

fn method_id(integration: &str) -> &'static str {
    match integration {
        "openai" => "chatgpt-browser",
        "github-copilot" | "opencode" | "xai" => "device",
        _ => "oauth",
    }
}

pub fn import_legacy_credentials(c: &Connection, file: &Path) -> anyhow::Result<()> {
    use serde_json::{json, Map, Value};
    let Ok(content) = std::fs::read_to_string(file) else { return Ok(()) };
    let input: Option<Value> = serde_json::from_str(&content).ok();
    let Some(Value::Object(input)) = input else {
        anyhow::bail!("Legacy credential file must contain an object");
    };
    let mut origins: Vec<String> = Vec::new();
    for (id, raw) in input {
        let Some(obj) = raw.as_object() else { continue };
        let s = |k: &str| obj.get(k).and_then(Value::as_str).map(str::to_string);
        let kind = s("type").unwrap_or_default();
        let credential = match kind.as_str() {
            "api" => {
                let Some(key) = s("key") else { continue };
                let metadata = obj.get("metadata").filter(|m| {
                    m.as_object().is_some_and(|m| m.values().all(Value::is_string))
                });
                if obj.get("metadata").is_some() && metadata.is_none() {
                    continue;
                }
                let mut v = Map::new();
                v.insert("type".into(), json!("key"));
                v.insert("key".into(), json!(key));
                if let Some(m) = metadata {
                    v.insert("metadata".into(), m.clone());
                }
                Value::Object(v)
            }
            "wellknown" => {
                let (Some(_), Some(token)) = (s("key"), s("token")) else { continue };
                json!({"type": "key", "key": token})
            }
            "oauth" => {
                let (Some(refresh), Some(access)) = (s("refresh"), s("access")) else { continue };
                let Some(expires) = obj.get("expires").and_then(Value::as_u64) else { continue };
                let account = s("accountId").filter(|v| !v.is_empty());
                let enterprise = s("enterpriseUrl").filter(|v| !v.is_empty());
                let integration = id.trim_end_matches('/');
                let mut v = Map::new();
                v.insert("type".into(), json!("oauth"));
                v.insert("methodID".into(), json!(method_id(integration)));
                v.insert("refresh".into(), json!(refresh));
                v.insert("access".into(), json!(access));
                v.insert("expires".into(), json!(expires));
                if account.is_some() || enterprise.is_some() {
                    let mut m = Map::new();
                    if let Some(a) = account {
                        m.insert("accountID".into(), json!(a));
                    }
                    if let Some(e) = enterprise {
                        m.insert("enterpriseUrl".into(), json!(e));
                    }
                    v.insert("metadata".into(), Value::Object(m));
                }
                Value::Object(v)
            }
            _ => continue,
        };
        let integration = id.trim_end_matches('/').to_string();
        if integration.is_empty() {
            continue;
        }
        if kind == "wellknown" {
            origins.push(integration.clone());
        }
        let exists = c
            .query_row("SELECT id FROM credential WHERE integration_id = ?", [&integration], |_| Ok(()))
            .optional()?
            .is_some();
        if exists {
            continue;
        }
        let label = if kind == "oauth" { "OAuth" } else { "API key" };
        let now = now_ms();
        c.execute(
            "INSERT INTO credential (id, integration_id, label, value, time_created, time_updated) VALUES (?, ?, ?, ?, ?, ?)",
            params![oc_schema::id::credential(), integration, label, credential.to_string(), now, now],
        )?;
    }
    if origins.is_empty() {
        return Ok(());
    }
    let stored: Option<String> =
        c.query_row("SELECT value FROM kv WHERE key = 'wellknown:sources'", [], |r| r.get(0)).optional()?;
    let mut current: Vec<String> = stored
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.as_array().cloned())
        .map(|a| a.into_iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    for o in origins {
        if !current.contains(&o) {
            current.push(o);
        }
    }
    let now = now_ms();
    c.execute(
        "INSERT INTO kv (key, value, time_created, time_updated) VALUES ('wellknown:sources', ?, ?, ?) ON CONFLICT (key) DO UPDATE SET value = excluded.value, time_updated = excluded.time_updated",
        params![serde_json::to_string(&current)?, now, now],
    )?;
    Ok(())
}
