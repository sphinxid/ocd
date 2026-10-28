//! `fromNodeProviderChain` / `defaultProvider` (`@aws-sdk/credential-provider-node`) and the
//! providers it runs: `fromEnv`, `fromSSO` (skipped without SSO inputs), `fromIni` (static keys,
//! assume-role with `source_profile` / `credential_source`, web identity, `credential_process`,
//! SSO, `login_session`), `fromProcess`, `fromTokenFile`, then the container or instance metadata
//! endpoints. A provider error with `tryNextLink` moves on to the next provider; any other error
//! ends the chain and is what the caller sees.

use super::ini::{self, Section, Sections};
use super::{imds, login, sso, sts};
use crate::protocols::cloud_env::{Env, State};
use futures::future::BoxFuture;
use parking_lot::Mutex;

/// A provider failure: `CredentialsProviderError` (`try_next` = `tryNextLink`) or any other thrown
/// error (`try_next` false).
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderError {
    pub message: String,
    pub try_next: bool,
}

impl ProviderError {
    /// `CredentialsProviderError` with the default `tryNextLink: true`.
    pub fn next(message: impl Into<String>) -> Self {
        Self { message: message.into(), try_next: true }
    }
    /// `tryNextLink: false`, or a plain `Error`.
    pub fn stop(message: impl Into<String>) -> Self {
        Self { message: message.into(), try_next: false }
    }
}

/// `AwsCredentialIdentity`.
#[derive(Debug, Clone, PartialEq)]
pub struct Identity {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
    /// Milliseconds since the epoch.
    pub expiration: Option<f64>,
}

/// `defaultProvider({ ignoreCache: true, profile })()`.
pub async fn default_provider(env: &Env, profile: Option<&str>) -> Result<Identity, ProviderError> {
    let init = Init { env, profile: profile.map(String::from), role_region: Mutex::new(None) };
    let steps: [Step; 7] = [Step::Env, Step::Sso, Step::Ini, Step::Process, Step::TokenFile, Step::Remote, Step::Fail];
    let mut last = ProviderError::next("No providers in chain");
    for step in steps {
        let result = match step {
            Step::Env => env_step(&init),
            Step::Sso => Err(ProviderError::next("Skipping SSO provider in default chain (inputs do not include SSO fields).")),
            Step::Ini => from_ini(&init).await,
            Step::Process => from_process(&init).await,
            Step::TokenFile => from_token_file(env).await,
            Step::Remote => imds::remote_provider(env, init.profile.as_deref()).await,
            Step::Fail => Err(ProviderError::stop("Could not load credentials from any providers")),
        };
        match result {
            Ok(identity) => return Ok(identity),
            Err(e) if e.try_next => last = e,
            Err(e) => return Err(e),
        }
    }
    Err(last)
}

enum Step {
    Env,
    Sso,
    Ini,
    Process,
    TokenFile,
    Remote,
    Fail,
}

/// The `init` object the chain shares; `role_region` is the region the first assume-role step
/// gives the (memoized) STS role assumer.
pub struct Init<'a> {
    pub env: &'a Env,
    pub profile: Option<String>,
    role_region: Mutex<Option<Option<String>>>,
}

fn env_step(init: &Init<'_>) -> Result<Identity, ProviderError> {
    let env = init.env;
    let profile = init.profile.clone().or_else(|| env.var("AWS_PROFILE"));
    if profile.is_some_and(|p| !p.is_empty()) {
        if env.non_empty("AWS_ACCESS_KEY_ID").is_some()
            && env.non_empty("AWS_SECRET_ACCESS_KEY").is_some()
            && !State::warn_once(&env.state().multiple_sources_warned)
        {
            tracing::warn!(
                "@aws-sdk/credential-provider-node - defaultProvider::fromEnv WARNING:\n    Multiple credential sources detected: \n    Both AWS_PROFILE and the pair AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY static credentials are set.\n    This SDK will proceed with the AWS_PROFILE value.\n    \n    However, a future version may change this behavior to prefer the ENV static credentials.\n    Please ensure that your environment only sets either the AWS_PROFILE or the\n    AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY pair.\n"
            );
        }
        return Err(ProviderError::next("AWS_PROFILE is set, skipping fromEnv provider."));
    }
    from_env(env)
}

/// `fromEnv` of `@aws-sdk/credential-provider-env`.
pub fn from_env(env: &Env) -> Result<Identity, ProviderError> {
    match (env.non_empty("AWS_ACCESS_KEY_ID"), env.non_empty("AWS_SECRET_ACCESS_KEY")) {
        (Some(access_key_id), Some(secret_access_key)) => Ok(Identity {
            access_key_id,
            secret_access_key,
            session_token: env.non_empty("AWS_SESSION_TOKEN"),
            expiration: env.non_empty("AWS_CREDENTIAL_EXPIRATION").and_then(|e| crate::js_date::parse(&e)),
        }),
        _ => Err(ProviderError::next("Unable to find environment variable credentials.")),
    }
}

/// `fromIni`.
async fn from_ini(init: &Init<'_>) -> Result<Identity, ProviderError> {
    let profiles = ini::known_profiles(init.env);
    let name = ini::profile_name(init.env, init.profile.as_deref());
    resolve_profile_data(init, name, &profiles, Vec::new(), false).await
}

fn has(section: Option<&Section>, key: &str) -> bool {
    section.is_some_and(|s| s.contains_key(key))
}

fn is_static(data: Option<&Section>) -> bool {
    has(data, "aws_access_key_id") && has(data, "aws_secret_access_key")
}

fn is_assume_role(data: Option<&Section>) -> bool {
    has(data, "role_arn")
        && ((has(data, "source_profile") && !has(data, "credential_source")) || (has(data, "credential_source") && !has(data, "source_profile")))
}

/// `isCredentialSourceWithoutRoleArn`.
fn credential_source_without_role(data: Option<&Section>) -> bool {
    data.and_then(|d| d.get("role_arn")).is_none_or(|v| v.is_empty()) && data.and_then(|d| d.get("credential_source")).is_some_and(|v| !v.is_empty())
}

pub fn is_sso(data: Option<&Section>) -> bool {
    ["sso_start_url", "sso_account_id", "sso_session", "sso_region", "sso_role_name"].iter().any(|k| has(data, k))
}

fn static_identity(data: &Section) -> Identity {
    Identity {
        access_key_id: data["aws_access_key_id"].clone(),
        secret_access_key: data["aws_secret_access_key"].clone(),
        session_token: data.get("aws_session_token").cloned(),
        expiration: None,
    }
}

/// `resolveProfileData`.
fn resolve_profile_data<'a>(
    init: &'a Init<'a>,
    name: String,
    profiles: &'a Sections,
    visited: Vec<String>,
    assume_role_recursive: bool,
) -> BoxFuture<'a, Result<Identity, ProviderError>> {
    Box::pin(async move {
        let data = profiles.get(&name);
        if !visited.is_empty() && is_static(data) {
            return Ok(static_identity(data.unwrap()));
        }
        if assume_role_recursive || is_assume_role(data) {
            return resolve_assume_role(init, name, profiles, visited).await;
        }
        if is_static(data) {
            return Ok(static_identity(data.unwrap()));
        }
        if has(data, "web_identity_token_file") && has(data, "role_arn") {
            let data = data.unwrap();
            let session_name = data.get("role_session_name").cloned().or_else(|| init.env.var("AWS_ROLE_SESSION_NAME"));
            return web_identity(init.env, &data["web_identity_token_file"], &data["role_arn"], session_name).await;
        }
        if has(data, "credential_process") {
            return process_credentials(init.env, &name, profiles).await;
        }
        if is_sso(data) {
            return sso::from_sso_profile(init.env, &name).await;
        }
        if data.and_then(|d| d.get("login_session")).is_some_and(|v| !v.is_empty()) {
            return login::from_login_profile(init.env, &name).await;
        }
        Err(ProviderError::next(format!("Could not resolve credentials using profile: [{name}] in configuration/credentials file(s).")))
    })
}

/// `resolveAssumeRoleCredentials`.
async fn resolve_assume_role(init: &Init<'_>, name: String, profiles: &Sections, visited: Vec<String>) -> Result<Identity, ProviderError> {
    let empty = Section::new();
    let data = profiles.get(&name).unwrap_or(&empty);
    let source_profile = data.get("source_profile").filter(|v| !v.is_empty()).cloned();
    {
        let mut region = init.role_region.lock();
        if region.is_none() {
            *region = Some(data.get("region").cloned());
        }
    }
    if let Some(source) = &source_profile
        && visited.contains(source)
    {
        return Err(ProviderError::next(format!(
            "Detected a cycle attempting to resolve credentials for profile {}. Profiles visited: {}",
            ini::profile_name(init.env, init.profile.as_deref()),
            visited.join(", ")
        )));
    }
    // `resolveCredentialSource` throws for an unknown source before the MFA check runs.
    let source_name = data.get("credential_source").map(String::as_str).unwrap_or("");
    if source_profile.is_none() && !["EcsContainer", "Ec2InstanceMetadata", "Environment"].contains(&source_name) {
        return Err(ProviderError::next(format!(
            "Unsupported credential source in profile {name}. Got {}, expected EcsContainer or Ec2InstanceMetadata or Environment.",
            data.get("credential_source").map(String::as_str).unwrap_or("undefined")
        )));
    }
    let without_role = credential_source_without_role(Some(data));
    let mut params = None;
    if !without_role {
        let role_session_name =
            data.get("role_session_name").filter(|v| !v.is_empty()).cloned().unwrap_or_else(|| format!("aws-sdk-js-{}", chrono::Utc::now().timestamp_millis()));
        let duration = ini::parse_int(data.get("duration_seconds").filter(|v| !v.is_empty()).map(String::as_str).unwrap_or("3600"));
        if data.get("mfa_serial").is_some_and(|v| !v.is_empty()) {
            return Err(ProviderError::stop(format!("Profile {name} requires multi-factor authentication, but no MFA code callback was provided.")));
        }
        params = Some(sts::AssumeRoleParams {
            role_arn: data.get("role_arn").cloned().unwrap_or_default(),
            role_session_name,
            external_id: data.get("external_id").cloned(),
            duration_seconds: duration,
        });
    }
    let source = match &source_profile {
        Some(source) => {
            let mut next_visited = visited.clone();
            next_visited.push(source.clone());
            let recursive = credential_source_without_role(profiles.get(source));
            resolve_profile_data(init, source.clone(), profiles, next_visited, recursive).await?
        }
        None => credential_source(init, source_name).await?,
    };
    let Some(params) = params else { return Ok(source) };
    let region = init.role_region.lock().clone().flatten();
    let region = sts::region(init.env, region.as_deref());
    sts::assume_role(init.env, &region, &source, &params).await
}

/// `resolveCredentialSource`.
async fn credential_source(init: &Init<'_>, source: &str) -> Result<Identity, ProviderError> {
    match source {
        "EcsContainer" => imds::container_chain(init.env).await,
        "Ec2InstanceMetadata" => imds::from_instance_metadata(init.env, init.profile.as_deref()).await,
        _ => from_env(init.env),
    }
}

/// `fromProcess`.
async fn from_process(init: &Init<'_>) -> Result<Identity, ProviderError> {
    let profiles = ini::known_profiles(init.env);
    let name = ini::profile_name(init.env, init.profile.as_deref());
    process_credentials(init.env, &name, &profiles).await
}

/// `resolveProcessCredentials`.
async fn process_credentials(env: &Env, name: &str, profiles: &Sections) -> Result<Identity, ProviderError> {
    let Some(profile) = profiles.get(name) else {
        return Err(ProviderError::next(format!("Profile {name} could not be found in shared credentials file.")));
    };
    let Some(command) = profile.get("credential_process") else {
        return Err(ProviderError::next(format!("Profile {name} did not contain credential_process.")));
    };
    run_process(env, name, command).await.map_err(ProviderError::next)
}

async fn run_process(env: &Env, name: &str, command: &str) -> Result<Identity, String> {
    let mut cmd = if cfg!(windows) {
        let mut c = tokio::process::Command::new("cmd.exe");
        c.args(["/d", "/s", "/c", command]);
        c
    } else {
        let mut c = tokio::process::Command::new("/bin/sh");
        c.args(["-c", command]);
        c
    };
    env.apply(&mut cmd);
    cmd.stdin(std::process::Stdio::null());
    let output = cmd.output().await.map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format!("Command failed: {command}\n{}", String::from_utf8_lossy(&output.stderr)));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let data: serde_json::Value = serde_json::from_str(stdout.trim()).map_err(|_| format!("Profile {name} credential_process returned invalid JSON."))?;
    if data.get("Version").and_then(|v| v.as_f64()) != Some(1.0) {
        return Err(format!("Profile {name} credential_process did not return Version 1."));
    }
    let (Some(access_key_id), Some(secret_access_key)) = (data.get("AccessKeyId"), data.get("SecretAccessKey")) else {
        return Err(format!("Profile {name} credential_process returned invalid credentials."));
    };
    let expiration = match data.get("Expiration") {
        Some(v) if js_truthy(v) => {
            let at = v.as_str().and_then(crate::js_date::parse);
            if at.is_some_and(|at| at < chrono::Utc::now().timestamp_millis() as f64) {
                return Err(format!("Profile {name} credential_process returned expired credentials."));
            }
            at
        }
        _ => None,
    };
    Ok(Identity {
        access_key_id: js_string(access_key_id),
        secret_access_key: js_string(secret_access_key),
        session_token: data.get("SessionToken").filter(|v| js_truthy(v)).map(js_string),
        expiration,
    })
}

fn js_truthy(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => false,
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        serde_json::Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

fn js_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// `fromTokenFile` with the environment variables.
async fn from_token_file(env: &Env) -> Result<Identity, ProviderError> {
    match (env.non_empty("AWS_WEB_IDENTITY_TOKEN_FILE"), env.non_empty("AWS_ROLE_ARN")) {
        (Some(file), Some(role)) => web_identity(env, &file, &role, env.var("AWS_ROLE_SESSION_NAME")).await,
        _ => Err(ProviderError::next("Web identity configuration not specified")),
    }
}

/// `fromWebToken` after reading the token file (`readFileSync(file, "ascii")`, not trimmed).
async fn web_identity(env: &Env, file: &str, role_arn: &str, session_name: Option<String>) -> Result<Identity, ProviderError> {
    let bytes = std::fs::read(file).map_err(|e| ProviderError::stop(fs_error(&e, "open", file)))?;
    let token: String = bytes.iter().map(|b| (b & 0x7f) as char).collect();
    let session_name = session_name.unwrap_or_else(|| format!("aws-sdk-js-session-{}", chrono::Utc::now().timestamp_millis()));
    sts::assume_role_with_web_identity(env, role_arn, &session_name, &token).await
}

/// A Node `fs` error message (`ENOENT: no such file or directory, open '/x'`).
pub fn fs_error(error: &std::io::Error, syscall: &str, path: &str) -> String {
    let (code, text) = match error.kind() {
        std::io::ErrorKind::NotFound => ("ENOENT", "no such file or directory"),
        std::io::ErrorKind::PermissionDenied => ("EACCES", "permission denied"),
        std::io::ErrorKind::IsADirectory => ("EISDIR", "illegal operation on a directory"),
        _ => return error.to_string(),
    };
    format!("{code}: {text}, {syscall} '{path}'")
}
