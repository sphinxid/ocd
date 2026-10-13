//! AWS credential resolution and Bedrock auth (`protocols/utils/bedrock-auth.ts`).
//!
//! The default chain is a subset of the AWS SDK node provider chain, re-resolved on every request:
//! environment variables, shared `credentials`/`config` files (static keys and `credential_process`)
//! for `AWS_PROFILE`/explicit profile, and the ECS container credentials endpoint. SSO, web identity
//! and EC2 instance metadata are not implemented.

use super::sigv4::{self, AwsCredentials};
use crate::error::AIError;
use crate::route::auth::{self, AuthFailure, AuthInput};
use crate::route::{Auth, Credential, Headers};
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// `resolveRegion`.
pub fn resolve_region(region: Option<&str>, credentials_region: Option<&str>) -> String {
    region
        .or(credentials_region)
        .map(String::from)
        .or_else(|| std::env::var("AWS_REGION").ok())
        .or_else(|| std::env::var("AWS_DEFAULT_REGION").ok())
        .unwrap_or_else(|| "us-east-1".into())
}

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_default()
}

/// Parse an INI file into `section → key → value`.
pub fn parse_ini(text: &str) -> HashMap<String, HashMap<String, String>> {
    let mut out: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut section = String::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].trim().to_string();
            out.entry(section.clone()).or_default();
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            out.entry(section.clone()).or_default().insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    out
}

fn config_file() -> PathBuf {
    std::env::var("AWS_CONFIG_FILE").map(PathBuf::from).unwrap_or_else(|_| home().join(".aws").join("config"))
}

fn credentials_file() -> PathBuf {
    std::env::var("AWS_SHARED_CREDENTIALS_FILE").map(PathBuf::from).unwrap_or_else(|_| home().join(".aws").join("credentials"))
}

/// Profiles defined in the shared config and credentials files.
pub fn list_profiles() -> Vec<String> {
    let mut out: Vec<String> = vec![];
    if let Ok(text) = std::fs::read_to_string(config_file()) {
        for name in parse_ini(&text).keys() {
            let n = name.strip_prefix("profile ").unwrap_or(name).trim().to_string();
            if !out.contains(&n) {
                out.push(n);
            }
        }
    }
    if let Ok(text) = std::fs::read_to_string(credentials_file()) {
        for name in parse_ini(&text).keys() {
            if !out.contains(name) {
                out.push(name.clone());
            }
        }
    }
    out.sort();
    out
}

fn chain_error(message: impl std::fmt::Display) -> AuthFailure {
    AuthFailure::Error(AIError::authentication(format!("AWS default credential chain failed: {message}")))
}

fn from_env(region: &str) -> Option<AwsCredentials> {
    let id = std::env::var("AWS_ACCESS_KEY_ID").ok().filter(|v| !v.is_empty())?;
    let secret = std::env::var("AWS_SECRET_ACCESS_KEY").ok().filter(|v| !v.is_empty())?;
    Some(AwsCredentials {
        region: region.into(),
        access_key_id: id,
        secret_access_key: secret,
        session_token: std::env::var("AWS_SESSION_TOKEN").ok().filter(|v| !v.is_empty()),
    })
}

async fn from_process(command: &str, region: &str) -> Result<AwsCredentials, AuthFailure> {
    let output = tokio::process::Command::new(if cfg!(windows) { "cmd" } else { "sh" })
        .arg(if cfg!(windows) { "/C" } else { "-c" })
        .arg(command)
        .output()
        .await
        .map_err(chain_error)?;
    if !output.status.success() {
        return Err(chain_error(format!("credential_process exited with {}", output.status)));
    }
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).map_err(chain_error)?;
    let s = |k: &str| value.get(k).and_then(|v| v.as_str()).map(String::from);
    Ok(AwsCredentials {
        region: region.into(),
        access_key_id: s("AccessKeyId").ok_or_else(|| chain_error("credential_process returned no AccessKeyId"))?,
        secret_access_key: s("SecretAccessKey").ok_or_else(|| chain_error("credential_process returned no SecretAccessKey"))?,
        session_token: s("SessionToken"),
    })
}

async fn from_profile(profile: &str, region: &str) -> Result<Option<AwsCredentials>, AuthFailure> {
    let creds = std::fs::read_to_string(credentials_file()).map(|t| parse_ini(&t)).unwrap_or_default();
    let config = std::fs::read_to_string(config_file()).map(|t| parse_ini(&t)).unwrap_or_default();
    let config_section = if profile == "default" {
        config.get("default").or_else(|| config.get("profile default"))
    } else {
        config.get(&format!("profile {profile}"))
    };
    for section in [creds.get(profile), config_section].into_iter().flatten() {
        if let (Some(id), Some(secret)) = (section.get("aws_access_key_id"), section.get("aws_secret_access_key")) {
            return Ok(Some(AwsCredentials {
                region: region.into(),
                access_key_id: id.clone(),
                secret_access_key: secret.clone(),
                session_token: section.get("aws_session_token").cloned(),
            }));
        }
        if let Some(command) = section.get("credential_process") {
            return from_process(command, region).await.map(Some);
        }
    }
    Ok(None)
}

async fn from_container(region: &str) -> Result<Option<AwsCredentials>, AuthFailure> {
    let url = if let Ok(rel) = std::env::var("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI") {
        format!("http://169.254.170.2{rel}")
    } else if let Ok(full) = std::env::var("AWS_CONTAINER_CREDENTIALS_FULL_URI") {
        full
    } else {
        return Ok(None);
    };
    let client = oc_util::http::client();
    let mut req = client.get(&url).timeout(std::time::Duration::from_secs(5));
    if let Ok(token) = std::env::var("AWS_CONTAINER_AUTHORIZATION_TOKEN") {
        req = req.header("authorization", token);
    }
    let value: serde_json::Value = req.send().await.map_err(chain_error)?.json().await.map_err(chain_error)?;
    let s = |k: &str| value.get(k).and_then(|v| v.as_str()).map(String::from);
    Ok(Some(AwsCredentials {
        region: region.into(),
        access_key_id: s("AccessKeyId").ok_or_else(|| chain_error("container credentials missing AccessKeyId"))?,
        secret_access_key: s("SecretAccessKey").ok_or_else(|| chain_error("container credentials missing SecretAccessKey"))?,
        session_token: s("Token"),
    }))
}

/// Resolve credentials through the (subset) default chain.
pub async fn default_chain(region: &str, profile: Option<&str>) -> Result<AwsCredentials, AuthFailure> {
    if profile.is_none() && std::env::var("AWS_PROFILE").map(|p| p.is_empty()).unwrap_or(true) {
        if let Some(c) = from_env(region) {
            return Ok(c);
        }
    }
    let profile = profile.map(String::from).or_else(|| std::env::var("AWS_PROFILE").ok()).unwrap_or_else(|| "default".into());
    if let Some(c) = from_profile(&profile, region).await? {
        return Ok(c);
    }
    if let Some(c) = from_container(region).await? {
        return Ok(c);
    }
    Err(chain_error("Could not load credentials from any providers"))
}

/// Credential source for SigV4.
#[derive(Clone, Debug)]
pub enum CredentialSource {
    Static(AwsCredentials),
    DefaultChain { region: String, profile: Option<String> },
}

struct SigV4Auth {
    source: Option<CredentialSource>,
    service: String,
    name: String,
}

#[async_trait]
impl Auth for SigV4Auth {
    async fn apply(&self, input: AuthInput<'_>) -> Result<Headers, AuthFailure> {
        let Some(source) = &self.source else {
            return Err(AuthFailure::Error(AIError::invalid_request(format!(
                "{} requires either route bearer auth or AWS credentials configured on the route",
                self.name
            ))));
        };
        let credentials = match source {
            CredentialSource::Static(c) => c.clone(),
            CredentialSource::DefaultChain { region, profile } => default_chain(region, profile.as_deref()).await?,
        };
        let mut headers = input.headers;
        headers.insert("content-type".into(), "application/json".into());
        let signed = sigv4::sign(input.method, input.url, &headers, input.body, &credentials, &self.service, chrono::Utc::now())
            .map_err(|e| AuthFailure::Error(AIError::invalid_request(format!("{} SigV4 signing failed: {e}", self.name))))?;
        for (k, v) in signed {
            headers.insert(k, v);
        }
        Ok(headers)
    }
}

/// `sigV4(source, {service, name})`.
pub fn sigv4_auth(source: Option<CredentialSource>, service: &str, name: &str) -> Arc<dyn Auth> {
    Arc::new(SigV4Auth { source, service: service.into(), name: name.into() })
}

/// Auth inputs for `resolveAuth`.
#[derive(Clone, Debug, Default)]
pub struct BedrockAuthOptions {
    pub api_key: Option<String>,
    pub credentials: Option<AwsCredentials>,
    pub profile: Option<String>,
    /// `"bearer" | "sigv4"`.
    pub mode: Option<String>,
    pub service: Option<String>,
    pub name: Option<String>,
}

/// `resolveAuth`: bearer wins, then static credentials, then the default chain.
pub fn bedrock_auth(options: BedrockAuthOptions, region: &str) -> Arc<dyn Auth> {
    let service = options.service.clone().unwrap_or_else(|| "bedrock".into());
    let name = options.name.clone().unwrap_or_else(|| "Bedrock Converse".into());
    let api_key = if options.mode.as_deref() == Some("sigv4") {
        None
    } else {
        options.api_key.clone().or_else(|| std::env::var("AWS_BEARER_TOKEN_BEDROCK").ok())
    };
    if let Some(key) = api_key {
        return auth::bearer(Credential::value(key));
    }
    if let Some(mut creds) = options.credentials {
        creds.region = region.into();
        return sigv4_auth(Some(CredentialSource::Static(creds)), &service, &name);
    }
    sigv4_auth(Some(CredentialSource::DefaultChain { region: region.into(), profile: options.profile }), &service, &name)
}

pub fn bedrock_base_url(region: &str) -> String {
    format!("https://bedrock-runtime.{region}.amazonaws.com")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ini_parsing() {
        let ini = parse_ini("[default]\naws_access_key_id = A\n[profile dev]\nregion=us-west-2\n");
        assert_eq!(ini["default"]["aws_access_key_id"], "A");
        assert_eq!(ini["profile dev"]["region"], "us-west-2");
    }

    #[tokio::test]
    async fn static_sigv4_signs() {
        let auth = bedrock_auth(
            BedrockAuthOptions {
                credentials: Some(AwsCredentials { region: "x".into(), access_key_id: "AK".into(), secret_access_key: "SK".into(), session_token: Some("T".into()) }),
                mode: Some("sigv4".into()),
                ..Default::default()
            },
            "us-east-1",
        );
        let headers = auth::apply(
            auth.as_ref(),
            AuthInput { method: "POST", url: "https://bedrock-runtime.us-east-1.amazonaws.com/model/m/converse-stream", body: "{}", headers: Headers::new() },
        )
        .await
        .unwrap();
        assert!(headers["authorization"].starts_with("AWS4-HMAC-SHA256 Credential=AK/"));
        assert!(headers["authorization"].contains("/us-east-1/bedrock/aws4_request, SignedHeaders=host;x-amz-date;x-amz-security-token,"));
        assert_eq!(headers["x-amz-security-token"], "T");
        assert_eq!(headers["content-type"], "application/json");
    }
}
