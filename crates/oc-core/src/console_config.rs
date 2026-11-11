//! The OpenCode Console workspace configuration (`packages/core/src/plugin/provider/opencode.ts` `load`,
//! `publish`, `fetchConfig`, the web search registration and the polling): while a location is signed in to
//! OpenCode Console, the Console's `GET /api/v2/config` (providers, remote MCP servers, policies and the web search
//! selection) merges as an integration config document, loaded right after managed policies.
//!
//! With a config, the workspace's instructions (`GET /api/v2/instructions`) join the discovered instruction files
//! as `OpenCode Console instruction <id>`, and its skills (`GET /api/v2/skills`, each downloaded from
//! `/api/v2/skills/<slug>` through the redirects Console answers with) join the skills as `skill://<slug>/SKILL.md`.
//! Every location signed in to the same workspace shares one fetch of each through the [`SharedCache`], reused for
//! a minute; each list keeps its last copy through a failure, so a failing list never holds back the config and its
//! policies. Each request times out after 5 seconds.

use crate::shared_cache::{self, SharedCache};
use crate::usage_limits::Http;
use indexmap::IndexMap;
use oc_ai::credentials::flows::console::{credential_headers, server_url, SIGNED_OUT_MESSAGE};
use oc_ai::credentials::integration::{ConnectionInfo, ConnectionStatus, IntegrationError, Integrations};
use oc_ai::credentials::CredentialValue;
use oc_config::schema::{Entry, Info};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// The integration of the OpenCode Console sign-in.
pub const INTEGRATION_ID: &str = "opencode";
/// The built-in plugin that loads it.
pub const PLUGIN_ID: &str = "opencode.provider.opencode";
/// The Console web search provider id.
pub const WEBSEARCH_ID: &str = "opencode";
/// How often a signed-in location checks the Console config again.
pub const POLL: Duration = Duration::from_secs(60);
/// How long one fetch of the workspace's config and lists is shared (`load(maxAge)`).
pub const MAX_AGE: Duration = Duration::from_secs(60);
/// The limit on each Console config or list request.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// The limit on one skill download.
pub const SKILL_TIMEOUT: Duration = Duration::from_secs(30);

const SIGNED_OUT: &str = "Reconnect OpenCode Console to continue";

fn sso_message(organization: Option<&str>) -> String {
    format!("Sign in with SSO again to use {}", organization.unwrap_or("your OpenCode Console organization"))
}

/// One of the workspace's instructions.
#[derive(Debug, Clone, PartialEq)]
pub struct Instruction {
    pub id: String,
    pub content: String,
}

/// One of the workspace's skills, by version (its files download separately).
#[derive(Debug, Clone, PartialEq)]
pub struct SkillRef {
    pub slug: String,
    pub version: String,
}

/// What the plugin last loaded for a location.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    /// The Console's config (`ConfigIntegration.Remote`), when it has one.
    pub remote: Option<Info>,
    /// The workspace's instructions (with a config only).
    pub instructions: Vec<Instruction>,
    /// The workspace's skills (with a config only).
    pub skills: Vec<SkillRef>,
    /// The active OpenCode Console connection.
    pub connection: Option<ConnectionInfo>,
    /// The organization behind the connection (`metadata.orgName`).
    pub organization: Option<String>,
    /// The account `integration.use` names (`ManagedPolicy.account` of the stored credential).
    pub account: Option<String>,
}

impl Snapshot {
    /// `owner`: the account and workspace downloaded skills belong to, so a switch hides them right away.
    pub fn owner(&self) -> String {
        format!("{}:{}", self.connection.as_ref().map(ConnectionInfo::key).unwrap_or_default(), self.organization.clone().unwrap_or_default())
    }

    /// Whether the workspace's instructions and skills apply at a location with config `entries`: the location
    /// holds the Console document unchanged. Managed `integration.use: deny` makes config layer a policies-only
    /// copy, which withholds instructions and skills too.
    pub fn allowed(&self, entries: &[Entry]) -> bool {
        self.document().is_some_and(|document| entries.contains(&document))
    }

    /// The workspace's instructions as discovered instruction files.
    pub fn instruction_files(&self) -> Vec<crate::instructions::File> {
        self.instructions
            .iter()
            .map(|i| crate::instructions::File { path: format!("OpenCode Console instruction {}", i.id), content: i.content.clone() })
            .collect()
    }
    /// The Console selected its hosted search and the location is signed in (`websearch.transform`).
    pub fn offers_websearch(&self) -> bool {
        self.connection.is_some()
            && matches!(
                self.remote.as_ref().and_then(|r| r.websearch.as_ref()),
                Some(oc_config::schema::WebSearch::Provider(id)) if id == WEBSEARCH_ID
            )
    }

    /// The integration document the snapshot contributes to config.
    pub fn document(&self) -> Option<Entry> {
        let (remote, connection) = (self.remote.clone()?, self.connection.as_ref()?);
        let mut document =
            oc_config::integration::make(INTEGRATION_ID, connection_json(connection), self.organization.clone(), remote);
        if let Entry::Document { integration: Some(source), .. } = &mut document {
            source.account = self.account.clone();
        }
        Some(document)
    }
}

/// `Connection.Info` without its status.
fn connection_json(connection: &ConnectionInfo) -> Value {
    match connection {
        ConnectionInfo::Credential { id, label, method, .. } => json!({"type": "credential", "id": id, "label": label, "method": method}),
        ConnectionInfo::Env { name, .. } => json!({"type": "env", "name": name}),
    }
}

/// Integration documents by location key (`Config.integration`), read by every config load of the location.
static DOCUMENTS: once_cell::sync::Lazy<Mutex<HashMap<String, Entry>>> = once_cell::sync::Lazy::new(|| Mutex::new(HashMap::new()));
/// The last snapshot by location key.
static SNAPSHOTS: once_cell::sync::Lazy<Mutex<HashMap<String, Snapshot>>> = once_cell::sync::Lazy::new(|| Mutex::new(HashMap::new()));

/// The integration documents of location `key`, in load order.
pub fn documents(key: &str) -> Vec<Entry> {
    DOCUMENTS.lock().get(key).cloned().into_iter().collect()
}

/// The last loaded snapshot of location `key`.
pub fn snapshot(key: &str) -> Snapshot {
    SNAPSHOTS.lock().get(key).cloned().unwrap_or_default()
}

/// Store a snapshot and its document; true when the document changed (config must reload).
pub fn publish(key: &str, next: Snapshot) -> bool {
    let document = next.document();
    SNAPSHOTS.lock().insert(key.to_string(), next);
    let mut documents = DOCUMENTS.lock();
    if documents.get(key) == document.as_ref() {
        return false;
    }
    match document {
        Some(document) => documents.insert(key.to_string(), document),
        None => documents.remove(key),
    };
    true
}

/// Forget a closed location.
pub fn forget(key: &str) {
    DOCUMENTS.lock().remove(key);
    SNAPSHOTS.lock().remove(key);
    SKILLS.lock().remove(key);
}

/// The skills downloaded for a location, tagged with the [`Snapshot::owner`] they were downloaded for.
#[derive(Debug, Clone, Default)]
pub struct ConsoleSkills {
    pub owner: String,
    pub items: Vec<ConsoleSkill>,
}

/// A downloaded skill and its other files (relative path → content).
#[derive(Debug, Clone, PartialEq)]
pub struct ConsoleSkill {
    pub skill: oc_config::sources::Skill,
    pub files: IndexMap<String, String>,
}

static SKILLS: once_cell::sync::Lazy<Mutex<HashMap<String, ConsoleSkills>>> = once_cell::sync::Lazy::new(|| Mutex::new(HashMap::new()));

/// The Console skills of location `key` for its current snapshot (`skills.transform`).
pub fn skills(key: &str) -> Vec<ConsoleSkill> {
    let owner = snapshot(key).owner();
    SKILLS.lock().get(key).filter(|s| s.owner == owner).map(|s| s.items.clone()).unwrap_or_default()
}

/// The files of Console skill `id` at location `key` (`Skill.files`), for `skill://` reads.
pub fn skill_files(key: &str, id: &str) -> Option<IndexMap<String, String>> {
    skills(key).into_iter().find(|s| s.skill.id == id).map(|s| s.files).filter(|f| !f.is_empty())
}

/// Replaces location `key`'s skills; true when they changed.
pub fn set_skills(key: &str, owner: String, items: Vec<ConsoleSkill>) -> bool {
    let mut skills = SKILLS.lock();
    let changed = skills.get(key).is_none_or(|s| s.owner != owner || s.items != items);
    skills.insert(key.to_string(), ConsoleSkills { owner, items });
    changed
}

/// Hides location `key`'s skills (not allowed, or signed out).
pub fn clear_skills(key: &str) -> bool {
    SKILLS.lock().remove(key).is_some_and(|s| !s.items.is_empty())
}

/// Why the Console config could not be loaded.
#[derive(Debug, Clone, PartialEq)]
pub enum LoadError {
    /// The Console session or refresh token was revoked or expired; only a new device login recovers.
    SignedOut,
    /// The organization enforces SSO and the user's SSO proof is missing or expired.
    SsoRequired { organization: Option<String>, url: String },
    Other(String),
    /// A failure worth retrying: the network, a timeout, or HTTP 408, 429, 500, 502, 503 or 504.
    Transient(String),
}

impl LoadError {
    /// The connection status a well-formed Console error reports (`connectionStatus`); anything else may be
    /// transient and changes nothing.
    fn status(&self) -> Option<ConnectionStatus> {
        match self {
            LoadError::SignedOut => Some(ConnectionStatus::needs_auth(SIGNED_OUT, None)),
            LoadError::SsoRequired { organization, url } => {
                Some(ConnectionStatus::needs_auth(sso_message(organization.as_deref()), Some(url.clone())))
            }
            LoadError::Other(_) | LoadError::Transient(_) => None,
        }
    }
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::SignedOut => write!(f, "{SIGNED_OUT}."),
            LoadError::SsoRequired { organization, .. } => write!(f, "{}.", sso_message(organization.as_deref())),
            LoadError::Other(message) | LoadError::Transient(message) => f.write_str(message),
        }
    }
}

/// `normalizeServer`: origin + pathname without trailing slashes; HTTP(S) only.
fn normalize_server(input: &str) -> Result<url::Url, String> {
    let url = url::Url::parse(input).map_err(|_| "Invalid OpenCode server URL: Invalid URL".to_string())?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err("Invalid OpenCode server URL: expected HTTP(S)".into());
    }
    Ok(url)
}

/// `ssoUrl(server, connectionID)`: Console only redirects to paths under its own base path after SSO.
pub fn sso_url(server: &str, connection_id: &str) -> Result<String, String> {
    let url = normalize_server(server)?;
    let path = url.path().trim_end_matches('/');
    let base = format!("{}{path}", url.origin().ascii_serialization());
    let redirect = format!("{path}/");
    let encode = |s: &str| percent_encode(s);
    Ok(format!("{base}/auth/sso/{}/start?redirectTo={}", encode(connection_id), encode(&redirect)))
}

/// `encodeURIComponent`.
fn percent_encode(input: &str) -> String {
    let mut out = String::new();
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// `fetchConfig`: the Console's config, `None` when it has none (404). A Console that accepts the connection but
/// never answers would otherwise hold the load, including the one at startup, so it times out after 5 seconds.
pub async fn fetch_config(
    http: &dyn crate::usage_limits::Http,
    user_agent: &str,
    credential: &CredentialValue,
) -> Result<Option<Info>, LoadError> {
    match tokio::time::timeout(REQUEST_TIMEOUT, fetch_config_now(http, user_agent, credential)).await {
        Ok(result) => result,
        Err(_) => Err(LoadError::Transient("TimeoutError".into())),
    }
}

/// A non-2xx answer's error, transient for 408, 429 and the 5xx statuses Effect's `retryTransient` retries.
fn status_failure(status: u16, url: &str) -> LoadError {
    let message = format!("StatusCode: non 2xx status code ({status} GET {url})");
    if shared_cache::transient_status(status) { LoadError::Transient(message) } else { LoadError::Other(message) }
}

async fn fetch_config_now(http: &dyn crate::usage_limits::Http, user_agent: &str, credential: &CredentialValue) -> Result<Option<Info>, LoadError> {
    let server = server_url(credential);
    let url = format!("{server}/api/v2/config");
    let mut headers = vec![("user-agent".to_string(), user_agent.to_string()), ("accept".to_string(), "application/json".to_string())];
    headers.extend(credential_headers(credential));
    let response = http.get(&url, &headers).await.map_err(LoadError::Transient)?;
    let body = || serde_json::from_str::<Value>(&response.body).ok();
    match response.status {
        404 => return Ok(None),
        401 if body().and_then(|b| b.get("_tag").cloned()) == Some(json!("Unauthorized")) => return Err(LoadError::SignedOut),
        403 => {
            if let Some(b) = body()
                && b.get("_tag") == Some(&json!("SsoRequired"))
                && b.get("orgId").is_some_and(Value::is_string)
                && let Some(connection) = b.get("connectionId").and_then(Value::as_str)
            {
                let organization = credential.metadata().and_then(|m| m.get("orgName")).and_then(Value::as_str).map(String::from);
                let url = sso_url(&server, connection).map_err(LoadError::Other)?;
                return Err(LoadError::SsoRequired { organization, url });
            }
        }
        _ => {}
    }
    if !(200..300).contains(&response.status) {
        return Err(status_failure(response.status, &url));
    }
    let body: Value = serde_json::from_str(&response.body).map_err(|e| LoadError::Other(e.to_string()))?;
    oc_config::integration::decode_remote(&body).map(Some).map_err(LoadError::Other)
}

/// Whether a 403 body is Console's SSO error (`SsoRequired`).
fn sso_required(body: &str) -> bool {
    serde_json::from_str::<Value>(body).ok().is_some_and(|b| {
        b.get("_tag") == Some(&json!("SsoRequired")) && b.get("orgId").is_some_and(Value::is_string) && b.get("connectionId").is_some_and(Value::is_string)
    })
}

/// `fetchList`: a 404 (none in the workspace) or a 403 other than SSO (the sign-in may not read them) is an empty
/// list. Any other error, including a malformed body, fails, so the caller keeps the last list.
async fn fetch_list<T>(
    http: &dyn Http,
    user_agent: &str,
    credential: &CredentialValue,
    path: &str,
    decode: impl Fn(&Value) -> Option<Vec<T>>,
) -> Result<Vec<T>, LoadError> {
    let url = format!("{}{path}", server_url(credential));
    let mut headers = vec![("user-agent".to_string(), user_agent.to_string()), ("accept".to_string(), "application/json".to_string())];
    headers.extend(credential_headers(credential));
    let request = async {
        let response = http.get(&url, &headers).await.map_err(LoadError::Transient)?;
        if response.status == 404 || (response.status == 403 && !sso_required(&response.body)) {
            return Ok(vec![]);
        }
        if !(200..300).contains(&response.status) {
            return Err(status_failure(response.status, &url));
        }
        let body: Value = serde_json::from_str(&response.body).map_err(|e| LoadError::Other(e.to_string()))?;
        decode(&body).ok_or_else(|| LoadError::Other(format!("Invalid {path} response")))
    };
    tokio::time::timeout(REQUEST_TIMEOUT, request).await.unwrap_or_else(|_| Err(LoadError::Transient("TimeoutError".into())))
}

/// `{instructions: [{id, content}]}`.
fn decode_instructions(body: &Value) -> Option<Vec<Instruction>> {
    body.get("instructions")?
        .as_array()?
        .iter()
        .map(|i| Some(Instruction { id: i.get("id")?.as_str()?.to_string(), content: i.get("content")?.as_str()?.to_string() }))
        .collect()
}

/// `{skills: [{slug, version}]}`.
fn decode_skills(body: &Value) -> Option<Vec<SkillRef>> {
    body.get("skills")?
        .as_array()?
        .iter()
        .map(|s| Some(SkillRef { slug: s.get("slug")?.as_str()?.to_string(), version: s.get("version")?.as_str()?.to_string() }))
        .collect()
}

/// `workspaceKey`: API keys carry no account or org, so the sign-in itself identifies the workspace.
pub fn workspace_key(connection: &ConnectionInfo, credential: &CredentialValue) -> String {
    let org = credential.metadata().and_then(|m| m.get("orgID")).cloned().unwrap_or(Value::Null);
    json!([server_url(credential), connection.key(), org]).to_string()
}

/// A load error as the shared cache keeps it.
fn shared(error: LoadError) -> shared_cache::LoadError {
    let transient = matches!(error, LoadError::Transient(_));
    shared_cache::LoadError::new(error.to_string(), transient)
}

/// What one fetch of the config shares: the config, or Console's answer for this sign-in. A sign-in error is
/// shared like a config rather than replaced by the last config, as a network failure is.
type ConfigAnswer = Result<Option<Info>, LoadError>;

/// The workspace's config through the cache, then its instructions and skills together (each its own entry).
async fn workspace(
    cache: &Arc<SharedCache>,
    max_age: Duration,
    http: Arc<dyn Http>,
    user_agent: &str,
    connection: &ConnectionInfo,
    credential: &CredentialValue,
) -> Result<(Option<Info>, Vec<Instruction>, Vec<SkillRef>), LoadError> {
    let workspace = workspace_key(connection, credential);
    let answer = {
        let (http, user_agent, credential) = (http.clone(), user_agent.to_string(), credential.clone());
        cache
            .get(&format!("opencode-console:{workspace}"), max_age, false, move || {
                let (http, user_agent, credential) = (http.clone(), user_agent.clone(), credential.clone());
                async move {
                    match fetch_config(&*http, &user_agent, &credential).await {
                        Ok(remote) => Ok::<ConfigAnswer, _>(Ok(remote)),
                        Err(error) if error.status().is_some() => Ok(Err(error)),
                        Err(error) => Err(shared(error)),
                    }
                }
            })
            .await
            .map_err(|e| LoadError::Other(e.message))?
    };
    let Some(remote) = answer? else { return Ok((None, vec![], vec![])) };
    let list = |name: &str| (cache.clone(), format!("opencode-console-{name}:{workspace}"), http.clone(), user_agent.to_string(), credential.clone());
    let (cache_a, key_a, http_a, ua_a, credential_a) = list("instructions");
    let (cache_b, key_b, http_b, ua_b, credential_b) = list("skills");
    let (instructions, skills) = tokio::join!(
        async move {
            cache_a
                .get(&key_a, max_age, false, move || {
                    let (http, user_agent, credential) = (http_a.clone(), ua_a.clone(), credential_a.clone());
                    async move { fetch_list(&*http, &user_agent, &credential, "/api/v2/instructions", decode_instructions).await.map_err(shared) }
                })
                .await
                .unwrap_or_default()
        },
        async move {
            cache_b
                .get(&key_b, max_age, false, move || {
                    let (http, user_agent, credential) = (http_b.clone(), ua_b.clone(), credential_b.clone());
                    async move { fetch_list(&*http, &user_agent, &credential, "/api/v2/skills", decode_skills).await.map_err(shared) }
                })
                .await
                .unwrap_or_default()
        },
    );
    Ok((Some(remote), instructions, skills))
}

/// `loadSkills`: downloads the listed skills (at most 8 at a time), each shared by every location through the cache
/// until its version changes. A failed download keeps the last copy that loaded, or skips the skill; `None` when the
/// connection's credential cannot be resolved (the current skills stay).
pub async fn load_skills(integrations: &Integrations, connection: &ConnectionInfo, listed: &[SkillRef]) -> Option<Vec<ConsoleSkill>> {
    load_skills_with(&shared_cache::GLOBAL, integrations, connection, listed).await
}

/// [`load_skills`] through `cache`.
pub async fn load_skills_with(cache: &Arc<SharedCache>, integrations: &Integrations, connection: &ConnectionInfo, listed: &[SkillRef]) -> Option<Vec<ConsoleSkill>> {
    use futures::StreamExt;
    if listed.is_empty() {
        return Some(vec![]);
    }
    let credential = match integrations.resolve(connection).await {
        Ok(Some(c)) if !matches!(c, CredentialValue::External { .. }) => c,
        Ok(_) => return Some(vec![]),
        Err(cause) => {
            tracing::warn!(%cause, "failed to load Console skills");
            return None;
        }
    };
    let server = match oc_tools::websearch::normalize_server(&server_url(&credential)) {
        Ok(server) => server,
        Err(cause) => {
            tracing::warn!(%cause, "failed to load Console skills");
            return None;
        }
    };
    let workspace = workspace_key(connection, &credential);
    let headers = credential_headers(&credential);
    let loaded: Vec<Option<ConsoleSkill>> = futures::stream::iter(listed.iter().cloned())
        .map(|item| {
            let (cache, server, headers, key) = (cache.clone(), server.clone(), headers.clone(), format!("opencode-console-skill:{workspace}:{}", item.slug));
            async move {
                let get = |force: bool| {
                    let (server, headers, slug, version) = (server.clone(), headers.clone(), item.slug.clone(), item.version.clone());
                    let cache = cache.clone();
                    let key = key.clone();
                    async move {
                        cache
                            .get(&key, shared_cache::FOREVER, force, move || {
                                let (server, headers, slug, version) = (server.clone(), headers.clone(), slug.clone(), version.clone());
                                async move { download_skill(&server, &slug, &headers).await.map(|skill| (version, skill)).map_err(|e| shared_cache::LoadError::new(e, false)) }
                            })
                            .await
                    }
                };
                // Any other version, or a failure, downloads again.
                let result = match get(false).await {
                    Ok((version, skill)) if version == item.version => Ok(skill),
                    _ => get(true).await.map(|(_, skill)| skill),
                };
                match result {
                    Ok(skill) => Some(skill),
                    Err(cause) => {
                        tracing::warn!(slug = %item.slug, cause = %cause, "failed to load Console skill");
                        None
                    }
                }
            }
        })
        .buffered(8)
        .collect()
        .await;
    Some(loaded.into_iter().flatten().collect())
}

/// A client that leaves redirects to [`oc_util::http::follow`].
static NO_REDIRECTS: once_cell::sync::Lazy<reqwest::Client> = once_cell::sync::Lazy::new(|| {
    oc_util::http::client_builder().redirect(reqwest::redirect::Policy::none()).build().unwrap_or_default()
});

/// `downloadSkill`: the skill bundle (through the presigned-storage redirect Console answers with, 30 s), its slug,
/// safe file paths and a `SKILL.md` that parses.
async fn download_skill(server: &str, slug: &str, credential: &[(String, String)]) -> Result<ConsoleSkill, String> {
    if !crate::skill_discovery::is_safe_segment(slug) {
        return Err(format!("unsafe skill slug: {slug}"));
    }
    let url = format!("{server}/api/v2/skills/{}", percent_encoding::utf8_percent_encode(slug, URI_COMPONENT));
    let mut headers = vec![("accept".to_string(), "application/json".to_string())];
    headers.extend(credential.iter().cloned());
    let request = async {
        let response = oc_util::http::follow(&NO_REDIRECTS, reqwest::Method::GET, &url, headers, None).await.map_err(|e| e.to_string())?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(format!("StatusCode: non 2xx status code ({status} GET {})", response.url()));
        }
        response.json::<Value>().await.map_err(|e| e.to_string())
    };
    let bundle = tokio::time::timeout(SKILL_TIMEOUT, request).await.map_err(|_| "TimeoutError".to_string())??;
    let invalid = || "Expected { slug, files: [{ path, content }] }".to_string();
    let received = bundle.get("slug").and_then(Value::as_str).ok_or_else(invalid)?;
    let files: Vec<(String, String)> = bundle
        .get("files")
        .and_then(Value::as_array)
        .ok_or_else(invalid)?
        .iter()
        .map(|f| Some((f.get("path")?.as_str()?.to_string(), f.get("content")?.as_str()?.to_string())))
        .collect::<Option<_>>()
        .ok_or_else(invalid)?;
    if received != slug {
        return Err(format!("expected skill {slug}, received {received}"));
    }
    if let Some((path, _)) = files.iter().find(|(path, _)| !crate::skill_discovery::is_safe_relative_path(path)) {
        return Err(format!("unsafe skill file path: {path}"));
    }
    let Some((_, main)) = files.iter().find(|(path, _)| path == "SKILL.md") else { return Err(format!("skill {slug} has no SKILL.md")) };
    let directory = std::path::PathBuf::from(format!("/{slug}"));
    let mut skill = oc_config::sources::parse_skill(&directory, &directory.join("SKILL.md"), main).map_err(|_| format!("skill {slug} has an invalid SKILL.md"))?;
    skill.path = std::path::PathBuf::from(format!("skill://{slug}/SKILL.md"));
    let mut rest: Vec<(String, String)> = files.into_iter().filter(|(path, _)| path != "SKILL.md").collect();
    rest.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(ConsoleSkill { skill, files: rest.into_iter().collect() })
}

/// `encodeURIComponent`'s unreserved set.
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

fn organization_name(credential: &CredentialValue) -> Option<String> {
    credential.metadata().and_then(|m| m.get("orgName")).and_then(Value::as_str).map(String::from)
}

/// `load()`: the active OpenCode Console connection's config, reporting `needs_auth` statuses. A load that fails
/// for the connection already in place keeps its last config: dropping it would lift organization policy while
/// personal credentials keep working.
pub async fn load(integrations: &Integrations, http: Arc<dyn Http>, user_agent: &str, previous: &Snapshot) -> Snapshot {
    load_with(&shared_cache::GLOBAL, MAX_AGE, integrations, http, user_agent, previous).await
}

/// [`load`] through `cache`, reusing a fetch younger than `max_age`.
pub async fn load_with(
    cache: &Arc<SharedCache>,
    max_age: Duration,
    integrations: &Integrations,
    http: Arc<dyn Http>,
    user_agent: &str,
    previous: &Snapshot,
) -> Snapshot {
    let connection = integrations.active(INTEGRATION_ID).await.ok().flatten();
    let Some(connection) = connection else { return Snapshot::default() };
    let result: Result<Snapshot, LoadError> = async {
        let credential = integrations.resolve(&connection).await.map_err(|error| match error {
            IntegrationError::Authorization(message) if message == SIGNED_OUT_MESSAGE => LoadError::SignedOut,
            other => LoadError::Other(other.to_string()),
        })?;
        match credential {
            None | Some(CredentialValue::External { .. }) => Ok(Snapshot { connection: Some(connection.clone()), ..Snapshot::default() }),
            Some(credential) => {
                let (remote, instructions, skills) = workspace(cache, max_age, http, user_agent, &connection, &credential).await?;
                Ok(Snapshot {
                    remote,
                    instructions,
                    skills,
                    connection: Some(connection.clone()),
                    organization: organization_name(&credential),
                    account: oc_config::policy::account(credential.metadata()),
                })
            }
        }
    }
    .await;
    match result {
        Ok(next) => {
            integrations.set_status(INTEGRATION_ID, &connection, None);
            next
        }
        Err(cause) => {
            if let Some(status) = cause.status() {
                integrations.set_status(INTEGRATION_ID, &connection, Some(status));
            }
            tracing::warn!(cause = %cause, "failed to load OpenCode provider config");
            let same = previous.connection.as_ref().map(ConnectionInfo::key) == Some(connection.key());
            if same {
                Snapshot {
                    remote: previous.remote.clone(),
                    instructions: previous.instructions.clone(),
                    skills: previous.skills.clone(),
                    connection: Some(connection),
                    organization: previous.organization.clone(),
                    account: previous.account.clone(),
                }
            } else {
                Snapshot { connection: Some(connection), ..Snapshot::default() }
            }
        }
    }
}

/// The Console web search provider for a snapshot's connection (`search(providerID, connection)`): every search
/// checks that the same connection is still active.
pub fn websearch_provider_for(
    integrations: Arc<Integrations>,
    connection: ConnectionInfo,
) -> Arc<dyn oc_tools::websearch::SearchProvider> {
    use oc_tools::websearch::{ConsoleCredential, ConsoleProvider};
    let credential: oc_tools::websearch::ConsoleCredentialFn = Arc::new(move || {
        let (integrations, connection) = (integrations.clone(), connection.clone());
        Box::pin(async move {
            let active = integrations.active(INTEGRATION_ID).await.ok().flatten();
            if active.as_ref().map(ConnectionInfo::key) != Some(connection.key()) {
                return Err("OpenCode Console connection changed".to_string());
            }
            let credential = integrations.resolve(active.as_ref().unwrap()).await.map_err(|e| e.to_string())?;
            let credential = match credential {
                Some(c) if !matches!(c, CredentialValue::External { .. }) => c,
                _ => return Err("OpenCode Console is not connected".to_string()),
            };
            let token = match &credential {
                CredentialValue::Oauth { access, .. } => access.clone(),
                CredentialValue::Key { key, .. } => key.clone(),
                CredentialValue::External { .. } => String::new(),
            };
            let org_id = credential.metadata().and_then(|m| m.get("orgID")).and_then(Value::as_str).map(String::from);
            Ok(ConsoleCredential { token, org_id, server: Some(server_url(&credential)) })
        })
    });
    Arc::new(ConsoleProvider::new(WEBSEARCH_ID, credential))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sso_urls_stay_under_the_console_base_path() {
        assert_eq!(
            sso_url("https://opencode.ai/console", "conn 1").unwrap(),
            "https://opencode.ai/console/auth/sso/conn%201/start?redirectTo=%2Fconsole%2F"
        );
        assert_eq!(sso_url("https://dev.example/", "c").unwrap(), "https://dev.example/auth/sso/c/start?redirectTo=%2F");
    }
}
