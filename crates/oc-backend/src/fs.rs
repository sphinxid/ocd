//! `fs.*` (`core/src/filesystem.ts`, `core/src/filesystem/search.ts`).

use std::cmp::Ordering;
use std::path::{MAIN_SEPARATOR, Path, PathBuf};

use oc_git::paths;
use oc_server::{ApiError, FsFile, FsSource};
use serde_json::{Value, json};

use crate::locations::Loc;

/// `FileSystem.read`: resolve a location-relative path, confined to the location.
pub fn read(loc: &Loc, key: &str, input: &str) -> Result<FsFile, ApiError> {
    let missing = || ApiError::file_not_found(input);
    if input.contains('\0') {
        return Err(missing());
    }
    let base = Path::new(key);
    let absolute = paths::resolve(base, input);
    if !paths::contains(base, &absolute) {
        return Err(missing());
    }
    let real = std::fs::canonicalize(&absolute).map_err(|_| missing())?;
    let root = std::fs::canonicalize(base).unwrap_or_else(|_| loc.directory().to_path_buf());
    if !paths::contains(&root, &real) {
        return Err(missing());
    }
    let meta = std::fs::metadata(&real).map_err(|_| missing())?;
    if !meta.is_file() {
        return Err(missing());
    }
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64);
    Ok(FsFile { size: meta.len(), mtime_ms, mime: mime_type(&real), source: FsSource::Path(real) })
}

/// `FSUtil.mimeType` (`mime-types.lookup` or `application/octet-stream`).
pub fn mime_type(p: &Path) -> String {
    // `FSUtil.mimeType`: npm mime-types lookup (util/src/fs-util.ts).
    oc_tools::mime::lookup(p)
}

fn entry(path: String, kind: &str) -> Value {
    json!({"path": path, "type": kind})
}

/// `FileSystem.list`: direct children (files and directories), directories first.
pub fn list(key: &str, input: Option<&str>) -> Result<Vec<Value>, ApiError> {
    let base = Path::new(key);
    let shown = input.unwrap_or(".");
    let directory = paths::resolve(base, shown);
    let meta = std::fs::metadata(&directory).map_err(|_| ApiError::directory_not_found(shown))?;
    if !meta.is_dir() {
        return Err(ApiError::directory_not_found(shown));
    }
    let read = std::fs::read_dir(&directory).map_err(|e| ApiError::defect(e.to_string()))?;
    let mut items: Vec<(String, bool)> = Vec::new();
    for item in read.flatten() {
        let Ok(ft) = item.file_type() else { continue };
        // Dirent types: symlinks are neither files nor directories.
        if !(ft.is_file() || ft.is_dir()) {
            continue;
        }
        let absolute = directory.join(item.file_name());
        let mut relative = paths::relative(base, &absolute);
        if relative.is_empty() {
            relative = ".".into();
        }
        if ft.is_dir() {
            relative.push(MAIN_SEPARATOR);
        }
        items.push((relative, ft.is_dir()));
    }
    items.sort_by(|a, b| match (a.1, b.1) {
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        _ => oc_git::vcs::locale_compare(&a.0, &b.0),
    });
    Ok(items.into_iter().map(|(p, d)| entry(p, if d { "directory" } else { "file" })).collect())
}

/// `FileSystem.write`: absolute or location-relative, creating parents; not confined.
pub fn write(key: &str, path: &str, data: &[u8]) -> Result<Value, ApiError> {
    let target = paths::resolve(Path::new(key), path);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ApiError::defect(e.to_string()))?;
    }
    std::fs::write(&target, data).map_err(|e| ApiError::defect(e.to_string()))?;
    Ok(json!({"path": target.display().to_string()}))
}

/// `rg --files` equivalent: respects ignore files, skips hidden entries and `.git`.
fn scan(root: &Path, limit: usize) -> Vec<String> {
    let mut out = Vec::new();
    let walker = ignore::WalkBuilder::new(root).hidden(true).follow_links(false).build();
    for item in walker.flatten() {
        if !item.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Ok(rel) = item.path().strip_prefix(root) else { continue };
        let rel = rel.to_string_lossy().replace('\\', "/");
        if rel.split('/').any(|p| p == ".git") {
            continue;
        }
        out.push(rel);
        if out.len() >= limit {
            break;
        }
    }
    out
}

/// `FileSystem.find`: fuzzy-ranked files and directories under the location.
pub fn find(root: PathBuf, query: &str, kind: Option<&str>, limit: Option<i64>, vcs: bool) -> Vec<Value> {
    let files = scan(&root, if vcs { usize::MAX } else { 100_000 });
    let mut dirs: indexmap::IndexSet<String> = indexmap::IndexSet::new();
    for f in &files {
        let parts: Vec<&str> = f.split('/').collect();
        for i in 0..parts.len().saturating_sub(1) {
            dirs.insert(format!("{}{}", parts[..=i].join("/"), MAIN_SEPARATOR));
        }
    }
    let items: Vec<&str> = match kind {
        Some("file") => files.iter().map(String::as_str).collect(),
        Some("directory") => dirs.iter().map(String::as_str).collect(),
        _ => files.iter().map(String::as_str).chain(dirs.iter().map(String::as_str)).collect(),
    };
    let limit = limit.unwrap_or(50).max(0) as usize;
    let mut scored: Vec<(f64, &str)> = items.into_iter().filter_map(|t| crate::fuzzy::score(query, t).map(|s| (s, t))).collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(Ordering::Equal).then(a.1.len().cmp(&b.1.len())));
    scored
        .into_iter()
        .take(limit)
        .map(|(_, t)| entry(t.to_string(), if t.ends_with(MAIN_SEPARATOR) { "directory" } else { "file" }))
        .collect()
}
