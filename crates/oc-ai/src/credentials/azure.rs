//! Azure CLI (Microsoft Entra ID) token helper (`packages/core/src/plugin/provider/azure.ts`).
//!
//! The `azure` integration's external method `azure-cli` stores no secret; bearer tokens are minted per
//! request with `az account get-access-token --scope <scope> --output json` and cached per scope until 5 min
//! before expiry.

use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

pub const COGNITIVE_SCOPE: &str = "https://cognitiveservices.azure.com/.default";
pub const FOUNDRY_SCOPE: &str = "https://ai.azure.com/.default";
pub const MANAGEMENT_SCOPE: &str = "https://management.azure.com/.default";
pub const METHOD_ID: &str = "azure-cli";
pub const METHOD_LABEL: &str = "Microsoft Entra ID (Azure CLI)";
pub const ENV_NAMES: &[&str] = &["AZURE_API_KEY", "AZURE_COGNITIVE_SERVICES_API_KEY"];
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const REFRESH_MARGIN_MS: u64 = 5 * 60_000;

/// Runs the Azure CLI; returns stdout or the `AppProcessError` message.
#[async_trait]
pub trait AzCli: Send + Sync {
    async fn run(&self, args: &[&str]) -> Result<String, String>;
}

/// Spawns `az` (env inherited, stdin ignored, 10 s timeout).
pub struct SystemAz;

#[async_trait]
impl AzCli for SystemAz {
    async fn run(&self, args: &[&str]) -> Result<String, String> {
        let command = format!("az {}", args.join(" "));
        let child = tokio::process::Command::new("az")
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .output();
        let output = match tokio::time::timeout(COMMAND_TIMEOUT, child).await {
            Ok(Ok(output)) => output,
            Ok(Err(error)) => return Err(format!("Command failed: {command}: {error}")),
            Err(_) => return Err(format!("Command failed: {command}: Command timed out")),
        };
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let status = output.status.code().map(|c| format!(" (exit {c})")).unwrap_or_default();
            return Err(format!(
                "Command failed{status}: {command}{}",
                if stderr.is_empty() { String::new() } else { format!(": {stderr}") }
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }
}

/// A minted token (epoch-ms expiry).
#[derive(Debug, Clone, PartialEq)]
pub struct AzureToken {
    pub access: String,
    pub expires: u64,
}

/// Per-scope token cache over the Azure CLI.
pub struct AzureCliTokens {
    cli: Arc<dyn AzCli>,
    clock: crate::credentials::Clock,
    tokens: Mutex<HashMap<String, AzureToken>>,
}

impl AzureCliTokens {
    pub fn new(cli: Arc<dyn AzCli>) -> Self {
        Self::with_clock(cli, crate::credentials::system_clock())
    }

    pub fn with_clock(cli: Arc<dyn AzCli>, clock: crate::credentials::Clock) -> Self {
        Self { cli, clock, tokens: Mutex::new(HashMap::new()) }
    }

    /// Cached token unless it expires within 5 minutes.
    pub async fn token(&self, scope: &str) -> Result<AzureToken, String> {
        let now = (self.clock)();
        if let Some(cached) = self.tokens.lock().get(scope).cloned()
            && cached.expires.saturating_sub(now) > REFRESH_MARGIN_MS
        {
            return Ok(cached);
        }
        let stdout = self.cli.run(&["account", "get-access-token", "--scope", scope, "--output", "json"]).await?;
        let token = parse_token(&stdout)?;
        self.tokens.lock().insert(scope.to_string(), token.clone());
        Ok(token)
    }

    /// `Bearer <token>` for a request URL.
    pub async fn bearer(&self, url: &str) -> Result<String, String> {
        let token = self.token(scope_for_url(url)).await?;
        Ok(format!("Bearer {}", token.access))
    }
}

/// Foundry hosts (`*.services.ai.azure.com`, path not under `/models`) use the AI scope; everything else the
/// Cognitive Services scope.
pub fn scope_for_url(url: &str) -> &'static str {
    match url::Url::parse(url) {
        Ok(target)
            if target.host_str().is_some_and(|h| h.ends_with(".services.ai.azure.com"))
                && !target.path().starts_with("/models") =>
        {
            FOUNDRY_SCOPE
        }
        _ => COGNITIVE_SCOPE,
    }
}

/// Decodes `{accessToken, expires_on?: seconds, expiresOn?: date}`.
pub fn parse_token(stdout: &str) -> Result<AzureToken, String> {
    let value: Value = serde_json::from_str(stdout).map_err(|e| e.to_string())?;
    let access = value
        .get("accessToken")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("Expected a non empty string at [\"accessToken\"]")?
        .to_string();
    let expires = match value.get("expires_on") {
        Some(Value::Number(n)) => n.as_f64().map(|s| s * 1000.0),
        Some(_) => return Err("Expected number at [\"expires_on\"]".into()),
        None => value.get("expiresOn").and_then(Value::as_str).and_then(parse_date),
    };
    match expires {
        Some(ms) if ms.is_finite() => Ok(AzureToken { access, expires: ms.max(0.0) as u64 }),
        _ => Err("Azure CLI returned an invalid token expiration".into()),
    }
}

/// `Date.parse` for the formats the Azure CLI emits (RFC 3339, or local `YYYY-MM-DD HH:MM:SS[.ffffff]`).
fn parse_date(text: &str) -> Option<f64> {
    if let Ok(date) = chrono::DateTime::parse_from_rfc3339(text) {
        return Some(date.timestamp_millis() as f64);
    }
    for format in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f"] {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(text, format) {
            use chrono::TimeZone;
            return chrono::Local.from_local_datetime(&naive).earliest().map(|d| d.timestamp_millis() as f64);
        }
    }
    None
}
