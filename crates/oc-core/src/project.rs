//! Project identification (`core/src/project.ts` resolve, `core/src/git.ts`).

use std::path::{Path, PathBuf};
use std::process::Stdio;

use oc_util::hash;

pub const GLOBAL: &str = "global";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Vcs {
    Git { store: PathBuf },
    Hg { store: PathBuf },
}

impl Vcs {
    pub fn kind(&self) -> &'static str {
        match self {
            Vcs::Git { .. } => "git",
            Vcs::Hg { .. } => "hg",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Resolved {
    pub id: String,
    /// Cached legacy id (`<store>/opencode`), if any.
    pub previous: Option<String>,
    /// Worktree root of the directory.
    pub directory: PathBuf,
    /// Main worktree (canonical project directory).
    pub canonical: PathBuf,
    pub vcs: Option<Vcs>,
}

#[derive(Debug, Clone)]
pub struct GitRepo {
    pub git_directory: PathBuf,
    pub common_directory: PathBuf,
    pub worktree: PathBuf,
}

async fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = tokio::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .await
        .ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

pub async fn discover_git(dir: &Path) -> Option<GitRepo> {
    let out = git(dir, &["rev-parse", "--git-dir", "--git-common-dir", "--show-toplevel"]).await?;
    let mut lines = out.lines();
    let abs = |p: &str| {
        let p = PathBuf::from(p.trim());
        let p = if p.is_absolute() { p } else { dir.join(p) };
        std::fs::canonicalize(&p).unwrap_or(p)
    };
    Some(GitRepo { git_directory: abs(lines.next()?), common_directory: abs(lines.next()?), worktree: abs(lines.next()?) })
}

/// Normalize a remote URL into `host/path` (`project.ts` `url` + `parts`).
pub fn normalize_remote(input: &str) -> Option<String> {
    let value = input.trim();
    if value.is_empty() {
        return None;
    }
    let parts = |host: &str, name: &str| -> Option<String> {
        let mut p = name.trim_start_matches('/').to_string();
        if let Some(s) = p.strip_suffix(".git/") {
            p = s.to_string();
        } else if let Some(s) = p.strip_suffix(".git") {
            p = s.to_string();
        }
        let p = p.trim_end_matches('/');
        if host.is_empty() || p.is_empty() {
            return None;
        }
        Some(format!("{}/{}", host.to_lowercase(), p))
    };
    if let Ok(u) = url::Url::parse(value) {
        if u.scheme() == "file" {
            return None;
        }
        return parts(u.host_str().unwrap_or(""), u.path());
    }
    let re = regex::Regex::new(r"^([^@/:]+@)?([^/:]+):(.+)$").unwrap();
    let c = re.captures(value)?;
    parts(&c[2], &c[3])
}

fn cached(store: &Path) -> Option<String> {
    std::fs::read_to_string(store.join("opencode")).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Walk up for the first `.git` or `.hg`.
fn find_vcs(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        for name in [".git", ".hg"] {
            let p = d.join(name);
            if p.exists() {
                return Some(p);
            }
        }
        dir = d.parent();
    }
    None
}

pub async fn resolve(input: &Path) -> Resolved {
    let directory = std::fs::canonicalize(input).unwrap_or_else(|_| input.to_path_buf());
    let native = find_vcs(&directory);
    if let Some(native) = native.as_ref().filter(|p| p.file_name().is_some_and(|n| n == ".git")) {
        if let Some(repo) = discover_git(native.parent().unwrap()).await {
            let previous = cached(&repo.common_directory);
            let remote = git(&repo.worktree, &["remote", "get-url", "origin"])
                .await
                .and_then(|o| normalize_remote(&o))
                .map(|n| hash::fast(format!("git-remote:{n}")));
            let root = async {
                let out = git(&repo.worktree, &["rev-list", "--max-parents=0", "HEAD"]).await?;
                let mut roots: Vec<&str> = out.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
                roots.sort();
                roots.first().map(|s| s.to_string())
            };
            let id = match remote.or_else(|| previous.clone()) {
                Some(id) => Some(id),
                None => root.await,
            };
            let canonical = if repo.git_directory == repo.common_directory {
                repo.worktree.clone()
            } else {
                git(&repo.worktree, &["worktree", "list", "--porcelain"])
                    .await
                    .and_then(|o| o.lines().next().and_then(|l| l.strip_prefix("worktree ")).map(PathBuf::from))
                    .unwrap_or_else(|| repo.worktree.clone())
            };
            return Resolved {
                id: id.unwrap_or_else(|| GLOBAL.into()),
                previous,
                directory: repo.worktree.clone(),
                canonical,
                vcs: Some(Vcs::Git { store: repo.common_directory }),
            };
        }
    }
    if let Some(dot_hg) = native.filter(|p| p.file_name().is_some_and(|n| n == ".hg")) {
        let worktree = dot_hg.parent().unwrap().to_path_buf();
        let previous = cached(&dot_hg);
        let id = match previous.clone() {
            Some(id) => Some(id),
            None => {
                let out = tokio::process::Command::new("hg")
                    .args(["log", "-r", "roots(all())", "-T", "{node}\n"])
                    .current_dir(&worktree)
                    .env("HGPLAIN", "1")
                    .stdin(Stdio::null())
                    .output()
                    .await
                    .ok()
                    .filter(|o| o.status.success());
                out.and_then(|o| {
                    let s = String::from_utf8_lossy(&o.stdout).into_owned();
                    let mut v: Vec<String> = s.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
                    v.sort();
                    v.into_iter().next()
                })
            }
        };
        return Resolved {
            id: id.unwrap_or_else(|| GLOBAL.into()),
            previous,
            canonical: worktree.clone(),
            directory: worktree,
            vcs: Some(Vcs::Hg { store: dot_hg }),
        };
    }
    Resolved {
        id: hash::fast(format!("directory:{}", directory.display())),
        previous: None,
        canonical: directory.clone(),
        directory,
        vcs: None,
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_remote as n;
    #[test]
    fn remotes() {
        assert_eq!(n("https://GitHub.com/a/b.git").as_deref(), Some("github.com/a/b"));
        assert_eq!(n("git@github.com:a/b.git").as_deref(), Some("github.com/a/b"));
        assert_eq!(n("ssh://git@host:22/a/b/").as_deref(), Some("host/a/b"));
        assert_eq!(n("file:///tmp/x"), None);
    }
}

// ---------------------------------------------------------------------------------------------
// Persistence (`project.ts` persist/list/update/activate, `project/sql.ts` upsertProject)

use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};

use crate::bus::Bus;

pub fn project_json(r: &rusqlite::Row) -> rusqlite::Result<Value> {
    let icon_url: Option<String> = r.get("icon_url")?;
    let icon_override: Option<String> = r.get("icon_url_override")?;
    let icon_color: Option<String> = r.get("icon_color")?;
    let mut info = serde_json::Map::new();
    info.insert("id".into(), json!(r.get::<_, String>("id")?));
    info.insert("canonical".into(), json!(r.get::<_, String>("worktree")?));
    if let Some(v) = r.get::<_, Option<String>>("vcs")? {
        info.insert("vcs".into(), json!(v));
    }
    if let Some(v) = r.get::<_, Option<String>>("name")? {
        info.insert("name".into(), json!(v));
    }
    if icon_url.is_some() || icon_override.is_some() || icon_color.is_some() {
        let mut icon = serde_json::Map::new();
        if let Some(v) = icon_url {
            icon.insert("url".into(), json!(v));
        }
        if let Some(v) = icon_override {
            icon.insert("override".into(), json!(v));
        }
        if let Some(v) = icon_color {
            icon.insert("color".into(), json!(v));
        }
        info.insert("icon".into(), Value::Object(icon));
    }
    if let Some(v) = r.get::<_, Option<String>>("commands")? {
        if let Ok(c) = serde_json::from_str::<Value>(&v) {
            info.insert("commands".into(), c);
        }
    }
    let mut time = json!({"created": r.get::<_, i64>("time_created")?, "updated": r.get::<_, i64>("time_updated")?, "active": r.get::<_, i64>("time_active")?});
    if let Some(archived) = r.get::<_, Option<i64>>("time_archived")? {
        time["archived"] = json!(archived);
    }
    info.insert("time".into(), time);
    let sandboxes: String = r.get("sandboxes")?;
    info.insert("sandboxes".into(), serde_json::from_str(&sandboxes).unwrap_or(json!([])));
    Ok(Value::Object(info))
}

pub fn get(bus: &Bus, id: &str) -> anyhow::Result<Option<Value>> {
    bus.db().with(|c| c.query_row("SELECT * FROM project WHERE id = ?", [id], project_json).optional())
}

/// `Project.list`: archived Projects are left out unless `archived` is true.
pub fn list(bus: &Bus, archived: bool) -> anyhow::Result<Vec<Value>> {
    let filter = if archived { "" } else { " WHERE time_archived IS NULL" };
    bus.db().with(|c| {
        let mut s = c.prepare(&format!("SELECT * FROM project{filter} ORDER BY time_active DESC, id ASC"))?;
        let r = s.query_map([], project_json)?.collect();
        r
    })
}

/// How long a Project must be idle (no update, no activity) before the sweep looks at it, and how often the
/// sweep runs (`SWEEP_INTERVAL`).
pub const SWEEP_INTERVAL_MS: i64 = 60 * 60 * 1000;

/// `Project.sweep`: archive idle Projects whose known directories are all gone. When the canonical directory is
/// gone but another main checkout survives, the Project moves there instead; surviving linked worktrees alone
/// leave it untouched. Each change publishes `project.updated`. Unreadable paths count as present.
pub fn sweep(bus: &Bus) -> anyhow::Result<()> {
    let db = bus.db();
    let cutoff = oc_schema::id::now_ms() as i64 - SWEEP_INTERVAL_MS;
    const IDLE: &str = "time_archived IS NULL AND time_updated < ?1 AND time_active < ?1";
    let projects: Vec<(String, String)> = db.with(|c| {
        let mut s = c.prepare(&format!("SELECT id, worktree FROM project WHERE {IDLE} AND id <> ?2"))?;
        s.query_map(params![cutoff, GLOBAL], |r| Ok((r.get(0)?, r.get(1)?)))?.collect()
    })?;
    if projects.is_empty() {
        return Ok(());
    }
    let worktrees: Vec<(String, String, Option<String>)> = db.with(|c| {
        let mut s = c.prepare("SELECT project_id, directory, strategy FROM worktree")?;
        s.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect()
    })?;
    let exists = |directory: &str| Path::new(directory).try_exists().unwrap_or(true);
    for (id, canonical) in projects {
        if exists(&canonical) {
            continue;
        }
        let surviving: Vec<&(String, String, Option<String>)> = worktrees
            .iter()
            .filter(|(project, directory, _)| *project == id && *directory != canonical && exists(directory))
            .collect();
        // Linked worktrees carry a strategy; only another main checkout can become the canonical directory.
        let main = surviving.iter().find(|(_, _, strategy)| strategy.is_none());
        if main.is_none() && !surviving.is_empty() {
            continue;
        }
        let now = oc_schema::id::now_ms() as i64;
        let row = db.with(|c| match main {
            Some((_, directory, _)) => c
                .query_row(
                    &format!("UPDATE project SET worktree = ?3, time_updated = ?4 WHERE id = ?2 AND {IDLE} RETURNING *"),
                    params![cutoff, id, directory, now],
                    project_json,
                )
                .optional(),
            None => c
                .query_row(
                    &format!("UPDATE project SET time_archived = ?3 WHERE id = ?2 AND {IDLE} RETURNING *"),
                    params![cutoff, id, now],
                    project_json,
                )
                .optional(),
        })?;
        if let Some(info) = row {
            bus.publish("project.updated", info, None);
        }
    }
    Ok(())
}

fn contains(parent: &Path, child: &Path) -> bool {
    child.starts_with(parent)
}

/// Resolve + persist (`Project.resolve`).
pub async fn resolve_and_persist(bus: &Bus, input: &Path) -> anyhow::Result<Resolved> {
    let project = resolve(input).await;
    persist(bus, &project)?;
    Ok(project)
}

pub fn persist(bus: &Bus, project: &Resolved) -> anyhow::Result<()> {
    let db = bus.db();
    let now = oc_schema::id::now_ms() as i64;
    let canonical = project.canonical.display().to_string();
    let vcs = project.vcs.as_ref().map(|v| v.kind());
    let previous: Option<String> = db.with(|c| {
        c.query_row("SELECT worktree FROM project WHERE id = ?", [&project.id], |r| r.get(0)).optional()
    })?;
    db.with(|c| match vcs {
        Some(v) => c.execute(
            "INSERT INTO project (id, worktree, vcs, sandboxes, time_created, time_updated) VALUES (?1, ?2, ?3, '[]', ?4, ?4) ON CONFLICT(id) DO UPDATE SET vcs = ?3 WHERE vcs IS NULL OR vcs <> ?3",
            params![project.id, canonical, v, now],
        ),
        None => c.execute(
            "INSERT INTO project (id, worktree, vcs, sandboxes, time_created, time_updated) VALUES (?1, ?2, NULL, '[]', ?3, ?3) ON CONFLICT(id) DO UPDATE SET vcs = NULL WHERE vcs IS NOT NULL",
            params![project.id, canonical, now],
        ),
    })?;
    // Resolving a Project unarchives it (time_updated kept).
    let unarchived = db.with(|c| {
        c.execute("UPDATE project SET time_archived = NULL WHERE id = ? AND time_archived IS NOT NULL", [&project.id])
    })? > 0;
    // Clones share a project ID; only replace a canonical directory that is gone.
    let replaced = match previous.filter(|p| *p != canonical && !Path::new(p).try_exists().unwrap_or(true)) {
        Some(_) => {
            db.with(|c| c.execute("UPDATE project SET worktree = ?, time_updated = ? WHERE id = ?", params![canonical, now, project.id]))?
                > 0
        }
        None => false,
    };
    if (replaced || unarchived)
        && let Some(info) = get(bus, &project.id)?
    {
        bus.publish("project.updated", info, None);
    }
    let Some(vcs) = &project.vcs else { return Ok(()) };
    let mut directories: Vec<(PathBuf, Option<&str>)> = vec![(project.canonical.clone(), None)];
    if project.directory != project.canonical {
        directories.push((project.directory.clone(), matches!(vcs, Vcs::Git { .. }).then_some("git")));
    }
    for (directory, strategy) in directories {
        let dir_s = directory.display().to_string();
        let stored = db.with(|c| {
            c.query_row(
                "SELECT directory FROM worktree WHERE project_id = ? AND directory = ?",
                params![project.id, dir_s],
                |r| r.get::<_, String>(0),
            )
            .optional()
        })?;
        if stored.is_some() {
            continue;
        }
        let resolved = std::fs::canonicalize(&directory).unwrap_or(directory.clone());
        let candidates: Vec<(String, String)> = db.with(|c| {
            let mut s = c.prepare("SELECT id, worktree FROM project WHERE vcs IS NULL AND worktree >= ?1 AND worktree <= ?1 || char(65535)")?;
            let r = s.query_map([resolved.display().to_string()], |r| Ok((r.get(0)?, r.get(1)?)))?.collect();
            r
        })?;
        let adopted: Vec<String> = candidates
            .into_iter()
            .filter(|(id, dir)| {
                id != &project.id
                    && contains(&resolved, Path::new(dir))
                    && find_vcs(Path::new(dir))
                        .and_then(|f| f.parent().map(|p| std::fs::canonicalize(p).unwrap_or(p.to_path_buf())))
                        .is_some_and(|p| p == resolved)
            })
            .map(|(id, _)| id)
            .collect();
        worktree_resolved(bus, &project.id, &dir_s, project.previous.as_deref().unwrap_or(GLOBAL), &adopted, strategy)?;
    }
    Ok(())
}

/// Durable `worktree.resolved` v1 on the project aggregate, with its projector and commit hook.
fn worktree_resolved(
    bus: &Bus,
    project_id: &str,
    directory: &str,
    previous: &str,
    adopted: &[String],
    strategy: Option<&str>,
) -> anyhow::Result<()> {
    let created = oc_schema::id::now_ms();
    let id = oc_schema::id::event();
    let seq = bus.db().immediate(|c| {
        let seq = oc_storage::projector::latest_seq(c, project_id)? + 1;
        let mut candidates: Vec<String> = Vec::new();
        for cand in std::iter::once(previous.to_string()).chain([GLOBAL.to_string()]).chain(adopted.iter().cloned()) {
            if cand != project_id && !candidates.contains(&cand) {
                candidates.push(cand);
            }
        }
        if !candidates.is_empty() {
            let placeholders = vec!["?"; candidates.len()].join(",");
            let sql = format!(
                "SELECT s.id, s.directory, s.project_id, p.worktree FROM session_v2 s JOIN project p ON s.project_id = p.id WHERE s.project_id IN ({placeholders}) AND s.workspace_id IS NULL"
            );
            let mut s = c.prepare(&sql)?;
            let rows: Vec<(String, String, String, String)> = s
                .query_map(rusqlite::params_from_iter(candidates.iter()), |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                .collect::<rusqlite::Result<_>>()?;
            for (sid, dir, pid, canonical) in rows {
                let is_adopted = adopted.contains(&pid);
                if !is_adopted && !(dir.as_str() >= directory && dir.as_str() <= &format!("{directory}\u{ffff}")[..]) {
                    continue;
                }
                let d = if is_adopted { PathBuf::from(canonical) } else { PathBuf::from(&dir) };
                let Ok(rel) = d.strip_prefix(directory) else { continue };
                c.execute(
                    "UPDATE session_v2 SET project_id = ?, path = ? WHERE id = ?",
                    params![project_id, rel.display().to_string().replace('\\', "/"), sid],
                )?;
            }
        }
        c.execute(
            "INSERT INTO worktree (project_id, directory, strategy, time_created) VALUES (?, ?, ?, ?) ON CONFLICT DO NOTHING",
            params![project_id, directory, strategy, created as i64],
        )?;
        oc_storage::projector::store_seq(c, project_id, seq)?;
        Ok(seq)
    })?;
    let mut data = json!({"projectID": project_id, "directory": directory, "previous": previous});
    if !adopted.is_empty() {
        data["adopted"] = json!(adopted);
    }
    bus.broadcast(oc_schema::event::Envelope {
        id,
        created,
        metadata: None,
        kind: "worktree.resolved".into(),
        durable: Some(oc_schema::event::Durable { aggregate_id: project_id.into(), seq, version: 1 }),
        location: None,
        data,
    });
    Ok(())
}
