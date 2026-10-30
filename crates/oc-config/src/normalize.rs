//! v1 → v2 normalization (`packages/core/src/config/normalize.ts`).
//!
//! Every document (v1, v2 or mixed) goes through [`normalize`]. Malformed recognized values are
//! skipped individually with a diagnostic; diagnostics never contain values.

use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::decode::{Decode, deep_equal, encode, js_numbers, non_negative_int, positive_int, record};
use crate::legacy;
use crate::schema::{
    Agent, Command, Compaction, Enterprise, Experimental, Formatter, IdleTimeout, FormatterEntry, Lsp, LspEntry, McpServer,
    McpTimeout, Media, ModelSelection, Plugin, Policy, Provider, Reference, Share, ToolOutput, UpdatePolicy,
    Warming, Watcher, WebSearch, Worktree,
};
use oc_schema::types::PermissionRule;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DiagnosticKind {
    Conflict,
    Invalid,
    Unsupported,
}

impl DiagnosticKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            DiagnosticKind::Conflict => "conflict",
            DiagnosticKind::Invalid => "invalid",
            DiagnosticKind::Unsupported => "unsupported",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Diagnostic {
    pub kind: DiagnosticKind,
    pub path: Vec<String>,
    pub message: &'static str,
}

impl Diagnostic {
    /// Log path: `"$"` for the root, otherwise `$.a.b.c`.
    pub fn log_path(&self) -> String {
        if self.path.first().map(String::as_str) == Some("$") {
            "$".into()
        } else {
            format!("$.{}", self.path.join("."))
        }
    }
}

pub const MSG_ROOT: &str = "rejected configuration because its root is not an object";
pub const MSG_INVALID: &str = "skipped malformed recognized value";
pub const MSG_CONFLICT: &str = "retained native value over legacy value";
pub const MSG_UNSUPPORTED: &str = "omitted unsupported legacy setting";
pub const MSG_UNSUPPORTED_MODEL: &str = "omitted unsupported legacy model reference";
pub const MSG_UNSUPPORTED_VARIANT: &str = "omitted unsupported legacy model variant";
pub const MSG_ENABLED_ONLY_MCP: &str = "omitted enabled-only legacy MCP entry";

/// Result of [`normalize`].
#[derive(Debug, Clone, PartialEq)]
pub enum NormalizeResult {
    Normalized { encoded: Value, diagnostics: Vec<Diagnostic> },
    Rejected { diagnostics: Vec<Diagnostic> },
}

impl NormalizeResult {
    pub fn diagnostics(&self) -> &[Diagnostic] {
        match self {
            NormalizeResult::Normalized { diagnostics, .. } | NormalizeResult::Rejected { diagnostics } => diagnostics,
        }
    }
    pub fn encoded(&self) -> Option<&Value> {
        match self {
            NormalizeResult::Normalized { encoded, .. } => Some(encoded),
            NormalizeResult::Rejected { .. } => None,
        }
    }
}

const UNSUPPORTED_TOP_LEVEL: &[&str] = &["logLevel", "server", "subagent_depth", "layout"];
const UNSUPPORTED_EXPERIMENTAL: &[&str] =
    &["disable_paste_summary", "batch_tool", "openTelemetry", "primary_tools", "continue_loop_on_deny"];
const UNSUPPORTED_PROVIDER: &[&str] = &["id", "whitelist", "blacklist"];
const UNSUPPORTED_MODEL: &[&str] = &["release_date", "attachment", "reasoning", "temperature", "experimental"];

type Diags = Vec<Diagnostic>;
type Obj = Map<String, Value>;

fn p(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

fn with(path: &[String], extra: &str) -> Vec<String> {
    let mut out = path.to_vec();
    out.push(extra.to_string());
    out
}

fn invalid(path: Vec<String>, d: &mut Diags) {
    d.push(Diagnostic { kind: DiagnosticKind::Invalid, path, message: MSG_INVALID });
}

fn conflict(path: Vec<String>, d: &mut Diags) {
    d.push(Diagnostic { kind: DiagnosticKind::Conflict, path, message: MSG_CONFLICT });
}

fn unsupported_if_present(value: &Obj, key: &str, path: Vec<String>, d: &mut Diags) {
    if value.contains_key(key) {
        d.push(Diagnostic { kind: DiagnosticKind::Unsupported, path, message: MSG_UNSUPPORTED });
    }
}

/// `decodeValue`: decode with a custom decoder, emitting `invalid` on failure.
fn decode_value<T>(f: impl Fn(&Value) -> Option<T>, value: &Value, path: Vec<String>, d: &mut Diags) -> Option<T> {
    let out = f(value);
    if out.is_none() {
        invalid(path, d);
    }
    out
}

/// `decodeEncoded`: decode + encode (canonical JSON).
fn decode_encoded<T: Decode + Serialize>(value: &Value, path: Vec<String>, d: &mut Diags) -> Option<Value> {
    decode_value(T::decode, value, path, d).map(|v| encode(&v))
}

fn decode_encoded_with<T: Serialize>(
    f: impl Fn(&Value) -> Option<T>,
    value: &Value,
    path: Vec<String>,
    d: &mut Diags,
) -> Option<Value> {
    decode_value(f, value, path, d).map(|v| encode(&v))
}

/// `canonical`: decode then encode (the value is known to be valid).
fn canonical<T: Decode + Serialize>(value: Value) -> Value {
    T::decode(&value).map(|v| encode(&v)).unwrap_or(value)
}

/// `decodeMap`: undefined → `{}`; non-record → invalid; per-entry recovery.
fn decode_map<T>(
    value: Option<&Value>,
    path: &[String],
    d: &mut Diags,
    f: impl Fn(&Value, Vec<String>, &mut Diags) -> Option<T>,
) -> Vec<(String, T)> {
    let Some(value) = value else { return vec![] };
    let Some(o) = record(value) else {
        invalid(path.to_vec(), d);
        return vec![];
    };
    o.iter().filter_map(|(name, raw)| f(raw, with(path, name), d).map(|v| (name.clone(), v))).collect()
}

/// `decodeList`: undefined → `[]`; non-array → invalid; per-item recovery (index path).
fn decode_list<T>(
    value: Option<&Value>,
    path: &[String],
    d: &mut Diags,
    f: impl Fn(&Value, Vec<String>, &mut Diags) -> Option<T>,
) -> Vec<T> {
    let Some(value) = value else { return vec![] };
    let Some(items) = value.as_array() else {
        invalid(path.to_vec(), d);
        return vec![];
    };
    items.iter().enumerate().filter_map(|(i, item)| f(item, with(path, &i.to_string()), d)).collect()
}

fn to_obj(entries: Vec<(String, Value)>) -> Obj {
    let mut out = Obj::new();
    for (k, v) in entries {
        out.insert(k, v);
    }
    out
}

/// `mergeMaps(legacy, native)`: union, native entry wins per name, conflict per differing name.
fn merge_maps(legacy: Obj, native: Obj, path: &[String], d: &mut Diags) -> Obj {
    let mut result = legacy;
    for (name, value) in native {
        if let Some(existing) = result.get(&name)
            && !deep_equal(existing, &value)
        {
            conflict(with(path, &name), d);
        }
        result.insert(name, value);
    }
    result
}

fn merge_map(target: &mut Obj, key: &str, legacy: Obj, native: Obj, present: bool, d: &mut Diags) {
    let merged = merge_maps(legacy, native, &p(&[key]), d);
    if present {
        target.insert(key.into(), Value::Object(merged));
    }
}

/// `overlay`: later value wins, conflict if differs.
fn overlay(target: &mut Obj, key: &str, value: Value, path: Vec<String>, d: &mut Diags) {
    if let Some(existing) = target.get(key)
        && !deep_equal(existing, &value)
    {
        conflict(path, d);
    }
    target.insert(key.into(), value);
}

/// `prefer(legacy, native)`: native wins; conflict only if both defined and not deep-equal.
fn prefer(legacy: Option<Value>, native: Option<Value>, path: Vec<String>, d: &mut Diags) -> Option<Value> {
    let Some(native) = native else { return legacy };
    if let Some(legacy) = &legacy
        && !deep_equal(legacy, &native)
    {
        conflict(path, d);
    }
    Some(native)
}

fn is_record(v: Option<&Value>) -> bool {
    v.is_some_and(Value::is_object)
}

fn is_array(v: Option<&Value>) -> bool {
    v.is_some_and(Value::is_array)
}

/// Normalize one parsed configuration document.
pub fn normalize(input: &Value) -> NormalizeResult {
    let Some(input) = input.as_object() else {
        return NormalizeResult::Rejected {
            diagnostics: vec![Diagnostic { kind: DiagnosticKind::Invalid, path: p(&["$"]), message: MSG_ROOT }],
        };
    };
    let mut owned = Value::Object(input.clone());
    js_numbers(&mut owned);
    let input = owned.as_object().unwrap();

    let mut d: Diags = Vec::new();
    let mut encoded = Obj::new();
    for key in UNSUPPORTED_TOP_LEVEL {
        unsupported_if_present(input, key, p(&[key]), &mut d);
    }

    let legacy_snapshots = input.get("snapshot").and_then(|v| decode_encoded::<bool>(v, p(&["snapshot"]), &mut d));
    let legacy_update = input.get("autoupdate").and_then(|v| {
        decode_value(
            |v| match v {
                Value::Bool(_) => Some(v.clone()),
                Value::String(s) if s == "notify" => Some(v.clone()),
                _ => None,
            },
            v,
            p(&["autoupdate"]),
            &mut d,
        )
    });
    let native_update = input.get("update").and_then(|v| decode_encoded::<UpdatePolicy>(v, p(&["update"]), &mut d));
    let legacy_share = input
        .get("autoshare")
        .and_then(|v| decode_value(Value::as_bool, v, p(&["autoshare"]), &mut d))
        .and_then(|b| b.then(|| json!("auto")));
    let legacy_media = input.get("attachment").and_then(|v| decode_encoded::<Media>(v, p(&["attachment"]), &mut d));
    if let Some(m) = legacy_media {
        encoded.insert("media".into(), m);
    }
    if let Some(s) = legacy_snapshots {
        encoded.insert("snapshots".into(), s);
    }
    let migrated_update = match &legacy_update {
        Some(Value::Bool(false)) => Some(json!("disable")),
        Some(Value::String(_)) => Some(json!("notify")),
        Some(Value::Bool(true)) => Some(json!("auto")),
        _ => None,
    };
    if let Some(update) = prefer(migrated_update, native_update, p(&["update"]), &mut d) {
        encoded.insert("update".into(), update);
    }
    if let Some(share) = legacy_share {
        encoded.insert("share".into(), share);
    }

    // references
    let legacy_refs = decode_map(input.get("reference"), &p(&["reference"]), &mut d, |v, path, d| {
        decode_encoded::<Reference>(v, path, d)
    });
    let native_refs = decode_map(input.get("references"), &p(&["references"]), &mut d, |v, path, d| {
        decode_encoded::<Reference>(v, path, d)
    });
    let present = is_record(input.get("reference")) || is_record(input.get("references"));
    merge_map(&mut encoded, "references", to_obj(legacy_refs), to_obj(native_refs), present, &mut d);

    // commands
    let legacy_commands =
        decode_map(input.get("command"), &p(&["command"]), &mut d, |v, path, d| {
            decode_value(legacy::decode_command, v, path, d)
        });
    diagnose_selection_map(input.get("command"), &p(&["command"]), &mut d);
    let migrated_commands: Vec<(String, Value)> = legacy_commands
        .into_iter()
        .map(|(k, c)| (k, canonical::<Command>(legacy::migrate_command(&c))))
        .collect();
    let native_commands = decode_map(input.get("commands"), &p(&["commands"]), &mut d, |v, path, d| {
        decode_encoded::<Command>(v, path, d)
    });
    let present = is_record(input.get("command")) || is_record(input.get("commands"));
    merge_map(&mut encoded, "commands", to_obj(migrated_commands), to_obj(native_commands), present, &mut d);

    // agents
    let mut legacy_agents = to_obj(
        decode_map(input.get("agent"), &p(&["agent"]), &mut d, |v, path, d| {
            decode_value(legacy::decode_agent, v, path, d)
        })
        .into_iter()
        .map(|(k, a)| (k, canonical::<Agent>(legacy::migrate_agent(&a))))
        .collect(),
    );
    let legacy_small_model =
        input.get("small_model").and_then(|v| decode_value(|v| v.as_str().map(str::to_string), v, p(&["small_model"]), &mut d));
    let migrated_small_model = legacy::model_selection(legacy_small_model.as_deref(), None);
    if legacy_small_model.as_deref().is_some_and(|s| !s.is_empty()) && migrated_small_model.is_none() {
        d.push(Diagnostic {
            kind: DiagnosticKind::Unsupported,
            path: p(&["small_model"]),
            message: MSG_UNSUPPORTED_MODEL,
        });
    }
    if let Some(model) = &migrated_small_model {
        let mut title = Obj::new();
        title.insert("model".into(), model.clone());
        if let Some(Value::Object(existing)) = legacy_agents.get("title") {
            for (k, v) in existing {
                title.insert(k.clone(), v.clone());
            }
        }
        legacy_agents.insert("title".into(), Value::Object(title));
    }
    let mode_agents = to_obj(
        decode_map(input.get("mode"), &p(&["mode"]), &mut d, |v, path, d| {
            decode_value(legacy::decode_agent, v, path, d)
        })
        .into_iter()
        .map(|(k, mut a)| {
            a.mode = Some("primary".into());
            (k, canonical::<Agent>(legacy::migrate_agent(&a)))
        })
        .collect(),
    );
    let migrated_agents = merge_maps(legacy_agents, mode_agents, &p(&["agents"]), &mut d);
    let native_agents = decode_map(input.get("agents"), &p(&["agents"]), &mut d, |v, path, d| {
        decode_encoded::<Agent>(v, path, d)
    });
    diagnose_agent_unsupported(input.get("agent"), &p(&["agent"]), &mut d);
    diagnose_agent_unsupported(input.get("mode"), &p(&["mode"]), &mut d);
    let present = migrated_small_model.is_some()
        || is_record(input.get("agent"))
        || is_record(input.get("mode"))
        || is_record(input.get("agents"));
    merge_map(&mut encoded, "agents", migrated_agents, to_obj(native_agents), present, &mut d);

    // providers
    let legacy_providers = migrate_providers(input.get("provider"), &mut d);
    let native_providers = decode_map(input.get("providers"), &p(&["providers"]), &mut d, |v, path, d| {
        decode_encoded::<Provider>(v, path, d)
    });
    let present = is_record(input.get("provider")) || is_record(input.get("providers"));
    merge_map(&mut encoded, "providers", legacy_providers, to_obj(native_providers), present, &mut d);

    // permissions
    let tool_rules = migrate_tools(input.get("tools"), &mut d);
    let permission_rules = migrate_permissions(input.get("permission"), &mut d);
    let native_permissions = decode_list(input.get("permissions"), &p(&["permissions"]), &mut d, |v, path, d| {
        decode_encoded::<PermissionRule>(v, path, d)
    });
    let mut permissions = tool_rules;
    permissions.extend(permission_rules);
    permissions.extend(native_permissions);
    if !permissions.is_empty() || is_array(input.get("permissions")) {
        encoded.insert("permissions".into(), Value::Array(permissions));
    }

    // plugins
    let legacy_plugins = decode_list(input.get("plugin"), &p(&["plugin"]), &mut d, |v, path, d| {
        decode_value(legacy::decode_plugin_spec, v, path, d)
    });
    let native_plugins = decode_list(input.get("plugins"), &p(&["plugins"]), &mut d, |v, path, d| {
        decode_encoded::<Plugin>(v, path, d)
    });
    if !legacy_plugins.is_empty()
        || !native_plugins.is_empty()
        || is_array(input.get("plugin"))
        || is_array(input.get("plugins"))
    {
        let mut plugins = legacy_plugins;
        plugins.extend(native_plugins);
        encoded.insert("plugins".into(), Value::Array(plugins));
    }

    normalize_skills(input, &mut encoded, &mut d);
    normalize_mcp(input, &mut encoded, &mut d);
    normalize_compaction(input, &mut encoded, &mut d);
    normalize_policies(input, &mut encoded, &mut d);
    normalize_experimental(input, &mut encoded, &mut d);
    normalize_watcher(input, &mut encoded, &mut d);
    normalize_formatter(input, &mut encoded, &mut d);
    normalize_lsp(input, &mut encoded, &mut d);

    if let Some(model) =
        input.get("model").and_then(|v| decode_value(ModelSelection::decode, v, p(&["model"]), &mut d))
    {
        let model = ModelSelection { provider_id: legacy::provider_id(&model.provider_id), ..model };
        let mut out = Obj::new();
        // `{ ...model, providerID }` keeps the decoded key order.
        out.insert("providerID".into(), json!(model.provider_id));
        out.insert("model".into(), json!(model.model));
        if let Some(v) = &model.variant {
            out.insert("variant".into(), json!(v));
        }
        encoded.insert("model".into(), Value::Object(out));
    }

    type Atomic = fn(&Value, Vec<String>, &mut Diags) -> Option<Value>;
    let native_atomic: [(&str, Atomic); 13] = [
        ("$schema", |v, p, d| decode_encoded::<String>(v, p, d)),
        ("shell", |v, p, d| decode_encoded::<String>(v, p, d)),
        ("default_agent", |v, p, d| decode_encoded::<String>(v, p, d)),
        ("share", |v, p, d| decode_encoded::<Share>(v, p, d)),
        ("enterprise", |v, p, d| decode_encoded::<Enterprise>(v, p, d)),
        ("username", |v, p, d| decode_encoded::<String>(v, p, d)),
        ("timezone", |v, p, d| decode_encoded::<String>(v, p, d)),
        ("snapshots", |v, p, d| decode_encoded::<bool>(v, p, d)),
        ("media", |v, p, d| decode_encoded::<Media>(v, p, d)),
        ("tool_output", |v, p, d| decode_encoded::<ToolOutput>(v, p, d)),
        ("websearch", |v, p, d| decode_encoded::<WebSearch>(v, p, d)),
        ("worktree", |v, p, d| decode_encoded::<Worktree>(v, p, d)),
        ("warming", |v, p, d| decode_encoded::<Warming>(v, p, d)),
    ];
    for (key, f) in native_atomic {
        let Some(raw) = input.get(key) else { continue };
        let Some(value) = f(raw, p(&[key]), &mut d) else { continue };
        overlay(&mut encoded, key, value, p(&[key]), &mut d);
    }

    let instructions = decode_list(input.get("instructions"), &p(&["instructions"]), &mut d, |v, path, d| {
        decode_encoded::<String>(v, path, d)
    });
    if !instructions.is_empty() || is_array(input.get("instructions")) {
        encoded.insert("instructions".into(), Value::Array(instructions));
    }

    NormalizeResult::Normalized { encoded: Value::Object(encoded), diagnostics: d }
}

fn normalize_skills(input: &Obj, encoded: &mut Obj, d: &mut Diags) {
    let Some(skills) = input.get("skills") else { return };
    let string = |v: &Value, path: Vec<String>, d: &mut Diags| decode_encoded::<String>(v, path, d);
    if skills.is_array() {
        let list = decode_list(Some(skills), &p(&["skills"]), d, string);
        encoded.insert("skills".into(), Value::Array(list));
        return;
    }
    let Some(o) = record(skills) else {
        invalid(p(&["skills"]), d);
        return;
    };
    let mut list = decode_list(o.get("paths"), &p(&["skills", "paths"]), d, string);
    list.extend(decode_list(o.get("urls"), &p(&["skills", "urls"]), d, string));
    encoded.insert("skills".into(), Value::Array(list));
}

fn is_direct_legacy_mcp(value: &Value) -> bool {
    record(value).is_some_and(|o| matches!(o.get("type").and_then(Value::as_str), Some("local") | Some("remote")))
}

fn is_enabled_only_mcp(value: &Value) -> bool {
    record(value).is_some_and(|o| !o.contains_key("type") && o.get("enabled").is_some_and(Value::is_boolean))
}

fn normalize_mcp(input: &Obj, encoded: &mut Obj, d: &mut Diags) {
    let mut legacy_servers = Obj::new();
    let mut native_servers = Obj::new();
    let mut timeout = Obj::new();
    // V1 never applied this to connecting, unlike a per-server timeout.
    if let Some(Value::Object(experimental)) = input.get("experimental")
        && let Some(raw) = experimental.get("mcp_timeout")
        && let Some(value) =
            decode_encoded_with(positive_int, raw, p(&["experimental", "mcp_timeout"]), d)
    {
        timeout.insert("catalog".into(), value.clone());
        timeout.insert("execution".into(), value);
    }
    if let Some(mcp) = input.get("mcp") {
        match record(mcp) {
            None => invalid(p(&["mcp"]), d),
            Some(mcp) => {
                for (name, value) in mcp {
                    let path = p(&["mcp", name]);
                    if is_enabled_only_mcp(value) {
                        d.push(Diagnostic {
                            kind: DiagnosticKind::Unsupported,
                            path,
                            message: MSG_ENABLED_ONLY_MCP,
                        });
                        continue;
                    }
                    if name == "servers" && !is_direct_legacy_mcp(value) {
                        let value = &expand_server_timeouts(value);
                        for (key, server) in decode_map(Some(value), &path, d, |v, path, d| {
                            decode_encoded::<McpServer>(v, path, d)
                        }) {
                            native_servers.insert(key, server);
                        }
                        continue;
                    }
                    if name == "timeout" && !is_direct_legacy_mcp(value) {
                        let value = match value {
                            Value::Number(_) => legacy::migrate_mcp_timeout(value),
                            _ => value.clone(),
                        };
                        normalize_mcp_timeout(&value, &mut timeout, &path, d);
                        continue;
                    }
                    if let Some(server) = decode_value(legacy::decode_mcp, value, path, d) {
                        legacy_servers.insert(name.clone(), canonical::<McpServer>(legacy::migrate_mcp(&server)));
                    }
                }
            }
        }
    }
    let servers = merge_maps(legacy_servers, native_servers, &p(&["mcp", "servers"]), d);
    if servers.is_empty() && timeout.is_empty() {
        if let Some(Value::Object(m)) = input.get("mcp")
            && m.is_empty()
        {
            encoded.insert("mcp".into(), json!({}));
        }
        return;
    }
    let mut out = Obj::new();
    if !timeout.is_empty() {
        out.insert("timeout".into(), Value::Object(timeout));
    }
    if !servers.is_empty() {
        out.insert("servers".into(), Value::Object(servers));
    }
    encoded.insert("mcp".into(), Value::Object(out));
}

/// Native servers accept the V1 bare-number timeout so one V1-shaped field does not drop the whole server.
fn expand_server_timeouts(value: &Value) -> Value {
    let Some(servers) = record(value) else { return value.clone() };
    Value::Object(
        servers
            .iter()
            .map(|(name, server)| {
                let server = match record(server) {
                    Some(o) if o.get("timeout").is_some_and(Value::is_number) => {
                        let mut o = o.clone();
                        let timeout = legacy::migrate_mcp_timeout(&o["timeout"]);
                        o.insert("timeout".into(), timeout);
                        Value::Object(o)
                    }
                    _ => server.clone(),
                };
                (name.clone(), server)
            })
            .collect(),
    )
}

fn normalize_mcp_timeout(value: &Value, timeout: &mut Obj, path: &[String], d: &mut Diags) {
    let Some(o) = record(value) else {
        invalid(path.to_vec(), d);
        return;
    };
    let recognized: Vec<&str> =
        crate::schema::MCP_TIMEOUT_FIELDS.iter().copied().filter(|k| o.contains_key(*k)).collect();
    if !o.is_empty() && recognized.is_empty() {
        invalid(path.to_vec(), d);
        return;
    }
    for key in recognized {
        let leaf_path = with(path, key);
        let Some(leaf) = decode_encoded_with(positive_int, &o[key], leaf_path.clone(), d) else { continue };
        overlay(timeout, key, leaf, leaf_path, d);
    }
    let _ = McpTimeout::default();
}

fn normalize_compaction(input: &Obj, encoded: &mut Obj, d: &mut Diags) {
    let Some(compaction) = input.get("compaction") else { return };
    let Some(c) = record(compaction) else {
        invalid(p(&["compaction"]), d);
        return;
    };
    unsupported_if_present(c, "tail_turns", p(&["compaction", "tail_turns"]), d);
    unsupported_if_present(c, "prune", p(&["compaction", "prune"]), d);
    let mut result = Obj::new();
    if let Some(v) = c.get("auto")
        && let Some(value) = decode_encoded::<bool>(v, p(&["compaction", "auto"]), d)
    {
        result.insert("auto".into(), value);
    }
    let legacy_tokens = c
        .get("preserve_recent_tokens")
        .and_then(|v| decode_encoded_with(non_negative_int, v, p(&["compaction", "preserve_recent_tokens"]), d));
    let native_keep = c.get("keep").and_then(record);
    if c.contains_key("keep") && native_keep.is_none() {
        invalid(p(&["compaction", "keep"]), d);
    }
    let native_tokens = native_keep
        .and_then(|k| k.get("tokens"))
        .and_then(|v| decode_encoded_with(non_negative_int, v, p(&["compaction", "keep", "tokens"]), d));
    if let Some(tokens) = prefer(legacy_tokens, native_tokens, p(&["compaction", "keep", "tokens"]), d) {
        result.insert("keep".into(), json!({ "tokens": tokens }));
    }
    let legacy_buffer =
        c.get("reserved").and_then(|v| decode_encoded_with(non_negative_int, v, p(&["compaction", "reserved"]), d));
    let native_buffer =
        c.get("buffer").and_then(|v| decode_encoded_with(non_negative_int, v, p(&["compaction", "buffer"]), d));
    if let Some(buffer) = prefer(legacy_buffer, native_buffer, p(&["compaction", "buffer"]), d) {
        result.insert("buffer".into(), buffer);
    }
    if !result.is_empty() || c.is_empty() {
        encoded.insert("compaction".into(), Value::Object(result));
    }
    let _ = Compaction::default();
}

struct ProviderList {
    present: bool,
    non_empty: bool,
    values: Vec<String>,
}

fn decode_provider_list(input: &Obj, key: &str, d: &mut Diags) -> ProviderList {
    let Some(value) = input.get(key) else {
        return ProviderList { present: false, non_empty: false, values: vec![] };
    };
    let Some(items) = value.as_array() else {
        invalid(p(&[key]), d);
        return ProviderList { present: true, non_empty: true, values: vec![] };
    };
    ProviderList {
        present: true,
        non_empty: !items.is_empty(),
        values: decode_list(Some(value), &p(&[key]), d, |v, path, d| {
            decode_value(|v| v.as_str().map(str::to_string), v, path, d)
        }),
    }
}

// V1 provider lists and `experimental.policies` stay supported and merge into
// top-level `policies`; within a document, later sources win ties.
fn normalize_policies(input: &Obj, encoded: &mut Obj, d: &mut Diags) {
    let mut generated: Vec<Value> = Vec::new();
    let enabled = decode_provider_list(input, "enabled_providers", d);
    if enabled.present && (!enabled.non_empty || !enabled.values.is_empty()) {
        generated.push(json!({ "action": "provider.use", "resource": "*", "effect": "deny" }));
        for resource in &enabled.values {
            generated.push(
                json!({ "action": "provider.use", "resource": legacy::provider_id(resource), "effect": "allow" }),
            );
        }
    }
    let disabled = decode_provider_list(input, "disabled_providers", d);
    for resource in &disabled.values {
        generated
            .push(json!({ "action": "provider.use", "resource": legacy::provider_id(resource), "effect": "deny" }));
    }
    let policy = |v: &Value, path: Vec<String>, d: &mut Diags| decode_encoded::<Policy>(v, path, d);
    let experimental = match input.get("experimental") {
        Some(Value::Object(e)) => decode_list(e.get("policies"), &p(&["experimental", "policies"]), d, policy),
        _ => vec![],
    };
    let native = decode_list(input.get("policies"), &p(&["policies"]), d, policy);
    let experimental_array = matches!(input.get("experimental"), Some(Value::Object(e)) if is_array(e.get("policies")));
    if !generated.is_empty()
        || !experimental.is_empty()
        || !native.is_empty()
        || is_array(input.get("policies"))
        || experimental_array
    {
        generated.extend(experimental);
        generated.extend(native);
        encoded.insert("policies".into(), Value::Array(generated));
    }
}

fn normalize_experimental(input: &Obj, encoded: &mut Obj, d: &mut Diags) {
    let mut result = Obj::new();
    if let Some(experimental) = input.get("experimental") {
        match record(experimental) {
            None => invalid(p(&["experimental"]), d),
            Some(e) => {
                for key in UNSUPPORTED_EXPERIMENTAL {
                    unsupported_if_present(e, key, p(&["experimental", key]), d);
                }
                for key in ["portable_shell_scanner", "todo", "background_compaction", "media_blob"] {
                    if let Some(v) = e.get(key)
                        && let Some(value) = decode_encoded::<bool>(v, p(&["experimental", key]), d)
                    {
                        result.insert(key.into(), value);
                    }
                }
                if let Some(v) = e.get("image_generate")
                    && let Some(value) = decode_encoded::<crate::schema::ImageGenerate>(v, p(&["experimental", "image_generate"]), d)
                {
                    result.insert("image_generate".into(), value);
                }
                if let Some(v) = e.get("schedules")
                    && let Some(value) = decode_encoded::<bool>(v, p(&["experimental", "schedules"]), d)
                {
                    result.insert("schedules".into(), value);
                }
                if let Some(v) = e.get("subagent_depth")
                    && let Some(value) =
                        decode_encoded_with(non_negative_int, v, p(&["experimental", "subagent_depth"]), d)
                {
                    result.insert("subagent_depth".into(), value);
                }
                if let Some(v) = e.get("location_idle_timeout")
                    && let Some(value) =
                        decode_encoded::<IdleTimeout>(v, p(&["experimental", "location_idle_timeout"]), d)
                {
                    result.insert("location_idle_timeout".into(), value);
                }
            }
        }
    }
    let empty_input = matches!(input.get("experimental"), Some(Value::Object(e)) if e.is_empty());
    if !result.is_empty() || empty_input {
        encoded.insert("experimental".into(), Value::Object(result));
    }
    let _ = Experimental::default();
}

fn normalize_watcher(input: &Obj, encoded: &mut Obj, d: &mut Diags) {
    let Some(watcher) = input.get("watcher") else { return };
    let Some(w) = record(watcher) else {
        invalid(p(&["watcher"]), d);
        return;
    };
    let ignore = decode_list(w.get("ignore"), &p(&["watcher", "ignore"]), d, |v, path, d| {
        decode_encoded::<String>(v, path, d)
    });
    let value = if !ignore.is_empty() || is_array(w.get("ignore")) { json!({ "ignore": ignore }) } else { json!({}) };
    encoded.insert("watcher".into(), value);
    let _ = Watcher::default();
}

fn normalize_formatter(input: &Obj, encoded: &mut Obj, d: &mut Diags) {
    let Some(formatter) = input.get("formatter") else { return };
    if formatter.is_boolean() {
        if let Some(v) = decode_encoded::<Formatter>(formatter, p(&["formatter"]), d) {
            encoded.insert("formatter".into(), v);
        }
        return;
    }
    let entries = decode_map(Some(formatter), &p(&["formatter"]), d, |v, path, d| {
        decode_encoded::<FormatterEntry>(v, path, d)
    });
    if let Some(o) = record(formatter)
        && (o.is_empty() || !entries.is_empty())
    {
        encoded.insert("formatter".into(), Value::Object(to_obj(entries)));
    }
}

fn normalize_lsp(input: &Obj, encoded: &mut Obj, d: &mut Diags) {
    let Some(lsp) = input.get("lsp") else { return };
    if lsp.is_boolean() {
        if let Some(v) = decode_encoded::<Lsp>(lsp, p(&["lsp"]), d) {
            encoded.insert("lsp".into(), v);
        }
        return;
    }
    let entries = decode_map(Some(lsp), &p(&["lsp"]), d, |v, path, d| decode_encoded::<LspEntry>(v, path, d));
    if let Some(o) = record(lsp)
        && (o.is_empty() || !entries.is_empty())
    {
        encoded.insert("lsp".into(), Value::Object(to_obj(entries)));
    }
}

fn migrate_tools(value: Option<&Value>, d: &mut Diags) -> Vec<Value> {
    let Some(value) = value else { return vec![] };
    let Some(o) = record(value) else {
        invalid(p(&["tools"]), d);
        return vec![];
    };
    o.iter()
        .filter_map(|(action, raw)| {
            let enabled = decode_value(Value::as_bool, raw, p(&["tools", action]), d)?;
            Some(json!({
                "action": legacy::normalize_action(action),
                "resource": "*",
                "effect": if enabled { "allow" } else { "deny" },
            }))
        })
        .collect()
}

fn migrate_permissions(value: Option<&Value>, d: &mut Diags) -> Vec<Value> {
    let Some(value) = value else { return vec![] };
    if value.is_string() {
        return decode_value(legacy::decode_action, value, p(&["permission"]), d)
            .map(|effect| vec![json!({ "action": "*", "resource": "*", "effect": effect })])
            .unwrap_or_default();
    }
    let Some(o) = record(value) else {
        invalid(p(&["permission"]), d);
        return vec![];
    };
    let mut out = Vec::new();
    for (action, raw) in o {
        if raw.is_string() {
            if let Some(effect) = decode_value(legacy::decode_action, raw, p(&["permission", action]), d) {
                out.push(json!({ "action": legacy::normalize_action(action), "resource": "*", "effect": effect }));
            }
            continue;
        }
        let Some(entries) = record(raw) else {
            invalid(p(&["permission", action]), d);
            continue;
        };
        for (index, (resource, effect)) in entries.iter().enumerate() {
            let path = p(&["permission", action, &index.to_string()]);
            if let Some(effect) = decode_value(legacy::decode_action, effect, path, d) {
                out.push(json!({ "action": legacy::normalize_action(action), "resource": resource, "effect": effect }));
            }
        }
    }
    out
}

fn migrate_providers(value: Option<&Value>, d: &mut Diags) -> Obj {
    let Some(value) = value else { return Obj::new() };
    let Some(o) = record(value) else {
        invalid(p(&["provider"]), d);
        return Obj::new();
    };
    let mut candidates: Vec<(String, String, Value)> = Vec::new();
    for (name, raw) in o {
        let path = p(&["provider", name]);
        diagnose_provider_unsupported(raw, &path, d);
        if invalid_provider_overlays(raw, &path, d) {
            continue;
        }
        let Some(provider) = decode_value(legacy::decode_provider, raw, path, d) else { continue };
        let destination = legacy::provider_id(name);
        candidates.push((name.clone(), destination, canonical::<Provider>(legacy::migrate_provider(name, &provider))));
    }
    let current: Vec<String> =
        candidates.iter().filter(|(name, dest, _)| name == dest).map(|(_, dest, _)| dest.clone()).collect();
    let mut result = Obj::new();
    for (name, destination, provider) in candidates {
        if name != destination && current.contains(&destination) {
            continue;
        }
        result.insert(destination, provider);
    }
    result
}

fn invalid_provider_overlays(value: &Value, path: &[String], d: &mut Diags) -> bool {
    let Some(options) = record(value).and_then(|o| o.get("options")).and_then(record) else { return false };
    let headers_invalid = options.get("headers").is_some_and(|h| match h {
        Value::Object(m) => m.values().any(|v| !v.is_string()),
        _ => true,
    });
    let body_invalid = options.get("body").is_some_and(|b| !b.is_object());
    if headers_invalid {
        invalid([path, &p(&["options", "headers"])].concat(), d);
    }
    if body_invalid {
        invalid([path, &p(&["options", "body"])].concat(), d);
    }
    headers_invalid || body_invalid
}

fn diagnose_provider_unsupported(value: &Value, path: &[String], d: &mut Diags) {
    let Some(o) = record(value) else { return };
    for key in UNSUPPORTED_PROVIDER {
        unsupported_if_present(o, key, with(path, key), d);
    }
    let Some(models) = o.get("models").and_then(record) else { return };
    for (name, model) in models {
        let Some(m) = record(model) else { continue };
        let base = [path, &p(&["models", name])].concat();
        for key in UNSUPPORTED_MODEL {
            unsupported_if_present(m, key, with(&base, key), d);
        }
        if m.get("status").is_some_and(|s| s != "deprecated") {
            unsupported_if_present(m, "status", with(&base, "status"), d);
        }
        if m.get("interleaved").is_some_and(Value::is_boolean) {
            unsupported_if_present(m, "interleaved", with(&base, "interleaved"), d);
        }
    }
}

fn diagnose_agent_unsupported(value: Option<&Value>, path: &[String], d: &mut Diags) {
    let Some(o) = value.and_then(record) else { return };
    for (name, agent) in o {
        let Some(a) = record(agent) else { continue };
        let base = with(path, name);
        unsupported_if_present(a, "name", with(&base, "name"), d);
        diagnose_selection(a, &base, d);
    }
}

fn diagnose_selection_map(value: Option<&Value>, path: &[String], d: &mut Diags) {
    let Some(o) = value.and_then(record) else { return };
    for (name, entry) in o {
        if let Some(e) = record(entry) {
            diagnose_selection(e, &with(path, name), d);
        }
    }
}

fn diagnose_selection(value: &Obj, path: &[String], d: &mut Diags) {
    let model = value.get("model").and_then(Value::as_str);
    let model_valid = model.is_some_and(legacy::is_legacy_model_ref);
    if model.is_some() && !model_valid {
        d.push(Diagnostic {
            kind: DiagnosticKind::Unsupported,
            path: with(path, "model"),
            message: MSG_UNSUPPORTED_MODEL,
        });
    }
    if let Some(variant) = value.get("variant").and_then(Value::as_str)
        && (!model_valid || variant.is_empty() || variant.contains('#'))
    {
        d.push(Diagnostic {
            kind: DiagnosticKind::Unsupported,
            path: with(path, "variant"),
            message: MSG_UNSUPPORTED_VARIANT,
        });
    }
}
