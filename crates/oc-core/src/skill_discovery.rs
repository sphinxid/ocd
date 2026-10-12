//! Remote skill sources (`core/skill/discovery.ts`): `GET {url}/index.json` → `{skills:[{name, version?, files[]}]}`,
//! files downloaded to `{cache}/skills/{hash(base)}/{name}/`.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use parking_lot::Mutex;
use serde::Deserialize;
use tokio::sync::OnceCell;

const SKILL_CONCURRENCY: usize = 4;
const FILE_CONCURRENCY: usize = 8;

#[derive(Deserialize)]
struct IndexSkill {
    name: String,
    version: Option<String>,
    files: Vec<String>,
}

#[derive(Deserialize)]
struct Index {
    skills: Vec<IndexSkill>,
}

fn is_safe_segment(value: &str) -> bool {
    !value.is_empty() && value != "." && value != ".." && !value.contains('/') && !value.contains('\\') && !value.contains('\0')
}

fn is_windows_absolute(value: &str) -> bool {
    let b = value.as_bytes();
    (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':') || value.starts_with("\\\\")
}

pub fn is_safe_relative_path(value: &str) -> bool {
    !value.is_empty()
        && !value.contains('\\')
        && !value.contains('\0')
        && !value.contains('?')
        && !value.contains('#')
        && url::Url::parse(value).is_err()
        && !value.starts_with('/')
        && !is_windows_absolute(value)
        && value.split('/').all(|segment| {
            percent_encoding::percent_decode_str(segment).decode_utf8().is_ok_and(|s| is_safe_segment(&s))
        })
}

/// `path.resolve(root, rel)` lexically (no symlink resolution).
fn resolve(root: &Path, rel: &str) -> PathBuf {
    let mut out = root.to_path_buf();
    for c in Path::new(rel).components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(p) => out.push(p),
            _ => {}
        }
    }
    out
}

fn contains(parent: &Path, child: &Path) -> bool {
    child.starts_with(parent)
}

fn client() -> &'static reqwest::Client {
    static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(reqwest::Client::new);
    &CLIENT
}

/// GET with 2 transient retries (exponential 200 ms, jittered); non-2xx is an error.
async fn get(url: &str, json: bool) -> anyhow::Result<bytes::Bytes> {
    let mut attempt = 0u32;
    loop {
        let mut request = client().get(url);
        if json {
            request = request.header("accept", "application/json");
        }
        let result = match request.send().await {
            Ok(r) if r.status().is_success() => return Ok(r.bytes().await?),
            Ok(r) => anyhow::anyhow!("StatusCode: {}", r.status()),
            Err(e) => e.into(),
        };
        if attempt >= 2 {
            return Err(result);
        }
        let base = 200.0 * 2f64.powi(attempt as i32);
        let jitter = 0.8 + rand::random::<f64>() * 0.4;
        tokio::time::sleep(Duration::from_millis((base * jitter) as u64)).await;
        attempt += 1;
    }
}

async fn download(url: &str, destination: &Path) -> bool {
    if destination.exists() {
        return true;
    }
    let result = async {
        let body = get(url, false).await?;
        if let Some(parent) = destination.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(destination, &body).await?;
        anyhow::Ok(())
    }
    .await;
    match result {
        Ok(()) => true,
        Err(error) => {
            tracing::error!(url, error = %error, "failed to download skill file");
            false
        }
    }
}

fn has_entry(root: &Path, name: &str) -> bool {
    root.join("SKILL.md").exists() || root.join(format!("{name}.md")).exists()
}

/// `SkillDiscovery.pull`: download the source's skills, returning the skill directories that exist.
pub async fn pull(cache: &Path, url: &str) -> Vec<PathBuf> {
    let base = if url.ends_with('/') { url.to_string() } else { format!("{url}/") };
    let Ok(source) = url::Url::parse(&base) else { return Vec::new() };
    let Ok(index_url) = source.join("index.json") else { return Vec::new() };
    let index: Index = match get(index_url.as_str(), true).await.and_then(|b| Ok(serde_json::from_slice(&b)?)) {
        Ok(index) => index,
        Err(error) => {
            tracing::error!(url = %index_url, error = %error, "failed to fetch skill index");
            return Vec::new();
        }
    };
    let source_root = cache.join("skills").join(oc_util::hash::fast(&base));
    struct Plan {
        name: String,
        version: Option<String>,
        root: PathBuf,
        files: Vec<(String, PathBuf, String)>,
    }
    let plans: Vec<Plan> = index
        .skills
        .into_iter()
        .filter_map(|skill| {
            if !is_safe_segment(&skill.name) {
                return None;
            }
            if !skill.files.iter().any(|f| f == "SKILL.md" || *f == format!("{}.md", skill.name)) {
                return None;
            }
            let root = resolve(&source_root, &skill.name);
            if !contains(&source_root, &root) || root == source_root {
                return None;
            }
            let encoded = percent_encoding::utf8_percent_encode(&skill.name, URI_COMPONENT).to_string();
            let skill_url = source.join(&format!("{encoded}/")).ok()?;
            let files = skill
                .files
                .iter()
                .map(|file| {
                    if !is_safe_relative_path(file) {
                        return None;
                    }
                    let resource = skill_url.join(file).ok()?;
                    if resource.origin() != source.origin() {
                        return None;
                    }
                    let destination = resolve(&root, file);
                    if !contains(&root, &destination) || destination == root {
                        return None;
                    }
                    Some((resource.to_string(), destination, file.clone()))
                })
                .collect::<Option<Vec<_>>>()?;
            Some(Plan { name: skill.name, version: skill.version, root, files })
        })
        .collect();

    let limit = Arc::new(tokio::sync::Semaphore::new(SKILL_CONCURRENCY));
    let tasks: Vec<_> = plans
        .into_iter()
        .map(|plan| {
            let limit = limit.clone();
            tokio::spawn(async move {
                let _permit = limit.acquire_owned().await.ok()?;
                let current = match &plan.version {
                    Some(_) => tokio::fs::read_to_string(plan.root.join(".opencode-version")).await.ok(),
                    None => None,
                };
                match &plan.version {
                    Some(version) if current.as_deref() != Some(version) => {
                        refresh(&plan.root, &plan.name, version, &plan.files).await
                    }
                    _ => {
                        let files = plan.files.iter().map(|(url, destination, _)| (url.clone(), destination.clone())).collect();
                        download_all(files).await;
                    }
                }
                has_entry(&plan.root, &plan.name).then_some(plan.root)
            })
        })
        .collect();
    let mut out = Vec::new();
    for task in tasks {
        if let Ok(Some(dir)) = task.await {
            out.push(dir);
        }
    }
    out
}

/// Download `(url, destination)` pairs with at most `FILE_CONCURRENCY` in flight; results in input order.
async fn download_all(files: Vec<(String, PathBuf)>) -> Vec<bool> {
    let limit = Arc::new(tokio::sync::Semaphore::new(FILE_CONCURRENCY));
    let tasks: Vec<_> = files
        .into_iter()
        .map(|(url, destination)| {
            let limit = limit.clone();
            tokio::spawn(async move {
                let _permit = limit.acquire_owned().await;
                download(&url, &destination).await
            })
        })
        .collect();
    let mut out = Vec::new();
    for task in tasks {
        out.push(task.await.unwrap_or(false));
    }
    out
}

/// Versioned atomic refresh: download into a staging directory, then swap it in place of the cached root.
async fn refresh(root: &Path, name: &str, version: &str, files: &[(String, PathBuf, String)]) {
    let token = uuid::Uuid::new_v4();
    let staging = PathBuf::from(format!("{}.tmp-{token}", root.display()));
    let backup = PathBuf::from(format!("{}.old-{token}", root.display()));
    let result = async {
        let downloaded = download_all(files.iter().map(|(url, _, file)| (url.clone(), resolve(&staging, file))).collect()).await;
        if !downloaded.iter().all(|d| *d) || !has_entry(&staging, name) {
            return anyhow::Ok(());
        }
        tokio::fs::write(staging.join(".opencode-version"), version).await?;
        let cached = root.exists();
        if cached {
            tokio::fs::rename(root, &backup).await?;
        }
        if let Err(error) = tokio::fs::rename(&staging, root).await {
            if cached {
                let _ = tokio::fs::rename(&backup, root).await;
            }
            return Err(error.into());
        }
        if cached {
            let _ = tokio::fs::remove_dir_all(&backup).await;
        }
        Ok(())
    }
    .await;
    if let Err(error) = result {
        tracing::error!(skill = name, error = %error, "failed to refresh skill");
    }
    let _ = tokio::fs::remove_dir_all(&staging).await;
}

/// `encodeURIComponent` set.
const URI_COMPONENT: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'!')
    .remove(b'~')
    .remove(b'*')
    .remove(b'\'')
    .remove(b'(')
    .remove(b')');

type Pulled = Arc<OnceCell<Vec<PathBuf>>>;
static PULLED: LazyLock<Mutex<HashMap<String, Pulled>>> = LazyLock::new(Default::default);

/// `pull` once per URL per process (sessions re-select every step; the network fetch happens on first use).
pub async fn pull_cached(cache: &Path, url: &str) -> Vec<PathBuf> {
    let cell = PULLED.lock().entry(url.to_string()).or_default().clone();
    cell.get_or_init(|| pull(cache, url)).await.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_path_safety() {
        assert!(is_safe_relative_path("SKILL.md"));
        assert!(is_safe_relative_path("scripts/run.sh"));
        assert!(!is_safe_relative_path("../x"));
        assert!(!is_safe_relative_path("a/%2e%2e/b"));
        assert!(!is_safe_relative_path("/etc/passwd"));
        assert!(!is_safe_relative_path("C:/x"));
        assert!(!is_safe_relative_path("https://evil/x"));
        assert!(!is_safe_relative_path("a?b"));
        assert!(!is_safe_relative_path("a//b"));
    }

    #[tokio::test]
    async fn pulls_index() {
        use axum::{routing::get, Router};
        let app = Router::new()
            .route(
                "/s/index.json",
                get(|| async {
                    r#"{"skills":[{"name":"demo","version":"1","files":["SKILL.md","ref/a.txt"]},{"name":"bad","files":["x.md"]},{"name":"evil","files":["SKILL.md","../x"]}]}"#
                }),
            )
            .route("/s/demo/SKILL.md", get(|| async { "---\nname: demo\ndescription: d\n---\nbody" }))
            .route("/s/demo/ref/a.txt", get(|| async { "a" }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let cache = tempfile::tempdir().unwrap();
        let dirs = pull(cache.path(), &format!("http://{addr}/s")).await;
        assert_eq!(dirs.len(), 1);
        assert!(dirs[0].ends_with("demo"));
        assert_eq!(std::fs::read_to_string(dirs[0].join(".opencode-version")).unwrap(), "1");
        assert_eq!(std::fs::read_to_string(dirs[0].join("ref/a.txt")).unwrap(), "a");
        // Same version: cached files are reused.
        assert_eq!(pull(cache.path(), &format!("http://{addr}/s")).await, dirs);
    }
}
