//! AWS credential resolution and Bedrock auth (`protocols/utils/bedrock-auth.ts`).
//!
//! The default chain is `fromNodeProviderChain({ ignoreCache: true, profile })` of
//! `@aws-sdk/credential-providers` 3.1057.0 (credential-provider-node 3.972.80), re-run on every
//! request: environment variables, the shared `config` / `credentials` files for the profile
//! (static keys, assume-role through `source_profile` or `credential_source`, web identity,
//! `credential_process`, IAM Identity Center SSO, `aws login` sessions), `credential_process`,
//! the web identity token file variables, then the ECS/EKS container endpoint or EC2 instance
//! metadata. See the submodules for each provider.

mod chain;
mod imds;
pub mod ini;
mod login;
mod service;
mod sso;
mod sts;

use super::sigv4::{self, AwsCredentials};
use crate::error::AIError;
use crate::protocols::cloud_env::Env;
use crate::route::auth::{self, AuthFailure, AuthInput};
use crate::route::{Auth, Credential, Headers};
use async_trait::async_trait;
use std::sync::Arc;

pub use chain::{Identity, ProviderError};
pub use ini::parse_ini;
pub use login::dpop_proof;

/// `resolveRegion`.
pub fn resolve_region(region: Option<&str>, credentials_region: Option<&str>) -> String {
    region
        .or(credentials_region)
        .map(String::from)
        .or_else(|| std::env::var("AWS_REGION").ok())
        .or_else(|| std::env::var("AWS_DEFAULT_REGION").ok())
        .unwrap_or_else(|| "us-east-1".into())
}

/// Profiles defined in the shared config and credentials files.
pub fn list_profiles() -> Vec<String> {
    let mut out: Vec<String> =
        ini::known_profiles(&Env::process()).into_keys().filter(|k| !k.starts_with("sso-session.") && !k.starts_with("services.")).collect();
    out.sort();
    out.dedup();
    out
}

fn chain_error(message: impl std::fmt::Display) -> AuthFailure {
    AuthFailure::Error(AIError::authentication(format!("AWS default credential chain failed: {message}")))
}

/// Resolve credentials through the default chain with the process environment.
#[allow(clippy::result_large_err)]
pub async fn default_chain(region: &str, profile: Option<&str>) -> Result<AwsCredentials, AuthFailure> {
    default_chain_with(&Env::process(), region, profile).await
}

/// [`default_chain`] with an explicit environment (tests).
#[allow(clippy::result_large_err)]
pub async fn default_chain_with(env: &Env, region: &str, profile: Option<&str>) -> Result<AwsCredentials, AuthFailure> {
    let identity = chain::default_provider(env, profile).await.map_err(|e| chain_error(e.message))?;
    Ok(AwsCredentials {
        region: region.into(),
        access_key_id: identity.access_key_id,
        secret_access_key: identity.secret_access_key,
        session_token: identity.session_token,
    })
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
        let ini = parse_ini("[default]\naws_access_key_id = A\n[profile dev]\nregion=us-west-2\n").unwrap();
        assert_eq!(ini["default"]["aws_access_key_id"], "A");
        assert_eq!(ini["profile.dev"]["region"], "us-west-2");
    }

    #[tokio::test]
    async fn static_sigv4_signs() {
        let auth = bedrock_auth(
            BedrockAuthOptions {
                credentials: Some(AwsCredentials {
                    region: "x".into(),
                    access_key_id: "AK".into(),
                    secret_access_key: "SK".into(),
                    session_token: Some("T".into()),
                }),
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
