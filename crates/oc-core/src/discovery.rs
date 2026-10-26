//! AGENTS.md discovery (`core/config/plugin/instruction.ts`), nested AGENTS.md on read
//! (`core/tool/plugin/read.ts`, `core/session/instructions.ts`), and the skill content block
//! (`core/skill.ts` toModelOutput/prepare).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::instructions::File;

fn resolve(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Directories from `start` up to and including `stop` (nearest first).
pub fn ancestors(start: &Path, stop: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut cur = Some(start);
    while let Some(d) = cur {
        out.push(d.to_path_buf());
        if d == stop {
            break;
        }
        cur = d.parent();
    }
    out
}

pub struct DiscoveryInput<'a> {
    pub directory: &'a Path,
    pub project_directory: &'a Path,
    pub home: &'a Path,
    pub global_config: &'a Path,
    pub project: bool,
    pub global: bool,
    /// Configured `instructions` entries (files, globs, `~/` paths and http(s) URLs), deduplicated in
    /// entry order.
    pub instructions: &'a [String],
    /// The last fetched copy of each configured URL.
    pub remote: &'a HashMap<String, File>,
}

/// How an instruction file read ended (`ConfigInstructionPlugin.read`).
enum ReadResult {
    File(File),
    /// Missing: may be mid-save and is transient.
    Missing,
    /// Denied (e.g. outside a sandbox grant): stays absent.
    Denied,
}

fn read_instruction(path: &Path) -> Result<ReadResult, std::io::Error> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(ReadResult::File(File {
            path: path.display().to_string(),
            content: String::from_utf8_lossy(&bytes).into_owned(),
        })),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ReadResult::Missing),
        Err(e) if oc_util::fs::access_denied(&e) => {
            tracing::debug!(path = %path.display(), reason = "PermissionDenied", "instruction file skipped");
            Ok(ReadResult::Denied)
        }
        Err(e) => Err(e),
    }
}

/// `ConfigInstructionPlugin` sources: the global `AGENTS.md`, project `AGENTS.md` files from the
/// location up to the project root (or home), then configured `instructions`. Returns `None` when
/// the set is unavailable (a discovered project file vanished, or a source failed). Entries with
/// the same path keep their first position (the discovery editor is a `Map`).
pub fn discover(input: &DiscoveryInput) -> Option<Vec<File>> {
    let start = resolve(input.directory);
    let root = resolve(input.project_directory);
    let home = resolve(input.home);
    let project = input.project && start.starts_with(&root);
    let stop = if start.starts_with(&home) { home } else { root.clone() };
    let global_file = resolve(&input.global_config.join("AGENTS.md"));

    let global = || -> Result<Vec<File>, std::io::Error> {
        if !input.global || !global_file.is_file() {
            return Ok(vec![]);
        }
        Ok(match read_instruction(&global_file)? {
            ReadResult::File(f) => vec![f],
            _ => vec![],
        })
    };
    let project_files = || -> Result<Option<Vec<File>>, std::io::Error> {
        if !project {
            return Ok(Some(vec![]));
        }
        let mut seen: Vec<PathBuf> = Vec::new();
        for dir in ancestors(&start, &stop) {
            let candidate = dir.join("AGENTS.md");
            if !candidate.is_file() {
                continue;
            }
            let r = resolve(&candidate);
            if (!input.global && r == global_file) || seen.contains(&r) {
                continue;
            }
            seen.push(r);
        }
        let mut files = Vec::new();
        for p in seen {
            match read_instruction(&p)? {
                ReadResult::File(f) => files.push(f),
                ReadResult::Missing => return Ok(None),
                ReadResult::Denied => {}
            }
        }
        Ok(Some(files))
    };
    let configured = || -> Result<Vec<File>, std::io::Error> {
        let mut files = Vec::new();
        let mut watched: Vec<PathBuf> = Vec::new();
        for source in input.instructions {
            if is_url(source) {
                if let Some(file) = input.remote.get(source) {
                    files.push(file.clone());
                }
                continue;
            }
            let mut matches = match_source(source, input, project, &start, &root);
            matches.sort();
            for item in matches {
                let resolved = resolve(&item);
                if watched.contains(&resolved) {
                    continue;
                }
                watched.push(resolved.clone());
                if let ReadResult::File(f) = read_instruction(&resolved)? {
                    files.push(f);
                }
            }
        }
        Ok(files)
    };
    // `isolate`: a failing source makes the whole set unavailable.
    let isolate = |source: &str, error: std::io::Error| {
        tracing::warn!(source, cause = %error, "failed to load instruction source");
    };
    let global = global().map_err(|e| isolate("global", e)).ok()?;
    let project_files = project_files().map_err(|e| isolate("project", e)).ok()??;
    let configured = configured().map_err(|e| isolate("configured", e)).ok()?;
    let mut out: Vec<File> = Vec::new();
    for file in global.into_iter().chain(project_files).chain(configured) {
        match out.iter_mut().find(|f| f.path == file.path) {
            Some(existing) => *existing = file,
            None => out.push(file),
        }
    }
    Some(out)
}

/// `isUrl`: an http(s) URL.
pub fn is_url(source: &str) -> bool {
    url::Url::parse(source).is_ok_and(|u| u.scheme() == "http" || u.scheme() == "https")
}

/// `ConfigInstructionPlugin.match`: relative patterns search upward from the location to the
/// project root. Without project discovery they resolve against the global config directory instead.
fn match_source(source: &str, input: &DiscoveryInput, project: bool, start: &Path, root: &Path) -> Vec<PathBuf> {
    let expanded = match source.strip_prefix("~/") {
        Some(rest) => input.home.join(rest),
        None => PathBuf::from(source),
    };
    if expanded.is_absolute() {
        let (Some(dir), Some(name)) = (expanded.parent(), expanded.file_name()) else { return vec![] };
        return glob_scan(dir, &name.to_string_lossy());
    }
    let pattern = expanded.to_string_lossy().replace('\\', "/");
    if project {
        // `fs.globUp(pattern, start, root)`.
        let mut out = Vec::new();
        for dir in ancestors(start, root) {
            out.extend(glob_scan(&dir, &pattern));
        }
        return out;
    }
    if !input.global {
        return vec![];
    }
    glob_scan(input.global_config, &pattern)
}

/// `fs.scan(pattern, { cwd, absolute: true, include: "file", dot: true })` (npm `glob`): files
/// (symlinks followed for the match itself, never for `**` descent) matching `pattern` under `cwd`.
pub fn glob_scan(cwd: &Path, pattern: &str) -> Vec<PathBuf> {
    let segments: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty() && *s != ".").collect();
    let mut out = Vec::new();
    if segments.is_empty() {
        return out;
    }
    glob_walk(cwd, &segments, &mut out);
    out.sort();
    out.dedup();
    out
}

fn segment_matcher(segment: &str) -> Option<globset::GlobMatcher> {
    globset::GlobBuilder::new(segment).literal_separator(true).backslash_escape(true).build().ok().map(|g| g.compile_matcher())
}

fn has_magic(segment: &str) -> bool {
    segment.contains(['*', '?', '[', '{'])
}

fn glob_walk(dir: &Path, segments: &[&str], out: &mut Vec<PathBuf>) {
    let Some((first, rest)) = segments.split_first() else { return };
    if *first == "**" {
        // `**` matches zero or more directories; symlinked directories are not followed.
        if rest.is_empty() {
            collect_files(dir, out);
            return;
        }
        glob_walk(dir, rest, out);
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                glob_walk(&entry.path(), segments, out);
            }
        }
        return;
    }
    if *first == ".." {
        if let Some(parent) = dir.parent() {
            glob_walk(parent, rest, out);
        }
        return;
    }
    let candidates: Vec<PathBuf> = if has_magic(first) {
        let Some(matcher) = segment_matcher(first) else { return };
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        entries.flatten().filter(|e| matcher.is_match(e.file_name())).map(|e| e.path()).collect()
    } else {
        vec![dir.join(first)]
    };
    for path in candidates {
        if rest.is_empty() {
            if std::fs::metadata(&path).is_ok_and(|m| m.is_file()) {
                out.push(path);
            }
        } else if std::fs::metadata(&path).is_ok_and(|m| m.is_dir()) {
            glob_walk(&path, rest, out);
        }
    }
}

/// Every file below `dir` (a trailing `**`), without descending into symlinked directories.
fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            collect_files(&path, out);
        } else if std::fs::metadata(&path).is_ok_and(|m| m.is_file()) {
            out.push(path);
        }
    }
}

/// Configured `instructions` entries, deduplicated in entry order.
pub fn configured_sources(entries: &[oc_config::schema::Entry]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for item in entries.iter().filter_map(oc_config::schema::Entry::info).flat_map(|i| i.instructions.iter().flatten()) {
        if !out.contains(item) {
            out.push(item.clone());
        }
    }
    out
}

/// Remote instructions are fetched only when config loads, not on every file rescan (5 s each, 4 at
/// a time). A failed fetch keeps the last good copy (`previous`) or skips the URL, so an unreachable
/// URL never makes the whole instruction set unavailable.
pub async fn fetch_remote(sources: &[String], previous: &HashMap<String, File>) -> HashMap<String, File> {
    use futures::StreamExt;
    static CLIENT: std::sync::LazyLock<reqwest::Client> = std::sync::LazyLock::new(oc_util::http::client);
    let urls: Vec<String> = sources.iter().filter(|s| is_url(s)).cloned().collect();
    let fetched: Vec<Option<File>> = futures::stream::iter(urls)
        .map(|url| async move {
            let request = async {
                let mut request = CLIENT.get(&url);
                for (k, v) in oc_util::trace::headers() {
                    request = request.header(k, v);
                }
                let response = request.send().await.map_err(|e| e.to_string())?;
                if !response.status().is_success() {
                    return Err(format!("StatusCode: non 2xx status code ({} GET {url})", response.status().as_u16()));
                }
                response.text().await.map_err(|e| e.to_string())
            };
            match tokio::time::timeout(std::time::Duration::from_secs(5), request).await {
                Ok(Ok(content)) => Some(File { path: url, content }),
                result => {
                    let cause = match result {
                        Ok(Err(e)) => e,
                        _ => "TimeoutError".to_string(),
                    };
                    tracing::warn!(url = %url, cause = %cause, "failed to fetch instruction url");
                    previous.get(&url).cloned()
                }
            }
        })
        .buffered(4)
        .collect()
        .await;
    fetched.into_iter().flatten().map(|f| (f.path.clone(), f)).collect()
}

/// Nested AGENTS.md for a successful read of `target` (directory listing or file).
pub fn nested_for_read(target: &Path, is_dir: bool, location: &Path) -> Vec<PathBuf> {
    let root = resolve(location);
    let dir = if is_dir { resolve(target) } else { resolve(target.parent().unwrap_or(target)) };
    if !dir.starts_with(&root) {
        return vec![];
    }
    let resolved_target = resolve(target);
    ancestors(&dir, &root)
        .into_iter()
        .filter(|d| !root.starts_with(d))
        .map(|d| d.join("AGENTS.md"))
        .filter(|p| p.is_file() && resolve(p) != resolved_target)
        .collect()
}

/// `SessionInstructions.load` synthetic payload for nested instruction files.
pub fn nested_synthetic(files: &[File], project_root: &Path) -> (String, String, serde_json::Value) {
    let text = files.iter().map(|f| format!("Instructions from: {}\n{}", f.path, f.content)).collect::<Vec<_>>().join("\n\n");
    let rels: Vec<String> = files
        .iter()
        .map(|f| {
            let p = Path::new(&f.path);
            p.strip_prefix(project_root).map(|r| r.display().to_string()).unwrap_or_else(|_| f.path.clone())
        })
        .collect();
    let description = format!("Loaded {}", rels.join(", "));
    let metadata = serde_json::json!({"instruction": {"paths": files.iter().map(|f| f.path.clone()).collect::<Vec<_>>()}});
    (text, description, metadata)
}

/// Skill content block (`Skill.toModelOutput` + `prepare`).
pub fn skill_block(name: &str, skill_path: &Path, content: &str) -> String {
    let directory = skill_path.parent().unwrap_or(Path::new("/"));
    let files: Vec<String> = if skill_path.file_name().is_some_and(|n| n == "SKILL.md") {
        let mut all: Vec<String> = walk_files(directory)
            .into_iter()
            .filter(|f| Path::new(f).file_name().is_some_and(|n| n != "SKILL.md"))
            .collect();
        all.sort();
        all.into_iter().take(10).collect()
    } else {
        vec![]
    };
    let mut lines = vec![
        format!("<skill_content name=\"{name}\">"),
        format!("# Skill: {name}"),
        String::new(),
        content.trim().to_string(),
        String::new(),
        format!("Base directory for this skill: {}", directory.display()),
        "Relative paths in this skill (e.g., scripts/, reference/) are relative to this base directory.".into(),
        "Note: file list is sampled.".into(),
        String::new(),
        "<skill_files>".into(),
    ];
    lines.extend(files.iter().map(|f| format!("<file>{f}</file>")));
    lines.push("</skill_files>".into());
    lines.push("</skill_content>".into());
    lines.join("\n")
}

/// `fs.scan("**/*", { include: "file", dot: true, ignore: FSUtil.ignoreGlobs })`.
fn walk_files(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            match e.file_type() {
                Ok(t) if t.is_dir() => {
                    if !oc_config::discovery::IGNORED_DIRECTORIES.contains(&e.file_name().to_string_lossy().as_ref()) {
                        stack.push(p)
                    }
                }
                Ok(t) if t.is_file() => out.push(p.display().to_string()),
                _ => {}
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn walks_project_agents() {
        let tmp = std::env::temp_dir().join(format!("oc-disc-{}", std::process::id()));
        let sub = tmp.join("repo/pkg");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(tmp.join("repo/AGENTS.md"), "root").unwrap();
        std::fs::write(sub.join("AGENTS.md"), "pkg").unwrap();
        let files = discover(&DiscoveryInput {
            directory: &sub,
            project_directory: &tmp.join("repo"),
            home: Path::new("/nonexistent-home"),
            global_config: Path::new("/nonexistent-config"),
            project: true,
            global: true,
            instructions: &[],
            remote: &HashMap::new(),
        })
        .unwrap();
        assert_eq!(files.iter().map(|f| f.content.as_str()).collect::<Vec<_>>(), vec!["pkg", "root"]);
        let nested = nested_for_read(&sub.join("x.rs"), false, &tmp.join("repo"));
        assert_eq!(nested, vec![resolve(&sub).join("AGENTS.md")]);
        std::fs::remove_dir_all(&tmp).ok();
    }

    fn render(files: &[File]) -> String {
        files.iter().map(|f| format!("Instructions from: {}\n{}", f.path, f.content)).collect::<Vec<_>>().join("\n\n")
    }

    /// A one-route HTTP server answering each request with the next `(status, body)`; records request paths.
    async fn serve(responses: Vec<(u16, &'static str)>) -> (String, std::sync::Arc<parking_lot::Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let seen = requests.clone();
        tokio::spawn(async move {
            let mut responses = responses.into_iter();
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                seen.lock().push(head.split_whitespace().nth(1).unwrap_or_default().to_string());
                let (status, body) = responses.next().unwrap_or((500, ""));
                let reply = format!("HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                let _ = socket.write_all(reply.as_bytes()).await;
            }
        });
        (base, requests)
    }

    // instruction-discovery.test.ts "loads configured files, globs, home paths, and URLs alongside AGENTS.md"
    #[tokio::test]
    async fn loads_configured_files_globs_home_paths_and_urls_alongside_agents_md() {
        let tmp = tempfile::tempdir().unwrap();
        let root = resolve(tmp.path());
        let project = root.join("repo");
        let directory = project.join("packages/core");
        std::fs::create_dir_all(project.join("rules")).unwrap();
        std::fs::create_dir_all(&directory).unwrap();
        let agents = project.join("AGENTS.md");
        let (first, second) = (project.join("rules/a.md"), project.join("rules/b.md"));
        let (absolute, home_file) = (root.join("absolute.md"), root.join("home.md"));
        for (path, content) in [(&agents, "agents"), (&first, "first"), (&second, "second"), (&absolute, "absolute"), (&home_file, "home")] {
            std::fs::write(path, content).unwrap();
        }
        let (base, requests) = serve(vec![(200, "remote"), (404, "")]).await;
        let remote = format!("{base}/instructions.md");
        let instructions = vec![
            "rules/*.md".to_string(),
            absolute.display().to_string(),
            "~/home.md".to_string(),
            remote.clone(),
            "missing.md".to_string(),
            format!("{base}/missing.md"),
        ];
        let fetched = fetch_remote(&instructions, &HashMap::new()).await;
        let input = DiscoveryInput {
            directory: &directory,
            project_directory: &project,
            home: &root,
            global_config: &root.join("global"),
            project: true,
            global: true,
            instructions: &instructions,
            remote: &fetched,
        };
        // The unreachable URL and missing file are skipped without making AGENTS.md unavailable.
        let files = discover(&input).unwrap();
        assert_eq!(
            render(&files),
            [
                format!("Instructions from: {}\nagents", agents.display()),
                format!("Instructions from: {}\nfirst", first.display()),
                format!("Instructions from: {}\nsecond", second.display()),
                format!("Instructions from: {}\nabsolute", absolute.display()),
                format!("Instructions from: {}\nhome", home_file.display()),
                format!("Instructions from: {remote}\nremote"),
            ]
            .join("\n\n")
        );
        std::fs::write(&first, "changed").unwrap();
        let files = discover(&input).unwrap();
        assert_eq!(files[1].content, "changed");
        // File rescans reuse fetched URLs instead of requesting them again.
        assert_eq!(requests.lock().len(), 2);
    }

    // "reloads configured sources on config updates and keeps the last fetched URL"
    #[tokio::test]
    async fn keeps_the_last_fetched_url_when_a_refetch_fails() {
        let (base, _) = serve(vec![(200, "remote"), (500, "")]).await;
        let remote = format!("{base}/instructions.md");
        let sources = vec![remote.clone()];
        let first = fetch_remote(&sources, &HashMap::new()).await;
        assert_eq!(first[&remote].content, "remote");
        let second = fetch_remote(&sources, &first).await;
        assert_eq!(second[&remote].content, "remote");
        // A URL that is no longer configured is dropped.
        assert!(fetch_remote(&[], &second).await.is_empty());
    }

    #[test]
    fn relative_patterns_use_the_global_directory_without_project_discovery() {
        let tmp = tempfile::tempdir().unwrap();
        let root = resolve(tmp.path());
        std::fs::create_dir_all(root.join("global/docs")).unwrap();
        std::fs::write(root.join("global/docs/a.md"), "global").unwrap();
        std::fs::create_dir_all(root.join("project/docs")).unwrap();
        std::fs::write(root.join("project/docs/a.md"), "project").unwrap();
        let instructions = vec!["docs/*.md".to_string()];
        let (project_dir, global_dir) = (root.join("project"), root.join("global"));
        let empty = HashMap::new();
        let input = |project: bool, global: bool| DiscoveryInput {
            directory: &project_dir,
            project_directory: &project_dir,
            home: Path::new("/nonexistent-home"),
            global_config: &global_dir,
            project,
            global,
            instructions: &instructions,
            remote: &empty,
        };
        let contents = |files: Vec<File>| files.into_iter().map(|f| f.content).collect::<Vec<_>>();
        assert_eq!(contents(discover(&input(true, true)).unwrap()), vec!["project"]);
        assert_eq!(contents(discover(&input(false, true)).unwrap()), vec!["global"]);
        // Configured entries are explicit requests: they load with both discovery scopes off.
        assert!(discover(&input(false, false)).unwrap().is_empty());
        let absolute = vec![root.join("global/docs/*.md").display().to_string()];
        let files = discover(&DiscoveryInput { instructions: &absolute, ..input(false, false) }).unwrap();
        assert_eq!(contents(files), vec!["global"]);
    }

    #[test]
    fn glob_scan_matches_like_npm_glob() {
        let tmp = tempfile::tempdir().unwrap();
        let root = resolve(tmp.path());
        for file in ["a.md", ".hidden.md", "b.txt", "docs/c.md", "docs/deep/d.md", ".dot/e.md"] {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "x").unwrap();
        }
        let names = |pattern: &str| {
            glob_scan(&root, pattern)
                .into_iter()
                .map(|p| p.strip_prefix(&root).unwrap().display().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(names("*.md"), vec![".hidden.md", "a.md"]);
        assert_eq!(names("**/*.md"), vec![".dot/e.md", ".hidden.md", "a.md", "docs/c.md", "docs/deep/d.md"]);
        assert_eq!(names("docs/**"), vec!["docs/c.md", "docs/deep/d.md"]);
        assert_eq!(names("{a,b}.*"), vec!["a.md", "b.txt"]);
        assert_eq!(names("docs"), Vec::<String>::new());
    }

    // "skips discovered files a sandbox denies and keeps the rest available"
    #[cfg(unix)]
    #[test]
    fn skips_discovered_files_a_sandbox_denies_and_keeps_the_rest_available() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let root = resolve(tmp.path());
        let packages = root.join("repo/packages");
        std::fs::create_dir_all(&packages).unwrap();
        let denied = root.join("repo/AGENTS.md");
        std::fs::write(&denied, "root").unwrap();
        std::fs::write(packages.join("AGENTS.md"), "package").unwrap();
        std::fs::set_permissions(&denied, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&denied).is_ok() {
            return; // running as root
        }
        let files = discover(&DiscoveryInput {
            directory: &packages,
            project_directory: &root.join("repo"),
            home: Path::new("/nonexistent-home"),
            global_config: Path::new("/nonexistent-config"),
            project: true,
            global: true,
            instructions: &[],
            remote: &HashMap::new(),
        })
        .unwrap();
        assert_eq!(render(&files), format!("Instructions from: {}\npackage", packages.join("AGENTS.md").display()));
    }
}
