//! Typed v2 location configuration schema (`packages/schema/src/config.ts` + `config/**`).
//!
//! Field order matches the original schema declaration order, so `serde_json::to_value`
//! produces the same canonical (encoded) JSON as `Schema.encode` in the original. Decoding is
//! implemented by [`Decode`] (Effect Schema semantics: excess properties ignored, optional keys
//! may be absent but not `null`, malformed values fail the containing value).

use std::path::PathBuf;

use indexmap::IndexMap;
use oc_schema::types::{Effect as PermissionEffect, PermissionRule};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::{Number, Value};

use crate::decode::{
    Decode, finite, int, literal, non_negative_int, opt, opt_with, positive_int, record, req, req_with,
};

pub type Ruleset = Vec<PermissionRule>;
pub type JsonRecord = IndexMap<String, Value>;
pub type StringRecord = IndexMap<String, String>;

macro_rules! literal_enum {
    ($(#[$m:meta])* $name:ident { $($variant:ident = $text:literal),+ $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name { $($variant),+ }
        impl $name {
            pub const ALL: &'static [&'static str] = &[$($text),+];
            pub fn as_str(&self) -> &'static str {
                match self { $(Self::$variant => $text),+ }
            }
            pub fn parse(text: &str) -> Option<Self> {
                match text { $($text => Some(Self::$variant),)+ _ => None }
            }
        }
        impl Decode for $name {
            fn decode(value: &Value) -> Option<Self> {
                Self::parse(value.as_str()?)
            }
        }
        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(self.as_str())
            }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

/// Implement serde `Deserialize` through [`Decode`] so consumers can use serde directly.
macro_rules! deserialize_via_decode {
    ($($name:ty),+ $(,)?) => {$(
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let value = Value::deserialize(d)?;
                <$name as Decode>::decode(&value)
                    .ok_or_else(|| serde::de::Error::custom(concat!("invalid ", stringify!($name))))
            }
        }
    )+};
}

literal_enum!(
    /// `update`: "Disable updates, notify when one is available, or install updates automatically"
    UpdatePolicy { Disable = "disable", Notify = "notify", Auto = "auto" }
);
literal_enum!(
    /// `share`
    Share { Manual = "manual", Auto = "auto", Disabled = "disabled" }
);
literal_enum!(AgentMode { Subagent = "subagent", Primary = "primary", All = "all" });
literal_enum!(McpProtocol { Legacy = "legacy", Auto = "auto", V20260728 = "2026-07-28" });
literal_enum!(Transport { Http = "http", Websocket = "websocket" });
literal_enum!(MaxTokensField { MaxCompletionTokens = "max_completion_tokens", MaxTokens = "max_tokens" });
literal_enum!(
    /// `Provider.Encoding`: HTTP request body compression; only endpoints known to decode it should enable it.
    Encoding { Zstd = "zstd", Gzip = "gzip", None = "none" }
);
literal_enum!(
    /// `Model.CacheFormat`: prompt-cache marker syntax the endpoint accepts.
    CacheFormat { Anthropic = "anthropic", Alibaba = "alibaba" }
);
literal_enum!(PolicyAction { ProviderUse = "provider.use", ToolUse = "tool.use", IntegrationUse = "integration.use" });
literal_enum!(PolicyEffect { Allow = "allow", Deny = "deny" });

// ---------------------------------------------------------------------------------------------
// Permission rules (`packages/schema/src/permission.ts`)
// ---------------------------------------------------------------------------------------------

pub fn decode_effect(value: &Value) -> Option<PermissionEffect> {
    match value.as_str()? {
        "allow" => Some(PermissionEffect::Allow),
        "deny" => Some(PermissionEffect::Deny),
        "ask" => Some(PermissionEffect::Ask),
        _ => None,
    }
}

impl Decode for PermissionRule {
    fn decode(value: &Value) -> Option<Self> {
        let o = record(value)?;
        Some(PermissionRule {
            action: req(o, "action")?,
            resource: req(o, "resource")?,
            effect: req_with(o, "effect", decode_effect)?,
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Model selection (`config/model.ts`)
// ---------------------------------------------------------------------------------------------

/// `Config.ModelSelection`: `provider/model[#variant]` string or explicit object; always encoded
/// as the object form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelSelection {
    #[serde(rename = "providerID")]
    pub provider_id: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

fn no_hash(s: &str) -> bool {
    !s.is_empty() && !s.contains('#')
}

impl ModelSelection {
    /// Short form `/^[^/#]+\/[^#]+(?:#[^#]+)?$/u` parsed like `Model.Ref.parse`.
    pub fn parse(input: &str) -> Option<Self> {
        let slash = input.find('/')?;
        let provider = &input[..slash];
        if provider.is_empty() || provider.contains('#') {
            return None;
        }
        let rest = &input[slash + 1..];
        let (model, variant) = match rest.find('#') {
            Some(i) => (&rest[..i], Some(&rest[i + 1..])),
            None => (rest, None),
        };
        if model.is_empty() {
            return None;
        }
        if let Some(v) = variant
            && !no_hash(v)
        {
            return None;
        }
        Some(ModelSelection {
            provider_id: provider.to_string(),
            model: model.to_string(),
            variant: variant.map(str::to_string),
        })
    }
}

impl Decode for ModelSelection {
    fn decode(value: &Value) -> Option<Self> {
        if let Some(s) = value.as_str() {
            return Self::parse(s);
        }
        let o = record(value)?;
        let provider_id: String = req(o, "providerID")?;
        let model: String = req(o, "model")?;
        let variant: Option<String> = opt(o, "variant")?;
        if !no_hash(&provider_id) || provider_id.contains('/') || !no_hash(&model) {
            return None;
        }
        if let Some(v) = &variant
            && !no_hash(v)
        {
            return None;
        }
        Some(ModelSelection { provider_id, model, variant })
    }
}

// ---------------------------------------------------------------------------------------------
// Agent (`config/agent.ts`)
// ---------------------------------------------------------------------------------------------

/// `Config.Provider.Request`
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct Request {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<StringRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<JsonRecord>,
}

impl Decode for Request {
    fn decode(value: &Value) -> Option<Self> {
        let o = record(value)?;
        Some(Request { headers: opt(o, "headers")?, body: opt(o, "body")? })
    }
}

/// `/^#[0-9a-fA-F]{6}$/u`
pub fn is_hex_color(s: &str) -> bool {
    s.len() == 7 && s.starts_with('#') && s[1..].chars().all(|c| c.is_ascii_hexdigit())
}

#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct Agent {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelSelection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request: Option<Request>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<AgentMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub steps: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permissions: Option<Ruleset>,
}

/// Field names of `ConfigAgent.Info` (used by Markdown v1 detection).
pub const AGENT_FIELDS: &[&str] =
    &["model", "request", "system", "description", "mode", "hidden", "color", "steps", "disabled", "permissions"];

impl Decode for Agent {
    fn decode(value: &Value) -> Option<Self> {
        let o = record(value)?;
        // An unsupported color, such as a V1 theme name, should not drop the whole agent.
        let color = o.get("color").and_then(Value::as_str).filter(|c| is_hex_color(c)).map(String::from);
        Some(Agent {
            model: opt(o, "model")?,
            request: opt(o, "request")?,
            system: opt(o, "system")?,
            description: opt(o, "description")?,
            mode: opt(o, "mode")?,
            hidden: opt(o, "hidden")?,
            color,
            steps: opt_with(o, "steps", positive_int)?,
            disabled: opt(o, "disabled")?,
            permissions: opt(o, "permissions")?,
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Formatter / LSP
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct FormatterEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<StringRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Vec<String>>,
}

impl Decode for FormatterEntry {
    fn decode(value: &Value) -> Option<Self> {
        let o = record(value)?;
        Some(FormatterEntry {
            disabled: opt(o, "disabled")?,
            command: opt(o, "command")?,
            environment: opt(o, "environment")?,
            extensions: opt(o, "extensions")?,
        })
    }
}

/// `boolean | Record<string, FormatterEntry>`
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Formatter {
    Enabled(bool),
    Entries(IndexMap<String, FormatterEntry>),
}

impl Decode for Formatter {
    fn decode(value: &Value) -> Option<Self> {
        if let Some(b) = value.as_bool() {
            return Some(Formatter::Enabled(b));
        }
        Decode::decode(value).map(Formatter::Entries)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LspServer {
    pub command: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<StringRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub initialization: Option<JsonRecord>,
}

/// `{ disabled: true } | Server`
#[derive(Debug, Clone, PartialEq)]
pub enum LspEntry {
    Disabled,
    Server(LspServer),
}

impl Serialize for LspEntry {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            LspEntry::Disabled => serde_json::json!({ "disabled": true }).serialize(s),
            LspEntry::Server(server) => server.serialize(s),
        }
    }
}

impl Decode for LspEntry {
    fn decode(value: &Value) -> Option<Self> {
        let o = record(value)?;
        if o.get("disabled") == Some(&Value::Bool(true)) {
            return Some(LspEntry::Disabled);
        }
        Some(LspEntry::Server(LspServer {
            command: req(o, "command")?,
            extensions: opt(o, "extensions")?,
            disabled: opt(o, "disabled")?,
            env: opt(o, "env")?,
            initialization: opt(o, "initialization")?,
        }))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Lsp {
    Enabled(bool),
    Entries(IndexMap<String, LspEntry>),
}

impl Decode for Lsp {
    fn decode(value: &Value) -> Option<Self> {
        if let Some(b) = value.as_bool() {
            return Some(Lsp::Enabled(b));
        }
        Decode::decode(value).map(Lsp::Entries)
    }
}

// ---------------------------------------------------------------------------------------------
// Media / tool output / watcher / enterprise / worktree / websearch
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct MediaImage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_resize: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_width: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_height: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_base64_bytes: Option<u64>,
}

impl Decode for MediaImage {
    fn decode(value: &Value) -> Option<Self> {
        let o = record(value)?;
        Some(MediaImage {
            auto_resize: opt(o, "auto_resize")?,
            max_width: opt_with(o, "max_width", positive_int)?,
            max_height: opt_with(o, "max_height", positive_int)?,
            max_base64_bytes: opt_with(o, "max_base64_bytes", positive_int)?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct Media {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<MediaImage>,
}

impl Decode for Media {
    fn decode(value: &Value) -> Option<Self> {
        let o = record(value)?;
        Some(Media { image: opt(o, "image")? })
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct ToolOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_lines: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
}

impl Decode for ToolOutput {
    fn decode(value: &Value) -> Option<Self> {
        let o = record(value)?;
        Some(ToolOutput {
            max_lines: opt_with(o, "max_lines", positive_int)?,
            max_bytes: opt_with(o, "max_bytes", positive_int)?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct Watcher {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ignore: Option<Vec<String>>,
}

impl Decode for Watcher {
    fn decode(value: &Value) -> Option<Self> {
        let o = record(value)?;
        Some(Watcher { ignore: opt(o, "ignore")? })
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct Enterprise {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

impl Decode for Enterprise {
    fn decode(value: &Value) -> Option<Self> {
        let o = record(value)?;
        Some(Enterprise { url: opt(o, "url")? })
    }
}

/// `{ directory }` where directory is `Schema.Trim` + non-empty (decoded value is trimmed).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Worktree {
    pub directory: String,
}

/// JavaScript `String.prototype.trim` whitespace set.
pub fn js_trim(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
}

impl Decode for Worktree {
    fn decode(value: &Value) -> Option<Self> {
        let o = record(value)?;
        let directory: String = req(o, "directory")?;
        let trimmed = js_trim(&directory);
        if trimmed.is_empty() {
            return None;
        }
        Some(Worktree { directory: trimmed.to_string() })
    }
}

/// `false | { provider: "random" | string }`
#[derive(Debug, Clone, PartialEq)]
pub enum WebSearch {
    Disabled,
    Provider(String),
}

impl Serialize for WebSearch {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            WebSearch::Disabled => s.serialize_bool(false),
            WebSearch::Provider(p) => serde_json::json!({ "provider": p }).serialize(s),
        }
    }
}

impl Decode for WebSearch {
    fn decode(value: &Value) -> Option<Self> {
        if value == &Value::Bool(false) {
            return Some(WebSearch::Disabled);
        }
        let o = record(value)?;
        Some(WebSearch::Provider(req(o, "provider")?))
    }
}

// ---------------------------------------------------------------------------------------------
// Warming (`config/warming.ts`) with Effect `DurationFromString`
// ---------------------------------------------------------------------------------------------

/// Effect `Duration` value as produced by `Duration.fromInput(string)`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Duration {
    /// Integral millisecond value (`0` is the zero duration).
    Millis(f64),
    Nanos(i128),
    Infinity,
    NegativeInfinity,
}

impl Duration {
    fn make_millis(input: f64) -> Duration {
        if input.is_nan() || input == 0.0 {
            return Duration::Millis(0.0);
        }
        if !input.is_finite() {
            return if input > 0.0 { Duration::Infinity } else { Duration::NegativeInfinity };
        }
        if input.fract() != 0.0 {
            return Duration::make_nanos(round_ties_away(input * 1_000_000.0));
        }
        Duration::Millis(input)
    }

    fn make_nanos(nanos: i128) -> Duration {
        if nanos == 0 { Duration::Millis(0.0) } else { Duration::Nanos(nanos) }
    }

    /// `Duration.fromInput` for strings.
    pub fn parse(input: &str) -> Option<Duration> {
        if input == "Infinity" {
            return Some(Duration::Infinity);
        }
        if input == "-Infinity" {
            return Some(Duration::NegativeInfinity);
        }
        static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
            regex::Regex::new(
                r"^(-?[0-9]+(?:\.[0-9]+)?)\s+(nanos?|micros?|millis?|seconds?|minutes?|hours?|days?|weeks?)$",
            )
            .unwrap()
        });
        let caps = RE.captures(input)?;
        let value_str = caps.get(1)?.as_str();
        let unit = caps.get(2)?.as_str();
        let parse_nanos = |scale: i128| -> Option<i128> {
            if value_str.contains('.') {
                Some(round_ties_away(value_str.parse::<f64>().ok()? * scale as f64))
            } else {
                Some(value_str.parse::<i128>().ok()? * scale)
            }
        };
        let value: f64 = value_str.parse().ok()?;
        Some(match unit {
            "nano" | "nanos" => Duration::make_nanos(parse_nanos(1)?),
            "micro" | "micros" => Duration::make_nanos(parse_nanos(1_000)?),
            "milli" | "millis" => Duration::make_millis(value),
            "second" | "seconds" => Duration::make_millis(value * 1_000.0),
            "minute" | "minutes" => Duration::make_millis(value * 60_000.0),
            "hour" | "hours" => Duration::make_millis(value * 3_600_000.0),
            "day" | "days" => Duration::make_millis(value * 86_400_000.0),
            "week" | "weeks" => Duration::make_millis(value * 604_800_000.0),
            _ => return None,
        })
    }

    pub fn minutes(n: f64) -> Duration {
        Duration::make_millis(n * 60_000.0)
    }

    /// `Duration.toMillis`
    pub fn to_millis(&self) -> f64 {
        match self {
            Duration::Millis(m) => *m,
            Duration::Nanos(n) => *n as f64 / 1_000_000.0,
            Duration::Infinity => f64::INFINITY,
            Duration::NegativeInfinity => f64::NEG_INFINITY,
        }
    }

    /// `String(duration)`
    pub fn encode(&self) -> String {
        match self {
            Duration::Infinity => "Infinity".into(),
            Duration::NegativeInfinity => "-Infinity".into(),
            Duration::Nanos(n) => format!("{n} nanos"),
            Duration::Millis(m) => format!("{} millis", crate::decode::js_number_string(*m)),
        }
    }
}

fn round_ties_away(input: f64) -> i128 {
    if input < 0.0 { (input - 0.5).ceil() as i128 } else { (input + 0.5).floor() as i128 }
}

impl Serialize for Duration {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.encode())
    }
}

impl Decode for Duration {
    fn decode(value: &Value) -> Option<Self> {
        Duration::parse(value.as_str()?)
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct WarmingInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interval: Option<Duration>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration: Option<Duration>,
}

impl Decode for WarmingInfo {
    fn decode(value: &Value) -> Option<Self> {
        let o = record(value)?;
        Some(WarmingInfo { prompt: opt(o, "prompt")?, interval: opt(o, "interval")?, duration: opt(o, "duration")? })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Warming {
    Enabled(bool),
    Settings(WarmingInfo),
}

impl Decode for Warming {
    fn decode(value: &Value) -> Option<Self> {
        if let Some(b) = value.as_bool() {
            return Some(Warming::Enabled(b));
        }
        WarmingInfo::decode(value).map(Warming::Settings)
    }
}

// ---------------------------------------------------------------------------------------------
// MCP (`packages/schema/src/mcp.ts`)
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct McpTimeout {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub startup: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub catalog: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution: Option<u64>,
}

pub const MCP_TIMEOUT_FIELDS: &[&str] = &["startup", "catalog", "execution"];

impl Decode for McpTimeout {
    fn decode(value: &Value) -> Option<Self> {
        let o = record(value)?;
        Some(McpTimeout {
            startup: opt_with(o, "startup", positive_int)?,
            catalog: opt_with(o, "catalog", positive_int)?,
            execution: opt_with(o, "execution", positive_int)?,
        })
    }
}

impl McpTimeout {
    /// `Object.assign(self, other)` for defined leaves.
    pub fn assign(&mut self, other: &McpTimeout) {
        if other.startup.is_some() {
            self.startup = other.startup;
        }
        if other.catalog.is_some() {
            self.catalog = other.catalog;
        }
        if other.execution.is_some() {
            self.execution = other.execution;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct McpOAuth {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callback_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redirect_uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_server_metadata_url: Option<String>,
}

pub fn port(value: &Value) -> Option<u16> {
    let i = int(value)?;
    (1..=65535).contains(&i).then_some(i as u16)
}

impl Decode for McpOAuth {
    fn decode(value: &Value) -> Option<Self> {
        let o = record(value)?;
        Some(McpOAuth {
            client_id: opt(o, "client_id")?,
            client_secret: opt(o, "client_secret")?,
            scope: opt(o, "scope")?,
            callback_port: opt_with(o, "callback_port", port)?,
            redirect_uri: opt(o, "redirect_uri")?,
            auth_server_metadata_url: opt(o, "auth_server_metadata_url")?,
        })
    }
}

/// `OAuthConfig | false`
#[derive(Debug, Clone, PartialEq)]
pub enum McpOAuthSetting {
    Disabled,
    Config(McpOAuth),
}

impl Serialize for McpOAuthSetting {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            McpOAuthSetting::Disabled => s.serialize_bool(false),
            McpOAuthSetting::Config(c) => c.serialize(s),
        }
    }
}

impl Decode for McpOAuthSetting {
    fn decode(value: &Value) -> Option<Self> {
        if value == &Value::Bool(false) {
            return Some(McpOAuthSetting::Disabled);
        }
        McpOAuth::decode(value).map(McpOAuthSetting::Config)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct McpLocal {
    pub command: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<StringRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codemode: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<McpTimeout>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<McpProtocol>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct McpRemote {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<StringRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oauth: Option<McpOAuthSetting>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codemode: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<McpTimeout>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<McpProtocol>,
}

/// `Mcp.ServerConfig`, discriminated by `type`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum McpServer {
    Local(McpLocal),
    Remote(McpRemote),
}

impl McpServer {
    pub fn timeout(&self) -> Option<&McpTimeout> {
        match self {
            McpServer::Local(l) => l.timeout.as_ref(),
            McpServer::Remote(r) => r.timeout.as_ref(),
        }
    }
    pub fn set_timeout(&mut self, timeout: Option<McpTimeout>) {
        match self {
            McpServer::Local(l) => l.timeout = timeout,
            McpServer::Remote(r) => r.timeout = timeout,
        }
    }
    pub fn disabled(&self) -> Option<bool> {
        match self {
            McpServer::Local(l) => l.disabled,
            McpServer::Remote(r) => r.disabled,
        }
    }
}

impl Decode for McpServer {
    fn decode(value: &Value) -> Option<Self> {
        let o = record(value)?;
        match o.get("type")?.as_str()? {
            "local" => Some(McpServer::Local(McpLocal {
                command: req(o, "command")?,
                cwd: opt(o, "cwd")?,
                environment: opt(o, "environment")?,
                disabled: opt(o, "disabled")?,
                codemode: opt(o, "codemode")?,
                timeout: opt(o, "timeout")?,
                protocol: opt(o, "protocol")?,
            })),
            "remote" => Some(McpServer::Remote(McpRemote {
                url: req(o, "url")?,
                headers: opt(o, "headers")?,
                oauth: opt(o, "oauth")?,
                disabled: opt(o, "disabled")?,
                codemode: opt(o, "codemode")?,
                timeout: opt(o, "timeout")?,
                protocol: opt(o, "protocol")?,
            })),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct Mcp {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<McpTimeout>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub servers: Option<IndexMap<String, McpServer>>,
}

impl Decode for Mcp {
    fn decode(value: &Value) -> Option<Self> {
        let o = record(value)?;
        Some(Mcp { timeout: opt(o, "timeout")?, servers: opt(o, "servers")? })
    }
}

// ---------------------------------------------------------------------------------------------
// Compaction / Command / Reference / Plugin
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct CompactionKeep {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
}

impl Decode for CompactionKeep {
    fn decode(value: &Value) -> Option<Self> {
        let o = record(value)?;
        Some(CompactionKeep { tokens: opt_with(o, "tokens", non_negative_int)? })
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct Compaction {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keep: Option<CompactionKeep>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub buffer: Option<u64>,
}

impl Decode for Compaction {
    fn decode(value: &Value) -> Option<Self> {
        let o = record(value)?;
        Some(Compaction {
            auto: opt(o, "auto")?,
            keep: opt(o, "keep")?,
            buffer: opt_with(o, "buffer", non_negative_int)?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Command {
    pub template: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelSelection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent: Option<bool>,
    /// Deprecated alias for subagent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtask: Option<bool>,
}

impl Decode for Command {
    fn decode(value: &Value) -> Option<Self> {
        let o = record(value)?;
        Some(Command {
            template: req(o, "template")?,
            description: opt(o, "description")?,
            agent: opt(o, "agent")?,
            model: opt(o, "model")?,
            subagent: opt(o, "subagent")?,
            subtask: opt(o, "subtask")?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReferenceGit {
    pub repository: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReferenceLocal {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,
}

/// `string | Git | Local`
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Reference {
    Text(String),
    Git(ReferenceGit),
    Local(ReferenceLocal),
}

impl Decode for Reference {
    fn decode(value: &Value) -> Option<Self> {
        if let Some(s) = value.as_str() {
            return Some(Reference::Text(s.to_string()));
        }
        let o = record(value)?;
        let git = (|| {
            Some(ReferenceGit {
                repository: req(o, "repository")?,
                branch: opt(o, "branch")?,
                description: opt(o, "description")?,
                hidden: opt(o, "hidden")?,
            })
        })();
        if let Some(git) = git {
            return Some(Reference::Git(git));
        }
        Some(Reference::Local(ReferenceLocal {
            path: req(o, "path")?,
            description: opt(o, "description")?,
            hidden: opt(o, "hidden")?,
        }))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PluginEntry {
    pub package: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<JsonRecord>,
}

/// `string | { package, options? }`
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Plugin {
    Spec(String),
    Entry(PluginEntry),
}

impl Decode for Plugin {
    fn decode(value: &Value) -> Option<Self> {
        if let Some(s) = value.as_str() {
            return Some(Plugin::Spec(s.to_string()));
        }
        let o = record(value)?;
        Some(Plugin::Entry(PluginEntry { package: req(o, "package")?, options: opt(o, "options")? }))
    }
}

// ---------------------------------------------------------------------------------------------
// Provider (`config/provider.ts`)
// ---------------------------------------------------------------------------------------------

/// `Provider.Compaction`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ProviderCompaction {
    Summary,
    Native,
}

impl Decode for ProviderCompaction {
    fn decode(value: &Value) -> Option<Self> {
        match record(value)?.get("type")?.as_str()? {
            "summary" => Some(ProviderCompaction::Summary),
            "native" => Some(ProviderCompaction::Native),
            _ => None,
        }
    }
}

/// `number | false`
#[derive(Debug, Clone, PartialEq)]
pub enum TimeoutSetting {
    Disabled,
    Millis(Number),
}

impl Serialize for TimeoutSetting {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            TimeoutSetting::Disabled => s.serialize_bool(false),
            TimeoutSetting::Millis(n) => n.serialize(s),
        }
    }
}

impl Decode for TimeoutSetting {
    fn decode(value: &Value) -> Option<Self> {
        if value == &Value::Bool(false) {
            return Some(TimeoutSetting::Disabled);
        }
        finite(value).map(TimeoutSetting::Millis)
    }
}

/// `Config.Provider.Settings` (struct with JSON rest).
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct Settings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<TimeoutSetting>,
    #[serde(rename = "headerTimeout", skip_serializing_if = "Option::is_none")]
    pub header_timeout: Option<TimeoutSetting>,
    #[serde(rename = "chunkTimeout", skip_serializing_if = "Option::is_none")]
    pub chunk_timeout: Option<TimeoutSetting>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compaction: Option<ProviderCompaction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transport: Option<Transport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoding: Option<Encoding>,
    #[serde(flatten)]
    pub rest: JsonRecord,
}

const SETTINGS_FIELDS: &[&str] = &["timeout", "headerTimeout", "chunkTimeout", "compaction", "transport", "encoding"];

fn rest_of(o: &crate::decode::Object, known: &[&str]) -> JsonRecord {
    o.iter().filter(|(k, _)| !known.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect()
}

impl Decode for Settings {
    fn decode(value: &Value) -> Option<Self> {
        let o = record(value)?;
        Some(Settings {
            timeout: opt(o, "timeout")?,
            header_timeout: opt(o, "headerTimeout")?,
            chunk_timeout: opt(o, "chunkTimeout")?,
            compaction: opt(o, "compaction")?,
            transport: opt(o, "transport")?,
            encoding: opt(o, "encoding")?,
            rest: rest_of(o, SETTINGS_FIELDS),
        })
    }
}

/// `Config.Model.Settings`
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct ModelSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compaction: Option<ProviderCompaction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoding: Option<Encoding>,
    #[serde(flatten)]
    pub rest: JsonRecord,
}

impl Decode for ModelSettings {
    fn decode(value: &Value) -> Option<Self> {
        let o = record(value)?;
        Some(ModelSettings {
            compaction: opt(o, "compaction")?,
            encoding: opt(o, "encoding")?,
            rest: rest_of(o, &["compaction", "encoding"]),
        })
    }
}

/// `Model.Compatibility`
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct Compatibility {
    #[serde(rename = "reasoningField", skip_serializing_if = "Option::is_none")]
    pub reasoning_field: Option<String>,
    #[serde(rename = "requireReasoning", skip_serializing_if = "Option::is_none")]
    pub require_reasoning: Option<bool>,
    #[serde(rename = "maxTokensField", skip_serializing_if = "Option::is_none")]
    pub max_tokens_field: Option<MaxTokensField>,
    #[serde(rename = "requireFinishReason", skip_serializing_if = "Option::is_none")]
    pub require_finish_reason: Option<bool>,
    #[serde(rename = "requireAssistantAfterTool", skip_serializing_if = "Option::is_none")]
    pub require_assistant_after_tool: Option<bool>,
    #[serde(rename = "supportsPromptCacheKey", skip_serializing_if = "Option::is_none")]
    pub supports_prompt_cache_key: Option<bool>,
    /// Request usage in the stream with `stream_options.include_usage`. Defaults to true.
    #[serde(rename = "streamUsage", skip_serializing_if = "Option::is_none")]
    pub stream_usage: Option<bool>,
    /// Prompt-cache marker syntax the endpoint accepts. Overrides the route default; both send
    /// `cache_control`, and `alibaba` skips tool definitions.
    #[serde(rename = "cacheFormat", skip_serializing_if = "Option::is_none")]
    pub cache_format: Option<CacheFormat>,
    #[serde(rename = "supportsThinkingBlockBinding", skip_serializing_if = "Option::is_none")]
    pub supports_thinking_block_binding: Option<bool>,
    #[serde(rename = "supportsEffortUpdates", skip_serializing_if = "Option::is_none")]
    pub supports_effort_updates: Option<bool>,
}

impl Decode for Compatibility {
    fn decode(value: &Value) -> Option<Self> {
        let o = record(value)?;
        Some(Compatibility {
            reasoning_field: opt(o, "reasoningField")?,
            require_reasoning: opt(o, "requireReasoning")?,
            max_tokens_field: opt(o, "maxTokensField")?,
            require_finish_reason: opt(o, "requireFinishReason")?,
            require_assistant_after_tool: opt(o, "requireAssistantAfterTool")?,
            supports_prompt_cache_key: opt(o, "supportsPromptCacheKey")?,
            stream_usage: opt(o, "streamUsage")?,
            cache_format: opt(o, "cacheFormat")?,
            supports_thinking_block_binding: opt(o, "supportsThinkingBlockBinding")?,
            supports_effort_updates: opt(o, "supportsEffortUpdates")?,
        })
    }
}

/// `Provider.Compatibility`: provider defaults inherited by its models. Model-version features
/// (`supportsThinkingBlockBinding`, `supportsEffortUpdates`) stay model-only.
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
#[serde(transparent)]
pub struct ProviderCompatibility(pub Compatibility);

impl Decode for ProviderCompatibility {
    fn decode(value: &Value) -> Option<Self> {
        let mut decoded = Compatibility::decode(value)?;
        decoded.supports_thinking_block_binding = None;
        decoded.supports_effort_updates = None;
        Some(ProviderCompatibility(decoded))
    }
}

impl Compatibility {
    /// `{ ...self, ...other }` for defined fields.
    pub fn overlay(&mut self, other: &Compatibility) {
        macro_rules! o {
            ($($f:ident),*) => {$( if other.$f.is_some() { self.$f = other.$f.clone(); } )*};
        }
        o!(
            reasoning_field,
            require_reasoning,
            max_tokens_field,
            require_finish_reason,
            require_assistant_after_tool,
            supports_prompt_cache_key,
            stream_usage,
            cache_format,
            supports_thinking_block_binding,
            supports_effort_updates
        );
    }
}

/// Partial `Model.Capabilities` (`Config.Model.Capabilities`).
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct Capabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<Vec<String>>,
    /// Whether the model accepts a sampling temperature. Unknown when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Reasoning>,
}

impl Decode for Capabilities {
    fn decode(value: &Value) -> Option<Self> {
        let o = record(value)?;
        Some(Capabilities {
            tools: opt(o, "tools")?,
            input: opt(o, "input")?,
            output: opt(o, "output")?,
            temperature: opt(o, "temperature")?,
            reasoning: opt(o, "reasoning")?,
        })
    }
}

/// `Config.Model.Reasoning.budget`
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct ReasoningBudget {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<i64>,
}

impl Decode for ReasoningBudget {
    fn decode(value: &Value) -> Option<Self> {
        let o = record(value)?;
        Some(ReasoningBudget { min: opt_with(o, "min", int)?, max: opt_with(o, "max", int)? })
    }
}

/// `Config.Model.Reasoning`: models.dev v2's `capabilities.reasoning`. OpenCode builds the model's
/// variants from it; no control means none.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Reasoning {
    Unsupported {
        supported: bool,
    },
    Supported {
        supported: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        toggle: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        effort: Option<Vec<String>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        budget: Option<ReasoningBudget>,
    },
}

impl Decode for Reasoning {
    fn decode(value: &Value) -> Option<Self> {
        let o = record(value)?;
        match o.get("supported")?.as_bool()? {
            false => Some(Reasoning::Unsupported { supported: false }),
            true => Some(Reasoning::Supported {
                supported: true,
                toggle: opt(o, "toggle")?,
                effort: opt(o, "effort")?,
                budget: opt(o, "budget")?,
            }),
        }
    }
}

/// `releaseDate`: `YYYY-MM-DD` or `YYYY-MM`.
fn release_date(value: &Value) -> Option<String> {
    let text = value.as_str()?;
    let bytes = text.as_bytes();
    let digits = |range: std::ops::Range<usize>| bytes[range].iter().all(u8::is_ascii_digit);
    let ok = match bytes.len() {
        7 => digits(0..4) && bytes[4] == b'-' && digits(5..7),
        10 => digits(0..4) && bytes[4] == b'-' && digits(5..7) && bytes[7] == b'-' && digits(8..10),
        _ => false,
    };
    ok.then(|| text.to_string())
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Variant {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settings: Option<ModelSettings>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<StringRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<JsonRecord>,
}

impl Decode for Variant {
    fn decode(value: &Value) -> Option<Self> {
        let o = record(value)?;
        Some(Variant { id: req(o, "id")?, settings: opt(o, "settings")?, headers: opt(o, "headers")?, body: opt(o, "body")? })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CostTier {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub size: i64,
}

impl Decode for CostTier {
    fn decode(value: &Value) -> Option<Self> {
        let o = record(value)?;
        literal(o.get("type")?, &["context"])?;
        Some(CostTier { kind: "context", size: req_with(o, "size", int)? })
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct CostCache {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read: Option<Number>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub write: Option<Number>,
}

impl Decode for CostCache {
    fn decode(value: &Value) -> Option<Self> {
        let o = record(value)?;
        Some(CostCache { read: opt_with(o, "read", finite)?, write: opt_with(o, "write", finite)? })
    }
}

/// USD per 1M tokens.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Cost {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier: Option<CostTier>,
    pub input: Number,
    pub output: Number,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache: Option<CostCache>,
}

impl Decode for Cost {
    fn decode(value: &Value) -> Option<Self> {
        let o = record(value)?;
        Some(Cost {
            tier: opt(o, "tier")?,
            input: req_with(o, "input", finite)?,
            output: req_with(o, "output", finite)?,
            cache: opt(o, "cache")?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum CostSetting {
    Single(Cost),
    Tiers(Vec<Cost>),
}

impl CostSetting {
    pub fn as_slice(&self) -> &[Cost] {
        match self {
            CostSetting::Single(c) => std::slice::from_ref(c),
            CostSetting::Tiers(c) => c,
        }
    }
}

impl Decode for CostSetting {
    fn decode(value: &Value) -> Option<Self> {
        if value.is_array() {
            return Decode::decode(value).map(CostSetting::Tiers);
        }
        Cost::decode(value).map(CostSetting::Single)
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct Limit {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<i64>,
}

impl Decode for Limit {
    fn decode(value: &Value) -> Option<Self> {
        let o = record(value)?;
        Some(Limit {
            context: opt_with(o, "context", int)?,
            input: opt_with(o, "input", int)?,
            output: opt_with(o, "output", int)?,
        })
    }
}

/// `Config.Model`
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct ProviderModel {
    #[serde(rename = "modelID", skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Release date as YYYY-MM-DD or YYYY-MM
    #[serde(rename = "releaseDate", skip_serializing_if = "Option::is_none")]
    pub release_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compatibility: Option<Compatibility>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settings: Option<ModelSettings>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<StringRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<JsonRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Capabilities>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variants: Option<Vec<Variant>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<CostSetting>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<Limit>,
}

impl Decode for ProviderModel {
    fn decode(value: &Value) -> Option<Self> {
        let o = record(value)?;
        Some(ProviderModel {
            model_id: opt(o, "modelID")?,
            family: opt(o, "family")?,
            name: opt(o, "name")?,
            release_date: opt_with(o, "releaseDate", release_date)?,
            compatibility: opt(o, "compatibility")?,
            package: opt(o, "package")?,
            settings: opt(o, "settings")?,
            headers: opt(o, "headers")?,
            body: opt(o, "body")?,
            capabilities: opt(o, "capabilities")?,
            variants: opt(o, "variants")?,
            cost: opt(o, "cost")?,
            disabled: opt(o, "disabled")?,
            limit: opt(o, "limit")?,
        })
    }
}

/// `Config.Provider`
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct Provider {
    /// Provider ID to inherit the catalog definition from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    /// Defaults for every model of this provider; model compatibility overrides individual fields.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compatibility: Option<ProviderCompatibility>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settings: Option<Settings>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<StringRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<JsonRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub models: Option<IndexMap<String, ProviderModel>>,
}

impl Decode for Provider {
    fn decode(value: &Value) -> Option<Self> {
        let o = record(value)?;
        Some(Provider {
            canonical: opt(o, "canonical")?,
            name: opt(o, "name")?,
            env: opt(o, "env")?,
            package: opt(o, "package")?,
            compatibility: opt(o, "compatibility")?,
            settings: opt(o, "settings")?,
            headers: opt(o, "headers")?,
            body: opt(o, "body")?,
            models: opt(o, "models")?,
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Experimental + policies
// ---------------------------------------------------------------------------------------------

/// `ConfigPolicy.Info`
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Policy {
    pub action: PolicyAction,
    pub resource: String,
    pub effect: PolicyEffect,
}

impl Decode for Policy {
    fn decode(value: &Value) -> Option<Self> {
        let o = record(value)?;
        Some(Policy { action: req(o, "action")?, resource: req(o, "resource")?, effect: req(o, "effect")? })
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct Experimental {
    /// Enable the experimental portable shell permission scanner. Defaults to false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub portable_shell_scanner: Option<bool>,
    /// Enable the experimental todowrite tool. Defaults to false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub todo: Option<bool>,
    /// Enable experimental background compaction (accepted; not implemented by ocd).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background_compaction: Option<bool>,
    /// Maximum subagent nesting depth. Defaults to 1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent_depth: Option<u64>,
    /// How long a location can go without session activity before its services are unloaded
    /// (accepted; ocd never unloads idle locations).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location_idle_timeout: Option<IdleTimeout>,
    /// Deprecated: use top-level policies. Ordered policies controlling access to configured resources.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policies: Option<Vec<Policy>>,
}

impl Decode for Experimental {
    fn decode(value: &Value) -> Option<Self> {
        let o = record(value)?;
        Some(Experimental {
            portable_shell_scanner: opt(o, "portable_shell_scanner")?,
            todo: opt(o, "todo")?,
            background_compaction: opt(o, "background_compaction")?,
            subagent_depth: opt_with(o, "subagent_depth", non_negative_int)?,
            location_idle_timeout: opt(o, "location_idle_timeout")?,
            policies: opt(o, "policies")?,
        })
    }
}

/// `Schema.Union([Schema.DurationFromString, Schema.Literal(false)])`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(untagged)]
pub enum IdleTimeout {
    Never(bool),
    After(Duration),
}

impl Decode for IdleTimeout {
    fn decode(value: &Value) -> Option<Self> {
        match value {
            Value::Bool(false) => Some(IdleTimeout::Never(false)),
            _ => Duration::decode(value).map(IdleTimeout::After),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Info
// ---------------------------------------------------------------------------------------------

/// `Config.Info` — one canonical location configuration document.
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct Info {
    /// JSON schema reference for configuration validation
    #[serde(rename = "$schema", skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    /// Default shell to use for terminal and shell tool execution
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    /// Default model to use when no session or agent model is selected
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelSelection>,
    /// Default primary agent to use when no session agent is selected
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update: Option<UpdatePolicy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub share: Option<Share>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enterprise: Option<Enterprise>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permissions: Option<Ruleset>,
    /// Ordered policies controlling access to providers, tools, and integrations
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policies: Option<Vec<Policy>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agents: Option<IndexMap<String, Agent>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshots: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watcher: Option<Watcher>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub formatter: Option<Formatter>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lsp: Option<Lsp>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media: Option<Media>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_output: Option<ToolOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp: Option<Mcp>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compaction: Option<Compaction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commands: Option<IndexMap<String, Command>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub references: Option<IndexMap<String, Reference>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub websearch: Option<WebSearch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugins: Option<Vec<Plugin>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree: Option<Worktree>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warming: Option<Warming>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub providers: Option<IndexMap<String, Provider>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub experimental: Option<Experimental>,
}

impl Decode for Info {
    fn decode(value: &Value) -> Option<Self> {
        let o = record(value)?;
        Some(Info {
            schema: opt(o, "$schema")?,
            shell: opt(o, "shell")?,
            model: opt(o, "model")?,
            default_agent: opt(o, "default_agent")?,
            update: opt(o, "update")?,
            share: opt(o, "share")?,
            enterprise: opt(o, "enterprise")?,
            username: opt(o, "username")?,
            permissions: opt(o, "permissions")?,
            policies: opt(o, "policies")?,
            agents: opt(o, "agents")?,
            snapshots: opt(o, "snapshots")?,
            watcher: opt(o, "watcher")?,
            formatter: opt(o, "formatter")?,
            lsp: opt(o, "lsp")?,
            media: opt(o, "media")?,
            tool_output: opt(o, "tool_output")?,
            mcp: opt(o, "mcp")?,
            compaction: opt(o, "compaction")?,
            skills: opt(o, "skills")?,
            commands: opt(o, "commands")?,
            instructions: opt(o, "instructions")?,
            references: opt(o, "references")?,
            websearch: opt(o, "websearch")?,
            plugins: opt(o, "plugins")?,
            worktree: opt(o, "worktree")?,
            warming: opt(o, "warming")?,
            providers: opt(o, "providers")?,
            experimental: opt(o, "experimental")?,
        })
    }
}

impl Info {
    /// Decode a canonical document (`Schema.decodeUnknownOption(Info, …)`).
    pub fn from_value(value: &Value) -> Option<Info> {
        Info::decode(value)
    }
    /// Encode to canonical JSON (`Schema.encode(Info)`).
    pub fn to_value(&self) -> Value {
        crate::decode::encode(self)
    }
}

deserialize_via_decode!(
    Info,
    ModelSelection,
    Agent,
    Command,
    Provider,
    ProviderModel,
    McpServer,
    McpTimeout,
    Reference,
    Plugin,
    Policy,
    Warming,
    Duration,
    Formatter,
    Lsp,
    WebSearch,
    Compaction,
    Media,
    ToolOutput,
    Worktree,
    Experimental,
    IdleTimeout,
    Settings,
);

/// `Config.Patch`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Patch {
    pub shell: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// Entries
// ---------------------------------------------------------------------------------------------

/// The integration connection that delivered a config document (`ConfigIntegration.Document`), such as the
/// OpenCode Console's workspace config. Its providers authenticate with, and only exist for, that connection,
/// and its policies are those of the organization behind it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct IntegrationSource {
    #[serde(rename = "integrationID")]
    pub integration_id: String,
    /// `Connection.Info`: `{type: "credential", id, label, method}` or `{type: "env", name}`.
    pub connection: Value,
    /// Organization behind the connection, named in denials by the document's policies.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub organization: Option<String>,
}

impl IntegrationSource {
    /// `IntegrationConnection.key(connection)`: `credential:<id>` or `env:<name>`.
    pub fn connection_key(&self) -> Option<String> {
        let c = self.connection.as_object()?;
        match c.get("type")?.as_str()? {
            "credential" => Some(format!("credential:{}", c.get("id")?.as_str()?)),
            "env" => Some(format!("env:{}", c.get("name")?.as_str()?)),
            _ => None,
        }
    }
}

/// `Config.Entry`: lowest priority first.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Entry {
    /// One parsed + normalized config document; `path` is absent for virtual sources.
    Document {
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<PathBuf>,
        info: Info,
        /// Set on a document an integration connection contributes (`ConfigIntegration.Document`).
        #[serde(flatten)]
        integration: Option<IntegrationSource>,
    },
    /// A config root directory scanned for Markdown agents/commands, plugins and skills.
    Directory { path: PathBuf },
}

impl Entry {
    pub fn document(path: Option<PathBuf>, info: Info) -> Entry {
        Entry::Document { path, info, integration: None }
    }
    /// The integration connection behind an integration document.
    pub fn integration(&self) -> Option<&IntegrationSource> {
        match self {
            Entry::Document { integration, .. } => integration.as_ref(),
            Entry::Directory { .. } => None,
        }
    }
    pub fn info(&self) -> Option<&Info> {
        match self {
            Entry::Document { info, .. } => Some(info),
            Entry::Directory { .. } => None,
        }
    }
    pub fn path(&self) -> Option<&PathBuf> {
        match self {
            Entry::Document { path, .. } => path.as_ref(),
            Entry::Directory { path } => Some(path),
        }
    }
    pub fn directory(&self) -> Option<&PathBuf> {
        match self {
            Entry::Directory { path } => Some(path),
            _ => None,
        }
    }
}

impl<'de> Deserialize<'de> for Entry {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(d)?;
        let o = value.as_object().ok_or_else(|| serde::de::Error::custom("invalid Config.Entry"))?;
        match o.get("type").and_then(Value::as_str) {
            Some("document") => {
                let path = match o.get("path") {
                    None => None,
                    Some(Value::String(s)) => Some(PathBuf::from(s)),
                    Some(_) => return Err(serde::de::Error::custom("invalid Config.Document path")),
                };
                let info = o
                    .get("info")
                    .and_then(Info::decode)
                    .ok_or_else(|| serde::de::Error::custom("invalid Config.Info"))?;
                let integration = match (o.get("integrationID").and_then(Value::as_str), o.get("connection")) {
                    (Some(id), Some(connection)) => Some(IntegrationSource {
                        integration_id: id.to_string(),
                        connection: connection.clone(),
                        organization: o.get("organization").and_then(Value::as_str).map(String::from),
                    }),
                    _ => None,
                };
                Ok(Entry::Document { path, info, integration })
            }
            Some("directory") => {
                let path = o
                    .get("path")
                    .and_then(Value::as_str)
                    .ok_or_else(|| serde::de::Error::custom("invalid Config.Directory path"))?;
                Ok(Entry::Directory { path: PathBuf::from(path) })
            }
            _ => Err(serde::de::Error::custom("invalid Config.Entry")),
        }
    }
}

/// `Config.Event.Updated` type string.
pub const EVENT_UPDATED: &str = "config.updated";
