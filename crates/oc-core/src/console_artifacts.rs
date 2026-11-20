//! The `console` artifact provider (`packages/core/src/plugin/provider/opencode-artifacts.ts`, 2.3.1, #63929):
//! while a location's Console config says the member may share artifacts (`x-opencode-artifacts`), OpenCode Console
//! is the location's default artifact provider. Publishing uploads only the files Console doesn't store yet
//! (`/api/artifacts/prepare`, then `PUT /api/artifacts/blobs/<sha256>`), then publishes them under the artifact's
//! slug (`PUT /api/artifacts/<slug>`), which comes from the title once (`artifact-slug:<id>` in the plugin's
//! storage). While event watches run, publishing names the session, so the artifact's comments wake it.

use std::collections::HashMap;
use std::sync::Arc;

use futures::StreamExt;
use oc_ai::credentials::CredentialValue;
use oc_ai::credentials::flows::console::{credential_headers, server_url};
use oc_ai::credentials::integration::{ConnectionInfo, Integrations};
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use serde_json::{Map, Value, json};
use sha2::Digest;

/// The `GET /api/v2/config` response header Console sends while the caller may share artifacts.
pub const HEADER: &str = "x-opencode-artifacts";
/// The provider's ID.
pub const PROVIDER_ID: &str = "console";
/// The provider's name.
pub const NAME: &str = "OpenCode Console";

/// What a location offers the provider through: the Console connection that may share, and whether event watches
/// run (`wakes`).
#[derive(Clone)]
pub struct Current {
    pub connection: ConnectionInfo,
    pub wakes: bool,
    /// The location's integrations, which resolve (and refresh) the connection's credential.
    pub integrations: Arc<Integrations>,
    pub user_agent: String,
}

static CURRENT: Lazy<Mutex<HashMap<String, Current>>> = Lazy::new(|| Mutex::new(HashMap::new()));

/// `sync`: offers the provider at location `location` through `next`, or removes it when it is `None`. True when
/// that changed the location's providers (the artifact registry reloads).
pub fn sync(location: &str, next: Option<Current>) -> bool {
    let mut current = CURRENT.lock();
    let key = |c: Option<&Current>| (c.map(|c| c.connection.key()), c.map(|c| c.wakes));
    if key(next.as_ref()) == key(current.get(location)) {
        if let Some(next) = next {
            current.insert(location.to_string(), next);
        }
        return false;
    }
    match next {
        Some(next) => current.insert(location.to_string(), next),
        None => current.remove(location),
    };
    true
}

fn current(location: &str) -> Option<Current> {
    CURRENT.lock().get(location).cloned()
}

/// The provider's prompt.
pub fn prompt(wakes: bool) -> String {
    format!(
        "Artifacts are private pages the workspace's members open and comment on inside OpenCode Console. Each needs an index.html.{}",
        if wakes { " Comments wake this session." } else { "" }
    )
}

/// The provider's prompt at location `location`, while it offers the provider.
pub fn offered(location: &str) -> Option<String> {
    current(location).map(|c| prompt(c.wakes))
}

fn slug_key(id: &str) -> String {
    format!("artifact-slug:{id}")
}

/// The plugin storage of `opencode.provider.opencode`.
fn storage_key(key: &str) -> String {
    format!("{}{key}", oc_plugin::storage::namespace(crate::console_config::PLUGIN_ID))
}

/// Console's slug rules: lowercase letters, digits and single dashes, at most 54 characters.
fn slugify(name: &str) -> Option<String> {
    let lower = name.to_lowercase();
    let mut out = String::new();
    for c in lower.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' {
            out.push(c);
        } else if !out.ends_with('-') || out.is_empty() {
            out.push('-');
        }
    }
    let mut collapsed = String::new();
    for c in out.chars() {
        if !(c == '-' && collapsed.ends_with('-')) {
            collapsed.push(c);
        }
    }
    let trimmed: String = collapsed.trim_matches('-').chars().take(54).collect();
    let trimmed = trimmed.trim_end_matches('-');
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// The slug a new artifact publishes under: from the title, with a suffix from the ID, so two artifacts with one
/// title never publish over each other.
pub fn new_slug(title: &str, id: &str) -> String {
    let base = slugify(title).map(|s| s.chars().take(47).collect::<String>().trim_end_matches('-').to_string()).unwrap_or_else(|| "artifact".into());
    let units: Vec<u16> = id.encode_utf16().collect();
    let suffix = String::from_utf16_lossy(&units[units.len().saturating_sub(6)..]).to_lowercase();
    format!("{base}-{suffix}")
}

/// A Console request as the connection's member; a refusal fails in plain words about the artifact `slug`.
struct Sender {
    http: reqwest::Client,
    server: String,
    headers: Vec<(String, String)>,
    user_agent: String,
    slug: String,
}

enum Body {
    None,
    Json(Value),
    Bytes(Vec<u8>),
}

impl Sender {
    async fn new(current: &Current, slug: &str) -> Result<Sender, String> {
        let credential = current.integrations.resolve(&current.connection).await.map_err(|e| e.to_string())?;
        let credential = match credential {
            Some(c) if !matches!(c, CredentialValue::External { .. }) => c,
            _ => return Err("OpenCode Console is not connected".into()),
        };
        Ok(Sender {
            http: oc_util::http::client(),
            server: server_url(&credential),
            headers: credential_headers(&credential),
            user_agent: current.user_agent.clone(),
            slug: slug.to_string(),
        })
    }

    /// Deleting one Console no longer has, such as one deleted in Console, succeeds.
    async fn send(&self, method: reqwest::Method, path: &str, body: Body) -> Result<String, String> {
        let url = format!("{}{path}", self.server);
        let mut request = self.http.request(method.clone(), &url).header("accept", "application/json");
        for (name, value) in &self.headers {
            request = request.header(name, value);
        }
        request = request.header("user-agent", &self.user_agent);
        request = match body {
            Body::None => request,
            Body::Json(value) => request.header("content-type", "application/json").body(value.to_string()),
            Body::Bytes(bytes) => request.header("content-type", "application/octet-stream").body(bytes),
        };
        let transport = |_: reqwest::Error| format!("Transport error ({method} {url})");
        let response = request.send().await.map_err(transport)?;
        let status = response.status().as_u16();
        let text = response.text().await.map_err(transport)?;
        if status < 300 || (method == reqwest::Method::DELETE && status == 404) {
            return Ok(text);
        }
        let problem = serde_json::from_str::<Value>(&text).ok().filter(|body| {
            body.is_object() && body.get("message").is_none_or(Value::is_string) && body.get("owner").is_none_or(Value::is_string)
        });
        let field = |key: &str| problem.as_ref().and_then(|p| p.get(key)).and_then(Value::as_str).map(str::to_string);
        Err(match field("owner") {
            Some(owner) => format!("{owner} already shared an OpenCode Console artifact named {}; give this one another title", self.slug),
            None if status == 404 => "Artifacts are off for this OpenCode Console workspace".to_string(),
            None => field("message").unwrap_or_else(|| format!("OpenCode Console answered {status}")),
        })
    }
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(sha2::Sha256::digest(bytes))
}

/// `title.slice(0, 200).trim()`.
fn short_title(title: &str) -> String {
    let units: Vec<u16> = title.encode_utf16().take(200).collect();
    oc_tools::util::js::trim(&String::from_utf16_lossy(&units)).to_string()
}

/// Uploads only the files Console doesn't store yet, then publishes them as the artifact `slug`; Console's URL.
async fn publish(current: &Current, slug: &str, title: &str, files: &[(String, Vec<u8>)], session_id: Option<&str>, replace: bool) -> Result<String, String> {
    let sender = Sender::new(current, slug).await?;
    let files: Vec<(&str, &[u8], String)> = files.iter().map(|(path, bytes)| (path.as_str(), bytes.as_slice(), sha256(bytes))).collect();
    let mut prepare = Map::new();
    prepare.insert("slug".into(), json!(slug));
    prepare.insert("files".into(), json!(files.iter().map(|(path, bytes, sha)| json!({"path": path, "sha256": sha, "bytes": bytes.len()})).collect::<Vec<_>>()));
    if replace {
        prepare.insert("replace".into(), json!(true));
    }
    let prepared = sender.send(reqwest::Method::POST, "/api/artifacts/prepare", Body::Json(Value::Object(prepare))).await?;
    let missing: Vec<String> = serde_json::from_str::<Value>(&prepared)
        .ok()
        .and_then(|v| v.get("missing")?.as_array()?.iter().map(|s| s.as_str().map(str::to_string)).collect::<Option<Vec<_>>>())
        .ok_or_else(|| "Expected { missing: string[] }".to_string())?;
    // Files with the same bytes upload once.
    let mut uploads: Vec<(String, Vec<u8>)> = Vec::new();
    for (_, bytes, sha) in &files {
        if missing.contains(sha) && !uploads.iter().any(|(s, _)| s == sha) {
            uploads.push((sha.clone(), bytes.to_vec()));
        }
    }
    let sender = &sender;
    let results: Vec<Result<String, String>> = futures::stream::iter(uploads)
        .map(move |(sha, bytes): (String, Vec<u8>)| async move {
            sender.send(reqwest::Method::PUT, &format!("/api/artifacts/blobs/{sha}"), Body::Bytes(bytes)).await
        })
        .buffer_unordered(4)
        .collect()
        .await;
    for result in results {
        result?;
    }
    let mut body = Map::new();
    body.insert("title".into(), json!(short_title(title)));
    body.insert("files".into(), json!(files.iter().map(|(path, _, sha)| json!({"path": path, "sha256": sha})).collect::<Vec<_>>()));
    if replace {
        body.insert("replace".into(), json!(true));
    }
    if current.wakes
        && let Some(session) = session_id
    {
        body.insert("sessionID".into(), json!(session));
    }
    let published = sender.send(reqwest::Method::PUT, &format!("/api/artifacts/{slug}"), Body::Json(Value::Object(body))).await?;
    serde_json::from_str::<Value>(&published)
        .ok()
        .and_then(|v| v.get("url")?.as_str().map(str::to_string))
        .ok_or_else(|| "Expected { url: string }".to_string())
}

fn connected(location: &str) -> Result<Current, String> {
    current(location).ok_or_else(|| "OpenCode Console is not connected".to_string())
}

fn slug_of(db: &oc_storage::Database, id: &str) -> Result<String, String> {
    match db.kv_get(&storage_key(&slug_key(id))).ok().flatten() {
        Some(Value::String(slug)) => Ok(slug),
        _ => Err(format!("No OpenCode Console artifact for {id}")),
    }
}

/// `create`: publishes a new artifact under a new slug; Console's URL.
pub async fn create(
    db: &oc_storage::Database,
    location: &str,
    id: &str,
    title: &str,
    files: &[(String, Vec<u8>)],
    session_id: Option<&str>,
) -> Result<String, String> {
    let current = connected(location)?;
    let slug = new_slug(title, id);
    let url = publish(&current, &slug, title, files, session_id, false).await?;
    db.kv_set(&storage_key(&slug_key(id)), &json!(slug)).map_err(|e| e.to_string())?;
    Ok(url)
}

/// `update`: it's the same artifact, so it shares over the name whoever shared it last.
pub async fn update(
    db: &oc_storage::Database,
    location: &str,
    id: &str,
    title: &str,
    files: &[(String, Vec<u8>)],
    session_id: Option<&str>,
) -> Result<String, String> {
    let current = connected(location)?;
    let slug = slug_of(db, id)?;
    publish(&current, &slug, title, files, session_id, true).await
}

/// `read`: Console has no API that returns an artifact's files.
pub fn read() -> Result<Vec<(String, Vec<u8>)>, String> {
    Err("OpenCode Console can't return an artifact's files yet".into())
}

/// `delete`: removes the artifact from Console and forgets its slug.
pub async fn delete(db: &oc_storage::Database, location: &str, id: &str) -> Result<(), String> {
    let current = connected(location)?;
    let slug = slug_of(db, id)?;
    let sender = Sender::new(&current, &slug).await?;
    sender.send(reqwest::Method::DELETE, &format!("/api/artifacts/{slug}"), Body::None).await?;
    let _ = db.kv_remove(&storage_key(&slug_key(id)));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_follow_consoles_rules() {
        assert_eq!(slugify("Q3 Report!"), Some("q3-report".into()));
        assert_eq!(slugify("  --Hello__World--  "), Some("hello-world".into()));
        assert_eq!(slugify("!!!"), None);
        assert_eq!(new_slug("Q3 Report!", "art_01HZY7ABCDEF"), "q3-report-abcdef");
        assert_eq!(new_slug("???", "art_x1"), "artifact-art_x1");
        let long = "a".repeat(46) + "-b";
        assert_eq!(new_slug(&long, "art_123456"), format!("{}-123456", "a".repeat(46)));
    }
}
