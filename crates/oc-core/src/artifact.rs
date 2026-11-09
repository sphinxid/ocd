//! Opt-in artifacts (`core/src/artifact.ts`, `experimental.artifacts`, #60351).
//!
//! An artifact is a set of files published through a provider. Records live in the KV store under
//! `artifact:item:<id>` (`{info, directory, projectID}`), scoped to the project of the location that
//! created them. The built-in `local` provider keeps the files under `<data>/artifacts/<id>/` and
//! serves them from `/api/experimental/artifacts/<id>/view/`; plugins add providers through
//! `ctx.experimental.artifact.transform` (the sidecar's `artifact` transform domain), which may also
//! replace `local` or pick another default.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::app::App;
use crate::location::Location;

pub mod tools;

/// `Artifact.LocalProviderID`.
pub const LOCAL: &str = "local";
const LOCAL_NAME: &str = "Local";
const LOCAL_PROMPT: &str =
    "Artifacts are served as HTML by this OpenCode server. Only people who can reach this server can open them.";
const ITEM_PREFIX: &str = "artifact:item:";

/// The transform domain plugin providers register in.
pub const DOMAIN: &str = "artifact";

/// `/api/experimental/artifacts/<id>/view/`: where the local provider serves an artifact.
pub fn view_url(id: &str) -> String {
    format!("/api/experimental/artifacts/{}/view/", encode_uri_component(id))
}

fn encode_uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

// ---- errors ---------------------------------------------------------------------------------

/// The service's failures (`Artifact.*Error`, plus `Session.NotFoundError`).
#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    Disabled,
    NotFound(String),
    FileMissing { artifact_id: String, path: String },
    ProviderNotFound(String),
    Invalid { reason: String, field: Option<&'static str> },
    /// A provider failed; `message` is the cause's message when it was an Error with one.
    Provider { provider_id: String, message: Option<String> },
    SessionNotFound(String),
    /// Not a typed failure (a provider that died, the database).
    Defect(String),
}

impl Error {
    pub fn message(&self) -> String {
        match self {
            Error::Disabled => "Artifacts are disabled; set experimental.artifacts in config".into(),
            Error::NotFound(id) => format!("Artifact not found: {id}"),
            Error::FileMissing { path, .. } => format!("Artifact file not found: {path}"),
            Error::ProviderNotFound(id) => format!("Artifact provider not found: {id}"),
            Error::Invalid { reason, .. } => reason.clone(),
            Error::Provider { provider_id, message: Some(m) } => format!("Artifact provider {provider_id} failed: {m}"),
            Error::Provider { provider_id, message: None } => format!("Artifact provider {provider_id} failed"),
            Error::SessionNotFound(id) => format!("Session not found: {id}"),
            Error::Defect(m) => m.clone(),
        }
    }

    fn invalid(reason: impl Into<String>, field: &'static str) -> Self {
        Error::Invalid { reason: reason.into(), field: Some(field) }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl From<anyhow::Error> for Error {
    fn from(e: anyhow::Error) -> Self {
        Error::Defect(format!("{e:#}"))
    }
}

// ---- schema ---------------------------------------------------------------------------------

/// `Artifact.Time`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Time {
    pub created: f64,
    pub updated: f64,
}

/// `Artifact.Info` (field order as upstream encodes it).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Info {
    pub id: String,
    pub provider: String,
    #[serde(rename = "sessionID", default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub title: String,
    pub entry: String,
    pub url: String,
    pub time: Time,
}

/// The KV record (`StoredRecord`).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Stored {
    info: Info,
    directory: String,
    #[serde(rename = "projectID")]
    project_id: String,
}

fn decode_stored(value: Value) -> Option<Stored> {
    let stored: Stored = serde_json::from_value(value).ok()?;
    // `Artifact.ID` / `ProviderID` checks of the decode.
    (stored.info.id.starts_with("art_") && !stored.info.provider.is_empty() && stored.info.provider.trim() == stored.info.provider)
        .then_some(stored)
}

/// `Artifact.CreateInput`.
#[derive(Debug, Clone, Default)]
pub struct CreateInput {
    pub title: String,
    /// The file map in the order a JS object enumerates it.
    pub files: Vec<(String, String)>,
    pub entry: Option<String>,
    pub provider: Option<String>,
    pub session_id: Option<String>,
}

/// `Artifact.UpdateInput`.
#[derive(Debug, Clone, Default)]
pub struct UpdateInput {
    pub title: Option<String>,
    pub files: Vec<(String, String)>,
    pub entry: Option<String>,
    pub session_id: Option<String>,
}

/// A JSON object's entries in the order a JS object enumerates them: integer-like keys first,
/// ascending, then the others in insertion order (a repeated key keeps its first position).
pub fn js_entries<T>(entries: impl IntoIterator<Item = (String, T)>) -> Vec<(String, T)> {
    let mut ordered: indexmap::IndexMap<String, T> = indexmap::IndexMap::new();
    for (key, value) in entries {
        ordered.insert(key, value);
    }
    let index = |k: &str| -> Option<u64> {
        let n: u64 = k.parse().ok()?;
        (n.to_string() == k && n < u32::MAX as u64).then_some(n)
    };
    let mut integers: Vec<(u64, String, T)> = Vec::new();
    let mut others: Vec<(String, T)> = Vec::new();
    for (k, v) in ordered {
        match index(&k) {
            Some(n) => integers.push((n, k, v)),
            None => others.push((k, v)),
        }
    }
    integers.sort_by_key(|(n, _, _)| *n);
    integers.into_iter().map(|(_, k, v)| (k, v)).chain(others).collect()
}

/// A JSON object's string entries (`Schema.Record(String, String)` input).
pub fn string_entries(value: &Value) -> Vec<(String, String)> {
    js_entries(
        value
            .as_object()
            .into_iter()
            .flatten()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string())),
    )
}

// ---- providers ------------------------------------------------------------------------------

/// A registered provider: the native local one (no handles) or a plugin's (function handles).
#[derive(Debug, Clone, PartialEq)]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub capabilities: Vec<String>,
    pub prompt: String,
    handles: Option<Handles>,
}

#[derive(Debug, Clone, PartialEq)]
struct Handles {
    create: String,
    read: String,
    update: String,
    delete: String,
}

impl Provider {
    fn local() -> Self {
        Provider {
            id: LOCAL.into(),
            name: LOCAL_NAME.into(),
            capabilities: vec!["static".into()],
            prompt: LOCAL_PROMPT.into(),
            handles: None,
        }
    }

    fn native_json(&self) -> Value {
        json!({ "id": self.id, "name": self.name, "capabilities": self.capabilities, "prompt": self.prompt, "native": true })
    }

    fn from_json(value: &Value) -> Option<Self> {
        let id = value.get("id")?.as_str()?.to_string();
        let name = value.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
        let capabilities = value
            .get("capabilities")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|c| c.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let prompt = value.get("prompt").and_then(Value::as_str).unwrap_or_default().to_string();
        let handles = if value.get("native").and_then(Value::as_bool) == Some(true) {
            None
        } else {
            let handle = |k: &str| value.get(k).and_then(oc_plugin::protocol::function_handle).map(str::to_string);
            Some(Handles { create: handle("create")?, read: handle("read")?, update: handle("update")?, delete: handle("delete")? })
        };
        Some(Provider { id, name, capabilities, prompt, handles })
    }
}

/// The provider registry of a location (`State<Data>`): providers in registration order, `local` first.
#[derive(Debug, Clone, PartialEq)]
pub struct Providers {
    pub list: Vec<Provider>,
    pub selection: Option<String>,
}

impl Default for Providers {
    fn default() -> Self {
        Providers { list: vec![Provider::local()], selection: None }
    }
}

impl Providers {
    pub fn get(&self, id: &str) -> Option<&Provider> {
        self.list.iter().find(|p| p.id == id)
    }

    /// `activeDefaultID`: the selection while it names a provider, else `local`.
    pub fn default_id(&self) -> &str {
        match &self.selection {
            Some(s) if self.get(s).is_some() => s,
            _ => LOCAL,
        }
    }

    /// `snapshotProviders`: what `artifact.updated` compares, sorted by id.
    pub fn snapshot(&self) -> Value {
        let mut list: Vec<&Provider> = self.list.iter().collect();
        list.sort_by(|a, b| locale_compare(&a.id, &b.id));
        Value::Array(
            list.into_iter()
                .map(|p| {
                    let mut m = Map::new();
                    m.insert("id".into(), json!(p.id));
                    m.insert("name".into(), json!(p.name));
                    m.insert("capabilities".into(), json!(p.capabilities));
                    if let Some(s) = &self.selection {
                        m.insert("selection".into(), json!(s));
                    }
                    Value::Object(m)
                })
                .collect(),
        )
    }

    /// `prompts()`: the default provider and the others sorted by name.
    pub fn prompts(&self) -> (Provider, Vec<Provider>) {
        let default = self.get(self.default_id()).cloned().unwrap_or_else(Provider::local);
        let mut others: Vec<Provider> = self.list.iter().filter(|p| p.id != default.id).cloned().collect();
        others.sort_by(|a, b| locale_compare(&a.name, &b.name));
        (default, others)
    }
}

fn locale_compare(a: &str, b: &str) -> Ordering {
    oc_schema::collate::locale_compare(a, b)
}

/// The providers of location `key`: the local one, then what plugin `artifact` transforms make of it.
pub async fn providers(plugins: &crate::plugins::Plugins, key: &str) -> Providers {
    if !plugins.transforms(key, DOMAIN) {
        return Providers::default();
    }
    let input = json!({ "providers": [Provider::local().native_json()] });
    let out = plugins.transform(key, DOMAIN, input).await;
    let list: Vec<Provider> =
        out.get("providers").and_then(Value::as_array).into_iter().flatten().filter_map(Provider::from_json).collect();
    let selection = out.get("selection").and_then(Value::as_str).map(str::to_string);
    if list.is_empty() {
        return Providers { selection, ..Providers::default() };
    }
    Providers { list, selection }
}

/// The `create_description` of `artifact_create` (`renderCreateDescription`).
pub fn create_description(providers: &Providers) -> String {
    let (default, others) = providers.prompts();
    let mut lines = vec![
        "Publish a new artifact to the default artifact provider or a named override.".to_string(),
        format!("Default provider ({} [{}]): {}", default.id, default.capabilities.join(", "), default.prompt),
    ];
    if !others.is_empty() {
        let list: Vec<String> = others.iter().map(|p| format!("{} [{}]", p.id, p.capabilities.join(", "))).collect();
        lines.push(format!("Other active providers (pass provider to override): {}.", list.join(", ")));
    }
    lines.join("\n")
}

// ---- service --------------------------------------------------------------------------------

/// A local file the view route serves.
#[derive(Debug, Clone)]
pub struct LocalFile {
    pub bytes: Vec<u8>,
    pub mime: String,
}

/// The artifact service of one location (`Artifact.Service` in its `Location` layer).
pub struct Service<'a> {
    pub app: &'a App,
    pub location: &'a Location,
}

/// `<data>/artifacts`.
pub fn root(app: &App) -> PathBuf {
    app.global.data.join("artifacts")
}

/// Whether the location's config sets `experimental.artifacts: true`.
pub fn enabled(location: &Location) -> bool {
    oc_config::fold::experimental(&location.entries).and_then(|e| e.artifacts) == Some(true)
}

/// `Artifact.locate`: the directory an artifact was created in, for routes without an explicit location.
pub fn locate(db: &oc_storage::Database, id: &str) -> Option<String> {
    let stored = db.kv_get(&format!("{ITEM_PREFIX}{id}")).ok().flatten()?;
    decode_stored(stored).map(|s| s.directory)
}

impl<'a> Service<'a> {
    pub fn new(app: &'a App, location: &'a Location) -> Self {
        Service { app, location }
    }

    fn key(&self) -> &str {
        &self.location.key
    }

    fn project_id(&self) -> &str {
        &self.location.project.id
    }

    fn item_dir(&self, id: &str) -> PathBuf {
        root(self.app).join(id)
    }

    pub fn enabled(&self) -> bool {
        enabled(self.location)
    }

    fn require_enabled(&self) -> Result<(), Error> {
        if self.enabled() { Ok(()) } else { Err(Error::Disabled) }
    }

    pub async fn registry(&self) -> Providers {
        providers(&self.app.plugins, self.key()).await
    }

    fn check_session(&self, session_id: Option<&str>) -> Result<(), Error> {
        let Some(id) = session_id else { return Ok(()) };
        match self.app.db.session_get(id)? {
            Some(_) => Ok(()),
            None => Err(Error::SessionNotFound(id.to_string())),
        }
    }

    /// `providers()`: `[{id, name, capabilities, default}]`, the default first, then by name.
    pub async fn providers(&self) -> Result<Vec<Value>, Error> {
        self.require_enabled()?;
        let registry = self.registry().await;
        let default = registry.default_id().to_string();
        let mut list: Vec<(bool, &Provider)> = registry.list.iter().map(|p| (p.id == default, p)).collect();
        list.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| locale_compare(&a.1.name, &b.1.name)));
        Ok(list
            .into_iter()
            .map(|(d, p)| json!({ "id": p.id, "name": p.name, "capabilities": p.capabilities, "default": d }))
            .collect())
    }

    /// `list({sessionID?, provider?})`: newest update first.
    pub fn list(&self, session_id: Option<&str>, provider: Option<&str>) -> Result<Vec<Info>, Error> {
        self.require_enabled()?;
        self.check_session(session_id)?;
        let mut items = self.scan()?;
        items.retain(|i| {
            session_id.is_none_or(|s| i.session_id.as_deref() == Some(s)) && provider.is_none_or(|p| i.provider == p)
        });
        items.sort_by(|a, b| {
            b.time.updated.partial_cmp(&a.time.updated).unwrap_or(Ordering::Equal).then_with(|| locale_compare(&b.id, &a.id))
        });
        Ok(items)
    }

    fn scan(&self) -> Result<Vec<Info>, Error> {
        let mut out = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let (entries, next) = self.app.db.kv_scan(ITEM_PREFIX, after.as_deref(), Some(500))?;
            for (_, value) in entries {
                if let Some(stored) = decode_stored(value)
                    && stored.project_id == self.project_id()
                {
                    out.push(stored.info);
                }
            }
            match next {
                Some(n) => after = Some(n),
                None => return Ok(out),
            }
        }
    }

    fn load(&self, id: &str) -> Result<Option<Stored>, Error> {
        let Some(value) = self.app.db.kv_get(&format!("{ITEM_PREFIX}{id}"))? else { return Ok(None) };
        Ok(decode_stored(value).filter(|s| s.project_id == self.project_id()))
    }

    fn store(&self, stored: &Stored) -> Result<(), Error> {
        let value = serde_json::to_value(stored).map_err(|e| Error::Defect(e.to_string()))?;
        self.app.db.kv_set(&format!("{ITEM_PREFIX}{}", stored.info.id), &value)?;
        Ok(())
    }

    fn resolve_provider(&self, registry: &Providers, requested: Option<&str>) -> Result<Provider, Error> {
        let target = requested.unwrap_or(registry.default_id());
        registry.get(target).cloned().ok_or_else(|| Error::ProviderNotFound(target.to_string()))
    }

    pub async fn create(&self, input: CreateInput) -> Result<Info, Error> {
        self.require_enabled()?;
        self.check_session(input.session_id.as_deref())?;
        let title = js_trim(&input.title).to_string();
        if title.is_empty() {
            return Err(Error::invalid("Artifact title must not be empty", "title"));
        }
        let files = normalize_files(input.files)?;
        let entry = resolve_entry(&files, input.entry.as_deref())?;
        let registry = self.registry().await;
        let provider = self.resolve_provider(&registry, input.provider.as_deref())?;
        let id = oc_schema::id::ascending("art");
        let now = oc_schema::id::now_ms() as f64;
        let url = self
            .provider_write(&provider, &id, &title, &entry, &files, input.session_id.as_deref(), true)
            .await?;
        let info = Info {
            id: id.clone(),
            provider: provider.id.clone(),
            session_id: input.session_id.clone(),
            title,
            entry,
            url: url.unwrap_or_else(|| view_url(&id)),
            time: Time { created: now, updated: now },
        };
        self.store(&Stored { info: info.clone(), directory: self.key().to_string(), project_id: self.project_id().to_string() })?;
        Ok(info)
    }

    pub fn info(&self, id: &str) -> Result<Info, Error> {
        self.require_enabled()?;
        self.load(id)?.map(|s| s.info).ok_or_else(|| Error::NotFound(id.to_string()))
    }

    /// `get(id)`: metadata and file contents (`Artifact.Content`).
    pub async fn get(&self, id: &str) -> Result<Value, Error> {
        self.require_enabled()?;
        let existing = self.load(id)?.ok_or_else(|| Error::NotFound(id.to_string()))?;
        let registry = self.registry().await;
        let provider = self.resolve_provider(&registry, Some(&existing.info.provider))?;
        let files = match &provider.handles {
            None => read_directory_files(&self.item_dir(id)),
            Some(h) => {
                let out = self.call(&provider, &h.read, vec![json!(id)]).await?;
                js_entries(out.as_object().into_iter().flatten().map(|(k, v)| {
                    let bytes = v.as_str().and_then(|s| STANDARD.decode(s).ok()).unwrap_or_default();
                    (k.clone(), bytes)
                }))
            }
        };
        let files: Map<String, Value> =
            js_entries(files.into_iter().map(|(k, bytes)| (k, decode_text(&bytes)))).into_iter().map(|(k, v)| (k, Value::String(v))).collect();
        Ok(json!({ "info": existing.info, "files": files }))
    }

    pub async fn update(&self, id: &str, input: UpdateInput) -> Result<Info, Error> {
        self.require_enabled()?;
        self.check_session(input.session_id.as_deref())?;
        let existing = self.load(id)?.ok_or_else(|| Error::NotFound(id.to_string()))?;
        let title = match &input.title {
            None => existing.info.title.clone(),
            Some(t) => js_trim(t).to_string(),
        };
        if title.is_empty() {
            return Err(Error::invalid("Artifact title must not be empty", "title"));
        }
        let files = normalize_files(input.files)?;
        let keep = files.iter().any(|(k, _)| *k == existing.info.entry).then(|| existing.info.entry.clone());
        let entry = resolve_entry(&files, input.entry.as_deref().or(keep.as_deref()))?;
        let registry = self.registry().await;
        let provider = self.resolve_provider(&registry, Some(&existing.info.provider))?;
        let now = oc_schema::id::now_ms() as f64;
        let session_id = input.session_id.clone().or(existing.info.session_id.clone());
        let url = self.provider_write(&provider, id, &title, &entry, &files, session_id.as_deref(), false).await?;
        let info = Info {
            id: id.to_string(),
            provider: existing.info.provider.clone(),
            session_id,
            title,
            entry,
            url: url.unwrap_or(existing.info.url.clone()),
            time: Time { created: existing.info.time.created, updated: now },
        };
        self.store(&Stored { info: info.clone(), directory: existing.directory, project_id: existing.project_id })?;
        Ok(info)
    }

    pub async fn remove(&self, id: &str) -> Result<(), Error> {
        self.require_enabled()?;
        let existing = self.load(id)?.ok_or_else(|| Error::NotFound(id.to_string()))?;
        let registry = self.registry().await;
        if let Some(provider) = registry.get(&existing.info.provider).cloned() {
            match &provider.handles {
                None => {
                    let _ = std::fs::remove_dir_all(self.item_dir(id));
                }
                Some(h) => {
                    self.call(&provider, &h.delete, vec![json!(id)]).await?;
                }
            }
        }
        self.app.db.kv_remove(&format!("{ITEM_PREFIX}{id}"))?;
        Ok(())
    }

    /// `readLocalFile(id, subpath?)`: a file of a local artifact (its entry without a subpath).
    pub fn read_local_file(&self, id: &str, subpath: Option<&str>) -> Result<LocalFile, Error> {
        self.require_enabled()?;
        let existing = self.load(id)?.ok_or_else(|| Error::NotFound(id.to_string()))?;
        if existing.info.provider != LOCAL {
            return Err(Error::invalid(
                format!("Artifact {id} is hosted by provider \"{}\", not the local server", existing.info.provider),
                "artifactID",
            ));
        }
        let requested = match subpath {
            Some(s) if !js_trim(s).is_empty() => normalize_path(s)?,
            _ => existing.info.entry.clone(),
        };
        let mut target = self.item_dir(id);
        for part in requested.split('/') {
            target.push(part);
        }
        if !std::fs::metadata(&target).is_ok_and(|m| m.is_file()) {
            return Err(Error::FileMissing { artifact_id: id.to_string(), path: requested });
        }
        let bytes = std::fs::read(&target).map_err(|e| Error::Defect(e.to_string()))?;
        let mime = detect_file_mime(&requested, &bytes);
        Ok(LocalFile { bytes, mime })
    }

    /// The provider's `create` (or `update`): the URL it reports, if any.
    #[allow(clippy::too_many_arguments)]
    async fn provider_write(
        &self,
        provider: &Provider,
        id: &str,
        title: &str,
        entry: &str,
        files: &[(String, Vec<u8>)],
        session_id: Option<&str>,
        create: bool,
    ) -> Result<Option<String>, Error> {
        match &provider.handles {
            None => {
                replace_directory_files(&self.item_dir(id), files)
                    .map_err(|_| Error::Provider { provider_id: provider.id.clone(), message: None })?;
                Ok(Some(view_url(id)))
            }
            Some(h) => {
                let mut input = Map::new();
                if create {
                    input.insert("id".into(), json!(id));
                }
                input.insert("title".into(), json!(title));
                input.insert("entry".into(), json!(entry));
                let encoded: Map<String, Value> = files.iter().map(|(k, v)| (k.clone(), Value::String(STANDARD.encode(v)))).collect();
                input.insert("files".into(), Value::Object(encoded));
                if let Some(s) = session_id {
                    input.insert("sessionID".into(), json!(s));
                }
                let out = if create {
                    self.call(provider, &h.create, vec![Value::Object(input)]).await?
                } else {
                    self.call(provider, &h.update, vec![json!(id), Value::Object(input)]).await?
                };
                Ok(out.get("url").and_then(Value::as_str).map(str::to_string))
            }
        }
    }

    /// Calls a plugin provider's wrapped function: `{ok}` or `{error}` (a typed failure).
    async fn call(&self, provider: &Provider, handle: &str, args: Vec<Value>) -> Result<Value, Error> {
        let out = self.app.plugins.call_handle(self.key(), handle, args).await.map_err(|e| Error::Defect(e.message))?;
        if let Some(ok) = out.get("ok") {
            return Ok(ok.clone());
        }
        if out.get("error").is_some() {
            let message = out.get("error").and_then(Value::as_str).map(str::to_string);
            return Err(Error::Provider { provider_id: provider.id.clone(), message });
        }
        Err(Error::Defect("artifact provider returned no result".into()))
    }
}

// ---- helpers --------------------------------------------------------------------------------

fn js_trim(s: &str) -> &str {
    oc_tools::util::js::trim(s)
}

/// `normalizePath`: forward slashes, no leading `./` or `/`, no empty, `.` or `..` segment.
pub fn normalize_path(raw: &str) -> Result<String, Error> {
    let replaced = raw.replace('\\', "/");
    let mut rest = replaced.as_str();
    while let Some(r) = rest.strip_prefix("./") {
        rest = r;
    }
    let trimmed = rest.trim_start_matches('/');
    if trimmed.is_empty() || trimmed.contains('\0') {
        return Err(Error::invalid(format!("Invalid artifact file path: {}", oc_tools::util::js::quote(raw)), "files"));
    }
    let segments: Vec<&str> = trimmed.split('/').collect();
    if segments.iter().any(|s| s.is_empty() || *s == "." || *s == "..") {
        return Err(Error::invalid(format!("Artifact file path must stay relative: {}", oc_tools::util::js::quote(raw)), "files"));
    }
    Ok(segments.join("/"))
}

/// `normalizeFiles(encodeStringFiles(files))`.
fn normalize_files(files: Vec<(String, String)>) -> Result<Vec<(String, Vec<u8>)>, Error> {
    if files.is_empty() {
        return Err(Error::invalid("Artifact must include at least one file", "files"));
    }
    let mut out: Vec<(String, Vec<u8>)> = Vec::with_capacity(files.len());
    for (raw, content) in files {
        let clean = normalize_path(&raw)?;
        out.push((clean, content.into_bytes()));
    }
    Ok(js_entries(out))
}

/// `resolveEntry`: the requested entry when it is one of the files, else `index.html`, `index.md`
/// or the only file.
fn resolve_entry(files: &[(String, Vec<u8>)], requested: Option<&str>) -> Result<String, Error> {
    let has = |k: &str| files.iter().any(|(f, _)| f == k);
    if let Some(requested) = requested {
        let clean = normalize_path(requested)?;
        if has(&clean) {
            return Ok(clean);
        }
        return Err(Error::invalid(format!("Entry file not found in files: {clean}"), "entry"));
    }
    if has("index.html") {
        return Ok("index.html".into());
    }
    if has("index.md") {
        return Ok("index.md".into());
    }
    if files.len() == 1 {
        return Ok(files[0].0.clone());
    }
    Err(Error::invalid("Include \"index.html\" or \"index.md\", or specify \"entry\" when publishing multiple files", "entry"))
}

/// `TextDecoder().decode`: lossy UTF-8 without a leading BOM.
fn decode_text(bytes: &[u8]) -> String {
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    String::from_utf8_lossy(bytes).into_owned()
}

/// `detectFileMime`: the extension's type (text, JavaScript and JSON with `; charset=utf-8`), else
/// the sniffed one.
fn detect_file_mime(path: &str, bytes: &[u8]) -> String {
    match oc_tools::mime::lookup_str(path) {
        Some(mime) if mime.starts_with("text/") || mime == "application/javascript" || mime == "application/json" => {
            format!("{mime}; charset=utf-8")
        }
        Some(mime) => mime.to_string(),
        None => oc_tools::mime::detect(bytes).to_string(),
    }
}

/// `replaceDirectoryFiles`: writes the files into a staging directory, then swaps it in for `base`.
fn replace_directory_files(base: &Path, files: &[(String, Vec<u8>)]) -> std::io::Result<()> {
    let parent = base.parent().unwrap_or(base);
    std::fs::create_dir_all(parent)?;
    let nonce = uuid::Uuid::new_v4();
    let name = base.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let staging = parent.join(format!("{name}.staging-{nonce}"));
    let backup = parent.join(format!("{name}.backup-{nonce}"));
    let result = (|| {
        std::fs::create_dir_all(&staging)?;
        for (rel, bytes) in files {
            let mut dest = staging.clone();
            for part in rel.split('/') {
                dest.push(part);
            }
            if let Some(dir) = dest.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&dest, bytes)?;
        }
        let existed = std::fs::symlink_metadata(base).is_ok();
        if existed {
            std::fs::rename(base, &backup)?;
        }
        if let Err(e) = std::fs::rename(&staging, base) {
            if existed {
                let _ = std::fs::rename(&backup, base);
            }
            return Err(e);
        }
        if existed {
            let _ = std::fs::remove_dir_all(&backup);
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    result
}

/// `readDirectoryFiles`: every file under `base` keyed by its relative path. Upstream reads them
/// concurrently, so a directory's files arrive before its subdirectories' (breadth first, each
/// directory in the order the file system lists it). A missing directory has none.
fn read_directory_files(base: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    let mut level: Vec<(PathBuf, String)> = vec![(base.to_path_buf(), String::new())];
    while !level.is_empty() {
        let mut next = Vec::new();
        for (dir, prefix) in level {
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let rel = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
                let Ok(kind) = entry.file_type() else { continue };
                if kind.is_dir() {
                    next.push((entry.path(), rel));
                } else if kind.is_file()
                    && let Ok(bytes) = std::fs::read(entry.path())
                {
                    out.push((rel, bytes));
                }
            }
        }
        level = next;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_paths_like_upstream() {
        assert_eq!(normalize_path("./a/b.html").unwrap(), "a/b.html");
        assert_eq!(normalize_path("/x\\y.js").unwrap(), "x/y.js");
        assert_eq!(normalize_path("././/a").unwrap(), "a");
        assert_eq!(
            normalize_path("a/../b").unwrap_err().message(),
            "Artifact file path must stay relative: \"a/../b\""
        );
        assert_eq!(normalize_path("").unwrap_err().message(), "Invalid artifact file path: \"\"");
        assert_eq!(normalize_path("a//b").unwrap_err().message(), "Artifact file path must stay relative: \"a//b\"");
    }

    #[test]
    fn resolves_entries() {
        let files = |names: &[&str]| names.iter().map(|n| (n.to_string(), vec![])).collect::<Vec<_>>();
        assert_eq!(resolve_entry(&files(&["a.css", "index.html"]), None).unwrap(), "index.html");
        assert_eq!(resolve_entry(&files(&["index.md", "x"]), None).unwrap(), "index.md");
        assert_eq!(resolve_entry(&files(&["only.txt"]), None).unwrap(), "only.txt");
        assert_eq!(
            resolve_entry(&files(&["a", "b"]), None).unwrap_err().message(),
            "Include \"index.html\" or \"index.md\", or specify \"entry\" when publishing multiple files"
        );
        assert_eq!(
            resolve_entry(&files(&["a"]), Some("./b")).unwrap_err().message(),
            "Entry file not found in files: b"
        );
    }

    #[test]
    fn orders_keys_like_js_objects() {
        let entries = js_entries(vec![("b".to_string(), 1), ("2".to_string(), 2), ("a".to_string(), 3), ("1".to_string(), 4), ("b".to_string(), 5)]);
        let keys: Vec<&str> = entries.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["1", "2", "b", "a"]);
        assert_eq!(entries[2].1, 5);
    }

    #[test]
    fn describes_create_with_other_providers() {
        let mut providers = Providers::default();
        providers.list.push(Provider {
            id: "pages".into(),
            name: "Pages".into(),
            capabilities: vec!["static".into(), "server-code".into()],
            prompt: "Pages prompt.".into(),
            handles: None,
        });
        assert_eq!(
            create_description(&providers),
            "Publish a new artifact to the default artifact provider or a named override.\nDefault provider (local [static]): Artifacts are served as HTML by this OpenCode server. Only people who can reach this server can open them.\nOther active providers (pass provider to override): pages [static, server-code]."
        );
        providers.selection = Some("pages".into());
        assert!(create_description(&providers).contains("Default provider (pages [static, server-code]): Pages prompt."));
        assert!(create_description(&providers).ends_with("Other active providers (pass provider to override): local [static]."));
    }

    #[test]
    fn mime_of_local_files() {
        assert_eq!(detect_file_mime("index.html", b"<p>"), "text/html; charset=utf-8");
        assert_eq!(detect_file_mime("app.js", b""), "text/javascript; charset=utf-8");
        assert_eq!(detect_file_mime("data.json", b"{}"), "application/json; charset=utf-8");
        assert_eq!(detect_file_mime("a.png", b""), "image/png");
        assert_eq!(detect_file_mime("noext", b"hello"), "text/plain");
    }
}
