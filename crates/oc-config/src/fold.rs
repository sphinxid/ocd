//! Per-key fold helpers over the entry list (spec 01 §6.6). Entries are folded from lowest to
//! highest priority; each function reproduces the corresponding domain plugin's rule.

use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use oc_schema::types::PermissionRule;
use serde::Serialize;
use serde_json::{Map, Value};

use crate::schema::{
    Agent, AgentMode, Capabilities, Compatibility, Cost, CostCache, Duration, Experimental, FormatterEntry, Info,
    McpServer, McpTimeout, ModelSelection, Provider, ProviderModel, Reference, Ruleset, ToolOutput, Variant, Warming,
    WebSearch,
};
use crate::schema::Entry;

fn documents(entries: &[Entry]) -> impl Iterator<Item = (&Option<PathBuf>, &Info)> {
    entries.iter().filter_map(|e| match e {
        Entry::Document { path, info, .. } => Some((path, info)),
        Entry::Directory { .. } => None,
    })
}

/// `Config.latest(entries, key)`: the value of the last document defining it.
pub fn latest<'a, T>(entries: &'a [Entry], get: impl Fn(&'a Info) -> Option<&'a T>) -> Option<&'a T> {
    entries.iter().rev().filter_map(Entry::info).find_map(get)
}

/// `shell`: latest defined; empty strings are ignored by the consumer.
pub fn shell(entries: &[Entry]) -> Option<&str> {
    latest(entries, |i| i.shell.as_ref()).map(String::as_str).filter(|s| !s.is_empty())
}

/// `model`: latest defined (whole selection).
pub fn model(entries: &[Entry]) -> Option<&ModelSelection> {
    latest(entries, |i| i.model.as_ref())
}

/// `default_agent`: latest defined (pass JSON + Markdown agent documents).
pub fn default_agent(entries: &[Entry]) -> Option<&str> {
    latest(entries, |i| i.default_agent.as_ref()).map(String::as_str)
}

/// `snapshots`: latest defined.
pub fn snapshots(entries: &[Entry]) -> Option<bool> {
    latest(entries, |i| i.snapshots.as_ref()).copied()
}

/// Tool output defaults (`core/src/tool-output.ts`).
pub const TOOL_OUTPUT_MAX_LINES: u64 = 2000;
pub const TOOL_OUTPUT_MAX_BYTES: u64 = 51200;

/// `tool_output`: latest defined object (not field-merged).
pub fn tool_output(entries: &[Entry]) -> Option<&ToolOutput> {
    latest(entries, |i| i.tool_output.as_ref())
}

/// Web search default selection: `None` = unchanged, `Some(None)` = disabled, `Some(Some(id))`.
pub fn websearch(entries: &[Entry]) -> Option<Option<&str>> {
    latest(entries, |i| i.websearch.as_ref()).map(|w| match w {
        WebSearch::Disabled => None,
        WebSearch::Provider(p) => Some(p.as_str()),
    })
}

/// Warming defaults (`core/src/plugin/warming.ts`).
pub const WARMING_PROMPT: &str =
    "This is a keep-alive request. Do not perform any work or use tools. Reply with exactly: OK";

/// Resolved warming settings.
#[derive(Debug, Clone, PartialEq)]
pub struct WarmingSettings {
    pub prompt: String,
    pub interval: Duration,
    pub duration: Duration,
}

/// `warming`: latest defined; `true` → defaults; object → `{...defaults, ...obj}`; invalid
/// (non-finite/non-positive) durations log the verbatim warning and disable warming.
pub fn warming(entries: &[Entry]) -> Option<WarmingSettings> {
    let configured = latest(entries, |i| i.warming.as_ref())?;
    let mut settings =
        WarmingSettings { prompt: WARMING_PROMPT.into(), interval: Duration::minutes(4.0), duration: Duration::minutes(30.0) };
    match configured {
        Warming::Enabled(false) => return None,
        Warming::Enabled(true) => {}
        Warming::Settings(s) => {
            if let Some(p) = &s.prompt {
                settings.prompt = p.clone();
            }
            if let Some(i) = s.interval {
                settings.interval = i;
            }
            if let Some(d) = s.duration {
                settings.duration = d;
            }
        }
    }
    let ok = |d: &Duration| {
        let m = d.to_millis();
        m.is_finite() && m > 0.0
    };
    if ok(&settings.interval) && ok(&settings.duration) {
        Some(settings)
    } else {
        tracing::warn!("warming interval and duration must be finite positive durations");
        None
    }
}

/// `experimental`: latest document that has `experimental` (whole object).
pub fn experimental(entries: &[Entry]) -> Option<&Experimental> {
    latest(entries, |i| i.experimental.as_ref())
}

/// `experimental.subagent_depth ?? 1`.
pub fn subagent_depth(entries: &[Entry]) -> u64 {
    experimental(entries).and_then(|e| e.subagent_depth).unwrap_or(1)
}

/// `experimental.portable_shell_scanner ?? false`.
pub fn portable_shell_scanner(entries: &[Entry]) -> bool {
    experimental(entries).and_then(|e| e.portable_shell_scanner).unwrap_or(false)
}

/// Compaction settings overlaid field-wise (defaults: auto true, keep 15000, buffer unset).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CompactionSettings {
    pub auto: Option<bool>,
    pub buffer: Option<u64>,
    pub keep: Option<u64>,
}

pub fn compaction(entries: &[Entry]) -> CompactionSettings {
    let mut out = CompactionSettings::default();
    for (_, info) in documents(entries) {
        let Some(c) = &info.compaction else { continue };
        if c.auto.is_some() {
            out.auto = c.auto;
        }
        if c.buffer.is_some() {
            out.buffer = c.buffer;
        }
        if let Some(t) = c.keep.as_ref().and_then(|k| k.tokens) {
            out.keep = Some(t);
        }
    }
    out
}

/// Image settings overlaid field-wise (defaults `true`, `2000`, `2000`, `5242880`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImageSettings {
    pub auto_resize: bool,
    pub max_width: u64,
    pub max_height: u64,
    pub max_base64_bytes: u64,
}

impl Default for ImageSettings {
    fn default() -> Self {
        ImageSettings { auto_resize: true, max_width: 2000, max_height: 2000, max_base64_bytes: 5_242_880 }
    }
}

pub fn media_image(entries: &[Entry]) -> ImageSettings {
    let mut out = ImageSettings::default();
    for (_, info) in documents(entries) {
        let Some(c) = info.media.as_ref().and_then(|m| m.image.as_ref()) else { continue };
        if let Some(v) = c.auto_resize {
            out.auto_resize = v;
        }
        if let Some(v) = c.max_width {
            out.max_width = v;
        }
        if let Some(v) = c.max_height {
            out.max_height = v;
        }
        if let Some(v) = c.max_base64_bytes {
            out.max_base64_bytes = v;
        }
    }
    out
}

/// `worktree.directory` of the last defining document, resolved: `~/` → home, else resolved
/// against the project's canonical checkout.
pub fn worktree_directory(entries: &[Entry], home: &Path, project_canonical: &Path) -> Option<PathBuf> {
    let mut out = None;
    for (_, info) in documents(entries) {
        let Some(w) = &info.worktree else { continue };
        out = Some(match w.directory.strip_prefix("~/") {
            Some(rest) => home.join(rest),
            None => crate::discovery::lexical(&project_canonical.join(&w.directory)),
        });
    }
    out
}

/// `watcher.ignore`: concatenated across documents.
pub fn watcher_ignore(entries: &[Entry]) -> Vec<String> {
    documents(entries).flat_map(|(_, i)| i.watcher.iter().flat_map(|w| w.ignore.iter().flatten().cloned())).collect()
}

/// Folded formatter configuration.
#[derive(Debug, Clone, PartialEq)]
pub enum FormatterConfig {
    Enabled(bool),
    Entries(IndexMap<String, FormatterEntry>),
}

/// `formatter`: a boolean resets everything before it; an object merges per formatter name
/// (`{...prev, ...item, environment: {...prev.environment, ...item.environment}}`).
pub fn formatter(entries: &[Entry]) -> Option<FormatterConfig> {
    let mut result: Option<FormatterConfig> = None;
    for (_, info) in documents(entries) {
        match &info.formatter {
            None => {}
            Some(crate::schema::Formatter::Enabled(b)) => result = Some(FormatterConfig::Enabled(*b)),
            Some(crate::schema::Formatter::Entries(items)) => {
                let mut merged = match result.take() {
                    Some(FormatterConfig::Entries(m)) => m,
                    _ => IndexMap::new(),
                };
                for (name, item) in items {
                    let prev = merged.get(name).cloned().unwrap_or_default();
                    let mut environment = prev.environment.clone().unwrap_or_default();
                    environment.extend(item.environment.clone().unwrap_or_default());
                    merged.insert(
                        name.clone(),
                        FormatterEntry {
                            disabled: item.disabled.or(prev.disabled),
                            command: item.command.clone().or(prev.command),
                            environment: Some(environment),
                            extensions: item.extensions.clone().or(prev.extensions),
                        },
                    );
                }
                result = Some(FormatterConfig::Entries(merged));
            }
        }
    }
    result
}

/// `mcp.timeout`: `Object.assign` of all documents' timeouts in order.
pub fn mcp_timeout(entries: &[Entry]) -> McpTimeout {
    let mut out = McpTimeout::default();
    for (_, info) in documents(entries) {
        if let Some(t) = info.mcp.as_ref().and_then(|m| m.timeout.as_ref()) {
            out.assign(t);
        }
    }
    out
}

/// MCP client timeout defaults (ms).
pub const MCP_DEFAULT_STARTUP_MS: u64 = 30_000;
pub const MCP_DEFAULT_CATALOG_MS: u64 = 30_000;
pub const MCP_DEFAULT_EXECUTION_MS: u64 = 43_200_000;

/// `mcp.servers`: per name the last document's server replaces the whole entry; final timeout =
/// `{...globalTimeout, ...server.timeout}`. (Servers already registered by other plugins must not
/// be overwritten by the consumer.)
pub fn mcp_servers(entries: &[Entry]) -> IndexMap<String, McpServer> {
    let timeout = mcp_timeout(entries);
    let mut servers: IndexMap<String, McpServer> = IndexMap::new();
    for (_, info) in documents(entries) {
        for (name, server) in info.mcp.iter().flat_map(|m| m.servers.iter().flatten()) {
            servers.insert(name.clone(), server.clone());
        }
    }
    for server in servers.values_mut() {
        let mut merged = timeout.clone();
        if let Some(own) = server.timeout() {
            merged.assign(own);
        }
        server.set_timeout(Some(merged));
    }
    servers
}

// ---------------------------------------------------------------------------------------------
// Agents and permissions
// ---------------------------------------------------------------------------------------------

/// Actions whose resources are filesystem paths and get `~`/`$HOME` expansion.
pub const PATH_ACTIONS: [&str; 3] = ["external_directory", "read", "edit"];

/// `expandHome`: `~`, `$HOME`, `~/…`, `$HOME/…`, `$HOME\…`.
pub fn expand_home(resource: &str, home: &Path) -> String {
    if resource == "~" || resource == "$HOME" {
        return home.to_string_lossy().to_string();
    }
    let relative = resource
        .strip_prefix("~/")
        .or_else(|| resource.strip_prefix("$HOME/"))
        .or_else(|| resource.strip_prefix("$HOME\\"));
    match relative {
        Some(rel) => {
            let home_str = home.to_string_lossy();
            if home_str.starts_with('/') {
                crate::discovery::lexical(&home.join(rel)).to_string_lossy().to_string()
            } else {
                // path.win32.join
                let joined = format!("{}\\{}", home_str.trim_end_matches(['\\', '/']), rel.replace('/', "\\"));
                joined
            }
        }
        None => resource.to_string(),
    }
}

/// Expand path-like permission resources.
pub fn expand_permissions(rules: &[PermissionRule], home: &Path) -> Ruleset {
    rules
        .iter()
        .map(|r| {
            if PATH_ACTIONS.contains(&r.action.as_str()) {
                PermissionRule { resource: expand_home(&r.resource, home), ..r.clone() }
            } else {
                r.clone()
            }
        })
        .collect()
}

/// Top-level `permissions` concatenated across documents and expanded.
pub fn permissions(entries: &[Entry], home: &Path) -> Ruleset {
    let rules: Vec<PermissionRule> =
        documents(entries).flat_map(|(_, i)| i.permissions.iter().flatten().cloned()).collect();
    expand_permissions(&rules, home)
}

/// Mutable agent state edited by the agent fold (mirrors core's `Agent.Info` fields).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct AgentState {
    pub model: Option<ModelSelection>,
    pub headers: IndexMap<String, String>,
    pub body: IndexMap<String, Value>,
    pub system: Option<String>,
    pub description: Option<String>,
    pub mode: Option<AgentMode>,
    pub hidden: Option<bool>,
    pub color: Option<String>,
    pub steps: Option<u64>,
    pub permissions: Ruleset,
}

impl AgentState {
    /// Overlay one configured agent item (fields replace; request headers/body `Object.assign`;
    /// permissions appended after expansion).
    pub fn apply(&mut self, item: &Agent, home: &Path) {
        if let Some(m) = &item.model {
            self.model = Some(m.clone());
        }
        if let Some(r) = &item.request {
            self.headers.extend(r.headers.clone().unwrap_or_default());
            self.body.extend(r.body.clone().unwrap_or_default());
        }
        if item.system.is_some() {
            self.system = item.system.clone();
        }
        if item.description.is_some() {
            self.description = item.description.clone();
        }
        if item.mode.is_some() {
            self.mode = item.mode;
        }
        if item.hidden.is_some() {
            self.hidden = item.hidden;
        }
        if item.color.is_some() {
            self.color = item.color.clone();
        }
        if item.steps.is_some() {
            self.steps = item.steps;
        }
        if let Some(p) = &item.permissions {
            self.permissions.extend(expand_permissions(p, home));
        }
    }
}

/// Result of the agent fold.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentFold {
    pub default_agent: Option<String>,
    pub agents: IndexMap<String, AgentState>,
}

/// The agent plugin transform: `existing` are agents registered before config (built-ins).
/// Every existing agent gets the top-level permissions appended; then each document's agents are
/// applied in order (`disabled: true` removes; new agents start with the top-level permissions).
/// Pass [`crate::sources::agent_documents`] to include Markdown agents.
pub fn agents(documents_: &[Entry], home: &Path, existing: IndexMap<String, AgentState>) -> AgentFold {
    let global = permissions(documents_, home);
    let mut agents = existing;
    for agent in agents.values_mut() {
        agent.permissions.extend(global.iter().cloned());
    }
    for (_, info) in documents(documents_) {
        for (id, item) in info.agents.iter().flatten() {
            if item.disabled == Some(true) {
                agents.shift_remove(id);
                continue;
            }
            let agent = agents.entry(id.clone()).or_insert_with(|| AgentState {
                permissions: global.clone(),
                ..Default::default()
            });
            agent.apply(item, home);
        }
    }
    AgentFold { default_agent: default_agent(documents_).map(str::to_string), agents }
}

// ---------------------------------------------------------------------------------------------
// References
// ---------------------------------------------------------------------------------------------

/// A resolved reference source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ReferenceSource {
    Local {
        path: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hidden: Option<bool>,
    },
    Git {
        repository: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hidden: Option<bool>,
    },
}

/// Alias must be non-empty and not contain `/`, whitespace, backtick or comma.
pub fn valid_alias(name: &str) -> bool {
    !name.is_empty() && !name.chars().any(|c| c == '/' || c == '`' || c == ',' || c.is_whitespace())
}

/// `references`: per name last replaces; insertion order = first appearance. Local paths: `~/` →
/// home; relative → the declaring document's directory (or the location directory).
pub fn references(entries: &[Entry], home: &Path, location: &Path) -> IndexMap<String, ReferenceSource> {
    let mut out = IndexMap::new();
    for (path, info) in documents(entries) {
        let directory = path.as_ref().and_then(|p| p.parent()).map(Path::to_path_buf).unwrap_or(location.to_path_buf());
        for (name, entry) in info.references.iter().flatten() {
            if !valid_alias(name) {
                continue;
            }
            let local_path = |value: &str| match value.strip_prefix("~/") {
                Some(rest) => home.join(rest),
                None if Path::new(value).is_absolute() => PathBuf::from(value),
                None => crate::discovery::lexical(&directory.join(value)),
            };
            let source = match entry {
                Reference::Text(s) if s.starts_with('.') || s.starts_with('/') || s.starts_with('~') => {
                    ReferenceSource::Local { path: local_path(s), description: None, hidden: None }
                }
                Reference::Text(s) => {
                    ReferenceSource::Git { repository: s.clone(), branch: None, description: None, hidden: None }
                }
                Reference::Local(l) => ReferenceSource::Local {
                    path: local_path(&l.path),
                    description: l.description.clone(),
                    hidden: l.hidden,
                },
                Reference::Git(g) => ReferenceSource::Git {
                    repository: g.repository.clone(),
                    branch: g.branch.clone(),
                    description: g.description.clone(),
                    hidden: g.hidden,
                },
            };
            out.insert(name.clone(), source);
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Providers
// ---------------------------------------------------------------------------------------------

/// `configuredProviders`: every `(id, provider)` from all documents in order.
pub fn configured_providers(entries: &[Entry]) -> Vec<(&String, &Provider)> {
    documents(entries).flat_map(|(_, i)| i.providers.iter().flatten()).collect()
}

/// `Provider.mergeOverlay`: records recurse, other values replace.
pub fn merge_overlay(base: Option<&Map<String, Value>>, overlay: Option<&Map<String, Value>>) -> Option<Map<String, Value>> {
    match (base, overlay) {
        (None, o) => o.cloned(),
        (Some(b), None) => Some(b.clone()),
        (Some(b), Some(o)) => {
            let mut out = b.clone();
            for (key, right) in o {
                let merged = match (b.get(key), right) {
                    (Some(Value::Object(l)), Value::Object(r)) => {
                        Value::Object(merge_overlay(Some(l), Some(r)).unwrap_or_default())
                    }
                    _ => right.clone(),
                };
                out.insert(key.clone(), merged);
            }
            Some(out)
        }
    }
}

/// `Provider.mergeHeaders`: case-insensitive; the last spelling and value win, position of the
/// first occurrence is kept.
pub fn merge_headers(
    base: Option<&IndexMap<String, String>>,
    overlay: Option<&IndexMap<String, String>>,
) -> Option<IndexMap<String, String>> {
    match (base, overlay) {
        (None, o) => o.cloned(),
        (Some(b), None) => Some(b.clone()),
        (Some(b), Some(o)) => {
            let mut map: IndexMap<String, (String, String)> = IndexMap::new();
            for (k, v) in b.iter().chain(o.iter()) {
                map.insert(k.to_lowercase(), (k.clone(), v.clone()));
            }
            Some(map.into_values().collect())
        }
    }
}

/// Full capabilities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedCapabilities {
    pub tools: bool,
    pub input: Vec<String>,
    pub output: Vec<String>,
}

impl Default for ResolvedCapabilities {
    fn default() -> Self {
        ResolvedCapabilities { tools: true, input: vec!["text".into(), "image".into()], output: vec!["text".into()] }
    }
}

/// `Model.mergeCapabilities(config, base)`.
pub fn merge_capabilities(config: &Capabilities, base: Option<&ResolvedCapabilities>) -> ResolvedCapabilities {
    let fallback = base.cloned().unwrap_or_default();
    ResolvedCapabilities {
        tools: config.tools.unwrap_or(fallback.tools),
        input: config.input.clone().unwrap_or(fallback.input),
        output: config.output.clone().unwrap_or(fallback.output),
    }
}

/// Configured cost normalized to an array with cache defaults `0`.
pub fn normalize_cost(cost: &crate::schema::CostSetting) -> Vec<Cost> {
    cost.as_slice()
        .iter()
        .map(|c| Cost {
            tier: c.tier.clone(),
            input: c.input.clone(),
            output: c.output.clone(),
            cache: Some(CostCache {
                read: Some(c.cache.as_ref().and_then(|x| x.read.clone()).unwrap_or(0.into())),
                write: Some(c.cache.as_ref().and_then(|x| x.write.clone()).unwrap_or(0.into())),
            }),
        })
        .collect()
}

fn settings_map<T: Serialize>(v: &Option<T>) -> Option<Map<String, Value>> {
    v.as_ref().and_then(|s| serde_json::to_value(s).ok()).and_then(|v| v.as_object().cloned())
}

fn json_map(v: &Option<IndexMap<String, Value>>) -> Option<Map<String, Value>> {
    v.as_ref().map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
}

/// Composite overlay of all documents' provider entries per id (field rules of
/// `config/plugin/provider.ts`, without catalog inheritance which needs the registry):
/// scalars replace, `settings`/`body` deep-merge, `headers` merge case-insensitively, models
/// overlay per field (compatibility shallow, capabilities field-wise, variants by id, cost/limit
/// as configured).
pub fn providers(entries: &[Entry]) -> IndexMap<String, Provider> {
    let mut out: IndexMap<String, Provider> = IndexMap::new();
    for (id, item) in configured_providers(entries) {
        let current = out.entry(id.clone()).or_default();
        if item.canonical.is_some() {
            current.canonical = item.canonical.clone();
        }
        if item.name.is_some() {
            current.name = item.name.clone();
        }
        if item.env.is_some() {
            current.env = item.env.clone();
        }
        if item.package.is_some() {
            current.package = item.package.clone();
        }
        if let Some(c) = &item.compatibility {
            let mut merged = current.compatibility.clone().unwrap_or_default();
            merged.0.overlay(&c.0);
            current.compatibility = Some(merged);
        }
        if item.settings.is_some() {
            let merged = merge_overlay(settings_map(&current.settings).as_ref(), settings_map(&item.settings).as_ref());
            current.settings = merged.and_then(|m| crate::decode::Decode::decode(&Value::Object(m)));
        }
        if item.headers.is_some() {
            current.headers = merge_headers(current.headers.as_ref(), item.headers.as_ref());
        }
        if item.body.is_some() {
            current.body = merge_overlay(json_map(&current.body).as_ref(), json_map(&item.body).as_ref())
                .map(|m| m.into_iter().collect());
        }
        for (model_id, config) in item.models.iter().flatten() {
            let models = current.models.get_or_insert_with(IndexMap::new);
            let model = models.entry(model_id.clone()).or_default();
            overlay_model(model, config);
        }
    }
    out
}

/// Overlay one configured model onto an accumulated model definition.
pub fn overlay_model(model: &mut ProviderModel, config: &ProviderModel) {
    if config.family.is_some() {
        model.family = config.family.clone();
    }
    if config.name.is_some() {
        model.name = config.name.clone();
    }
    if config.model_id.is_some() {
        model.model_id = config.model_id.clone();
    }
    if config.release_date.is_some() {
        model.release_date = config.release_date.clone();
    }
    if let Some(c) = &config.compatibility {
        let mut merged = model.compatibility.clone().unwrap_or_else(Compatibility::default);
        merged.overlay(c);
        model.compatibility = Some(merged);
    }
    if config.package.is_some() {
        model.package = config.package.clone();
    }
    if config.settings.is_some() {
        let merged = merge_overlay(settings_map(&model.settings).as_ref(), settings_map(&config.settings).as_ref());
        model.settings = merged.and_then(|m| crate::decode::Decode::decode(&Value::Object(m)));
    }
    if config.headers.is_some() {
        model.headers = merge_headers(model.headers.as_ref(), config.headers.as_ref());
    }
    if config.body.is_some() {
        model.body =
            merge_overlay(json_map(&model.body).as_ref(), json_map(&config.body).as_ref()).map(|m| m.into_iter().collect());
    }
    if let Some(c) = &config.capabilities {
        let base = model.capabilities.clone().unwrap_or_default();
        model.capabilities = Some(Capabilities {
            tools: c.tools.or(base.tools),
            input: c.input.clone().or(base.input),
            output: c.output.clone().or(base.output),
            temperature: c.temperature.or(base.temperature),
            reasoning: c.reasoning.clone().or(base.reasoning),
        });
    }
    if let Some(variants) = &config.variants {
        let list = model.variants.get_or_insert_with(Vec::new);
        for variant in variants {
            let existing = match list.iter().position(|v| v.id == variant.id) {
                Some(i) => &mut list[i],
                None => {
                    list.push(Variant { id: variant.id.clone(), settings: None, headers: None, body: None });
                    list.last_mut().unwrap()
                }
            };
            if variant.settings.is_some() {
                let merged =
                    merge_overlay(settings_map(&existing.settings).as_ref(), settings_map(&variant.settings).as_ref());
                existing.settings = merged.and_then(|m| crate::decode::Decode::decode(&Value::Object(m)));
            }
            if variant.headers.is_some() {
                existing.headers = merge_headers(existing.headers.as_ref(), variant.headers.as_ref());
            }
            if variant.body.is_some() {
                existing.body = merge_overlay(json_map(&existing.body).as_ref(), json_map(&variant.body).as_ref())
                    .map(|m| m.into_iter().collect());
            }
        }
    }
    if let Some(cost) = &config.cost {
        model.cost = Some(crate::schema::CostSetting::Tiers(normalize_cost(cost)));
    }
    if config.disabled.is_some() {
        model.disabled = config.disabled;
    }
    if let Some(limit) = &config.limit {
        let mut merged = model.limit.clone().unwrap_or_default();
        if limit.context.is_some() {
            merged.context = limit.context;
        }
        if limit.input.is_some() {
            merged.input = limit.input;
        }
        if limit.output.is_some() {
            merged.output = limit.output;
        }
        model.limit = Some(merged);
    }
}

// ---------------------------------------------------------------------------------------------
// Plugins, skills
// ---------------------------------------------------------------------------------------------

/// Configured `plugins` items from all documents in order.
pub fn plugins(entries: &[Entry]) -> Vec<&crate::schema::Plugin> {
    documents(entries).flat_map(|(_, i)| i.plugins.iter().flatten()).collect()
}

/// Configured `skills` items: union, deduplicated.
pub fn skills(entries: &[Entry]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for item in documents(entries).flat_map(|(_, i)| i.skills.iter().flatten()) {
        if !out.contains(item) {
            out.push(item.clone());
        }
    }
    out
}
