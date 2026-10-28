//! The remote providers of the default chain: container credentials (`fromHttp` of
//! `@aws-sdk/credential-provider-http`, then `fromContainerMetadata`) and EC2 instance metadata
//! (`fromInstanceMetadata` of `@smithy/credential-provider-imds`: IMDSv2 token with v1 fallback,
//! 1 s timeouts, no retries, expired credentials extended), plus the IMDS region lookup the SDK's
//! region loader falls back to.

use super::chain::{ProviderError, fs_error};
use super::ini::{self, Preferred};
use super::{chain::Identity, service};
use crate::protocols::cloud_env::Env;
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_millis(1000);

/// `remoteProvider`.
pub async fn remote_provider(env: &Env, profile: Option<&str>) -> Result<Identity, ProviderError> {
    if env.non_empty("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI").is_some() || env.non_empty("AWS_CONTAINER_CREDENTIALS_FULL_URI").is_some() {
        return container_chain(env).await;
    }
    if env.non_empty("AWS_EC2_METADATA_DISABLED").is_some_and(|v| v != "false") {
        return Err(ProviderError::next("EC2 Instance Metadata Service access disabled"));
    }
    from_instance_metadata(env, profile).await
}

/// `chain(fromHttp(init), fromContainerMetadata(init))`.
pub async fn container_chain(env: &Env) -> Result<Identity, ProviderError> {
    let http = match from_http_setup(env) {
        Ok(setup) => from_http(env, setup).await,
        Err(e) => return Err(e),
    };
    match http {
        Ok(identity) => Ok(identity),
        Err(e) if !e.try_next => Err(e),
        Err(_) => from_container_metadata(env).await,
    }
}

struct HttpSetup {
    url: url::Url,
    token: Option<String>,
    token_file: Option<String>,
}

/// The synchronous part of `fromHttp` (URL choice and `checkUrl`).
fn from_http_setup(env: &Env) -> Result<HttpSetup, ProviderError> {
    let relative = env.non_empty("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI");
    let full = env.non_empty("AWS_CONTAINER_CREDENTIALS_FULL_URI");
    let token = env.non_empty("AWS_CONTAINER_AUTHORIZATION_TOKEN");
    let token_file = env.non_empty("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE");
    if relative.is_some() && full.is_some() {
        tracing::warn!("@aws-sdk/credential-provider-http: you have set both awsContainerCredentialsRelativeUri and awsContainerCredentialsFullUri.");
        tracing::warn!("awsContainerCredentialsRelativeUri will take precedence.");
    }
    if token.is_some() && token_file.is_some() {
        tracing::warn!("@aws-sdk/credential-provider-http: you have set both awsContainerAuthorizationToken and awsContainerAuthorizationTokenFile.");
        tracing::warn!("awsContainerAuthorizationTokenFile will take precedence.");
    }
    let host = match (relative, full) {
        (Some(relative), _) => format!("http://169.254.170.2{relative}"),
        (None, Some(full)) => full,
        _ => {
            return Err(ProviderError::next(
                "No HTTP credential provider host provided.\nSet AWS_CONTAINER_CREDENTIALS_FULL_URI or AWS_CONTAINER_CREDENTIALS_RELATIVE_URI.",
            ));
        }
    };
    let url = url::Url::parse(&host).map_err(|_| ProviderError::stop(format!("\"{host}\" cannot be parsed as a URL.")))?;
    check_url(&url)?;
    Ok(HttpSetup { url, token, token_file })
}

/// `checkUrl`: HTTPS, loopback, or the ECS / EKS container hosts.
fn check_url(url: &url::Url) -> Result<(), ProviderError> {
    if url.scheme() == "https" {
        return Ok(());
    }
    let host = match url.host() {
        Some(url::Host::Ipv6(ip)) => format!("[{ip}]"),
        Some(host) => host.to_string(),
        None => String::new(),
    };
    if ["169.254.170.2", "169.254.170.23", "[fd00:ec2::23]", "[::1]", "localhost"].contains(&host.as_str()) {
        return Ok(());
    }
    if !host.contains('[') {
        let parts: Vec<&str> = host.split('.').collect();
        let in_range = |c: &&str| ini::parse_int(c).is_some_and(|n| (0..=255).contains(&n));
        if parts.len() == 4 && parts[0] == "127" && parts[1..].iter().all(in_range) {
            return Ok(());
        }
    }
    Err(ProviderError::next(
        "URL not accepted. It must either be HTTPS or match one of the following:\n  - loopback CIDR 127.0.0.0/8 or [::1/128]\n  - ECS container host 169.254.170.2\n  - EKS container host 169.254.170.23 or [fd00:ec2::23]",
    ))
}

/// `fromHttp`: up to 4 attempts, 1 s apart.
async fn from_http(env: &Env, setup: HttpSetup) -> Result<Identity, ProviderError> {
    let _ = env;
    let mut attempt = 0;
    loop {
        attempt += 1;
        let result = http_attempt(&setup).await;
        if result.is_ok() || attempt >= 4 {
            return result;
        }
        tokio::time::sleep(TIMEOUT).await;
    }
}

async fn http_attempt(setup: &HttpSetup) -> Result<Identity, ProviderError> {
    let client = oc_util::http::client_builder().no_proxy().connect_timeout(TIMEOUT).build().map_err(|e| ProviderError::next(e.to_string()))?;
    let mut request = client.get(setup.url.clone()).timeout(TIMEOUT);
    let token = match &setup.token_file {
        Some(file) => Some(std::fs::read_to_string(file).map_err(|e| ProviderError::stop(fs_error(&e, "open", file)))?),
        None => setup.token.clone(),
    };
    if let Some(token) = token {
        if token.contains("\r\n") {
            return Err(ProviderError::next("Authorization token contains invalid \\r\\n sequence."));
        }
        request = request.header("authorization", token);
    }
    let response = request.send().await.map_err(|e| ProviderError::next(format!("Error: {}", service::network_message(&e))))?;
    let status = response.status().as_u16();
    let body = response.text().await.map_err(|e| ProviderError::next(format!("Error: {e}")))?;
    if status == 200 {
        // `JSON.parse` failures escape the provider's try/catch.
        let parsed = parse_json(&body).map_err(ProviderError::stop)?;
        let s = |k: &str| parsed.get(k).and_then(|v| v.as_str()).map(String::from);
        return match (s("AccessKeyId"), s("SecretAccessKey"), s("Token"), s("Expiration")) {
            (Some(access_key_id), Some(secret_access_key), Some(token), Some(expiration)) => {
                Ok(Identity { access_key_id, secret_access_key, session_token: Some(token), expiration: crate::js_date::parse(&expiration) })
            }
            _ => Err(ProviderError::next(
                "HTTP credential provider response not of the required format, an object matching: { AccessKeyId: string, SecretAccessKey: string, Token: string, Expiration: string(rfc3339) }",
            )),
        };
    }
    Err(ProviderError::next(format!("Server responded with status: {status}")))
}

/// `JSON.parse(text)` with Bun's `SyntaxError` message on failure.
pub fn parse_json(text: &str) -> Result<serde_json::Value, String> {
    serde_json::from_str(text).map_err(|e| json_error(text, &e))
}

/// The common `JSON.parse` messages of JavaScriptCore.
fn json_error(text: &str, error: &serde_json::Error) -> String {
    let trimmed = text.trim_start_matches([' ', '\t', '\n', '\r']);
    let message = match trimmed.chars().next() {
        None => "Unexpected EOF".to_string(),
        Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$' => {
            let word: String = trimmed.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '$').collect();
            format!("Unexpected identifier \"{word}\"")
        }
        Some(c) if !(c == '{' || c == '[' || c == '"' || c == '-' || c.is_ascii_digit()) => format!("Unrecognized token '{c}'"),
        Some('"') if error.is_eof() => "Unterminated string".to_string(),
        Some('{') if error.is_eof() => "Expected '}'".to_string(),
        _ => "Unable to parse JSON string".to_string(),
    };
    format!("JSON Parse error: {message}")
}

/// `fromContainerMetadata` (no retries).
async fn from_container_metadata(env: &Env) -> Result<Identity, ProviderError> {
    let url = if let Some(relative) = env.non_empty("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI") {
        format!("http://169.254.170.2{relative}")
    } else if let Some(full) = env.non_empty("AWS_CONTAINER_CREDENTIALS_FULL_URI") {
        let parsed = url::Url::parse(&full).map_err(|_| ProviderError::stop(format!("{full} is not a valid container metadata service URL")))?;
        let host = parsed.host_str().unwrap_or("");
        if !["localhost", "127.0.0.1"].contains(&host) {
            return Err(ProviderError::stop(format!("{host} is not a valid container metadata service hostname")));
        }
        if !["http", "https"].contains(&parsed.scheme()) {
            return Err(ProviderError::stop(format!("{}: is not a valid container metadata service protocol", parsed.scheme())));
        }
        if parsed.scheme() == "https" {
            // Node's `http.request` refuses an `https:` URL; the request fails like a connection error.
            return Err(ProviderError::next("Unable to connect to instance metadata service"));
        }
        parsed.to_string()
    } else {
        return Err(ProviderError::stop(
            "The container metadata credential provider cannot be used unless the AWS_CONTAINER_CREDENTIALS_RELATIVE_URI or AWS_CONTAINER_CREDENTIALS_FULL_URI environment variable is set",
        ));
    };
    let mut headers = Vec::new();
    if let Some(token) = env.non_empty("AWS_CONTAINER_AUTHORIZATION_TOKEN") {
        headers.push(("authorization", token));
    }
    let body = metadata_request(reqwest::Method::GET, &url, &headers).await?;
    imds_credentials(&body)
}

/// `httpRequest`: Node `http` with a 1 s timeout; non-2xx statuses are errors.
async fn metadata_request(method: reqwest::Method, url: &str, headers: &[(&str, String)]) -> Result<String, ImdsError> {
    let client = service::http_client();
    let mut request = client.request(method, url).timeout(TIMEOUT);
    for (k, v) in headers {
        request = request.header(*k, v.as_str());
    }
    let response = request.send().await.map_err(|e| {
        if e.is_timeout() {
            ImdsError { message: "TimeoutError from instance metadata service".into(), status: None }
        } else {
            ImdsError { message: "Unable to connect to instance metadata service".into(), status: None }
        }
    })?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(ImdsError { message: "Error response received from instance metadata service".into(), status: Some(status) });
    }
    response.text().await.map_err(|_| ImdsError { message: "TimeoutError from instance metadata service".into(), status: None })
}

/// A `ProviderError` from `httpRequest` (`tryNextLink` true) with its status code.
struct ImdsError {
    message: String,
    status: Option<u16>,
}

impl From<ImdsError> for ProviderError {
    fn from(e: ImdsError) -> Self {
        ProviderError::next(e.message)
    }
}

fn imds_credentials(body: &str) -> Result<Identity, ProviderError> {
    let parsed = parse_json(body).map_err(ProviderError::stop)?;
    let s = |k: &str| parsed.get(k).and_then(|v| v.as_str()).map(String::from);
    match (s("AccessKeyId"), s("SecretAccessKey"), s("Token"), s("Expiration")) {
        (Some(access_key_id), Some(secret_access_key), Some(token), Some(expiration)) => {
            Ok(Identity { access_key_id, secret_access_key, session_token: Some(token), expiration: crate::js_date::parse(&expiration) })
        }
        _ => Err(ProviderError::next("Invalid response received from instance metadata service.")),
    }
}

/// `getInstanceMetadataEndpoint`: `AWS_EC2_METADATA_SERVICE_ENDPOINT` / `ec2_metadata_service_endpoint`,
/// else the endpoint mode (`IPv4` → 169.254.169.254, `IPv6` → [fd00:ec2::254]).
fn instance_metadata_endpoint(env: &Env) -> Result<String, ProviderError> {
    let configured = ini::load_config(
        env,
        None,
        Preferred::Config,
        |env| env.var("AWS_EC2_METADATA_SERVICE_ENDPOINT"),
        |profile, _| profile.get("ec2_metadata_service_endpoint").cloned(),
    );
    if let Some(endpoint) = configured.filter(|e| !e.is_empty()) {
        return Ok(endpoint);
    }
    let mode = ini::load_config(
        env,
        None,
        Preferred::Config,
        |env| env.var("AWS_EC2_METADATA_SERVICE_ENDPOINT_MODE"),
        |profile, _| profile.get("ec2_metadata_service_endpoint_mode").cloned(),
    )
    .unwrap_or_else(|| "IPv4".into());
    match mode.as_str() {
        "IPv4" => Ok("http://169.254.169.254".into()),
        "IPv6" => Ok("http://[fd00:ec2::254]".into()),
        other => Err(ProviderError::stop(format!("Unsupported endpoint mode: {other}. Select from IPv4,IPv6"))),
    }
}

/// `fromInstanceMetadata(init)()` for a fresh provider (`staticStabilityProvider` has no past
/// credentials, so only the expired-credentials extension applies).
pub async fn from_instance_metadata(env: &Env, profile: Option<&str>) -> Result<Identity, ProviderError> {
    let endpoint = instance_metadata_endpoint(env)?;
    let parsed = url::Url::parse(&endpoint).map_err(|e| ProviderError::stop(e.to_string()))?;
    let base = format!(
        "{}://{}{}",
        parsed.scheme(),
        match parsed.host() {
            Some(url::Host::Ipv6(ip)) => format!("[{ip}]"),
            Some(host) => host.to_string(),
            None => String::new(),
        },
        parsed.port().map(|p| format!(":{p}")).unwrap_or_default()
    );
    let token = metadata_request(reqwest::Method::PUT, &format!("{base}/latest/api/token"), &[("x-aws-ec2-metadata-token-ttl-seconds", "21600".into())]).await;
    let identity = match token {
        Ok(token) => credentials_with(env, profile, &base, Some(token)).await?,
        Err(e) if e.status == Some(400) => return Err(ProviderError::next("EC2 Metadata token request returned error")),
        Err(_) => credentials_with(env, profile, &base, None).await?,
    };
    if identity.expiration.is_some_and(|at| at < chrono::Utc::now().timestamp_millis() as f64) {
        let refresh = 5 * 60 + rand::random_range(0..5 * 60);
        let next = chrono::Utc::now() + chrono::Duration::seconds(refresh);
        tracing::warn!(
            "Attempting credential expiration extension due to a credential service availability issue. A refresh of these credentials will be attempted after {}.\nFor more information, please visit: https://docs.aws.amazon.com/sdkref/latest/guide/feature-static-credentials.html",
            next.to_rfc2822()
        );
        return Ok(Identity { expiration: Some(next.timestamp_millis() as f64), ..identity });
    }
    Ok(identity)
}

/// `getCredentials`: the v1-fallback check, the role name, then its credentials.
async fn credentials_with(env: &Env, profile: Option<&str>, base: &str, token: Option<String>) -> Result<Identity, ProviderError> {
    let mut headers = Vec::new();
    match token {
        Some(token) => headers.push(("x-aws-ec2-metadata-token", token)),
        None => {
            let from_env = env.var("AWS_EC2_METADATA_V1_DISABLED").map(|v| !v.is_empty() && v != "false");
            let (merged, _) = ini::profile_values(env, profile, Preferred::Config);
            let from_profile = merged.get("ec2_metadata_v1_disabled").is_some_and(|v| !v.is_empty() && v != "false");
            let blocked = from_env.unwrap_or(from_profile);
            if blocked {
                let mut causes = Vec::new();
                if from_env.is_none() && from_profile {
                    causes.push("config file profile (ec2_metadata_v1_disabled)");
                }
                if from_env == Some(true) {
                    causes.push("process environment variable (AWS_EC2_METADATA_V1_DISABLED)");
                }
                return Err(ProviderError::next(format!(
                    "AWS EC2 Metadata v1 fallback has been blocked by AWS SDK configuration in the following: [{}].",
                    causes.join(", ")
                )));
            }
        }
    }
    let path = format!("{base}/latest/meta-data/iam/security-credentials/");
    let role = metadata_request(reqwest::Method::GET, &path, &headers).await?;
    let body = metadata_request(reqwest::Method::GET, &format!("{path}{}", role.trim()), &headers).await?;
    imds_credentials(&body)
}

/// `getInstanceMetadataRegion` of the region loader (`AWS_EC2_METADATA_DISABLED`, a 60 s negative cache).
pub async fn instance_metadata_region(env: &Env) -> Option<String> {
    if env.non_empty("AWS_EC2_METADATA_DISABLED").is_some() {
        return None;
    }
    if env.state().imds_region_negative_until.lock().is_some_and(|until| Instant::now() < until) {
        return None;
    }
    let base = if let Some(endpoint) = env.non_empty("AWS_EC2_METADATA_SERVICE_ENDPOINT") {
        endpoint.trim_end_matches('/').to_string()
    } else if env.var("AWS_EC2_METADATA_SERVICE_ENDPOINT_MODE").as_deref() == Some("IPv6") {
        "http://[fd00:ec2::254]".to_string()
    } else {
        "http://169.254.169.254".to_string()
    };
    let lookup = async {
        let token = metadata_request(reqwest::Method::PUT, &format!("{base}/latest/api/token"), &[("x-aws-ec2-metadata-token-ttl-seconds", "21600".into())])
            .await
            .ok()?;
        let region =
            metadata_request(reqwest::Method::GET, &format!("{base}/latest/meta-data/placement/region"), &[("x-aws-ec2-metadata-token", token)]).await.ok()?;
        Some(region.trim().to_string()).filter(|r| !r.is_empty())
    };
    let region = lookup.await;
    if region.is_none() {
        *env.state().imds_region_negative_until.lock() = Some(Instant::now() + Duration::from_secs(60));
    }
    region
}
