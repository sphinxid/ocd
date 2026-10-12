//! Auth combinators (`packages/ai/src/route/auth.ts`, `auth-options.ts`).

use super::Headers;
use crate::error::AIError;
use async_trait::async_trait;
use futures::future::BoxFuture;
use std::sync::Arc;

/// Input to [`Auth::apply`].
pub struct AuthInput<'a> {
    pub method: &'a str,
    pub url: &'a str,
    pub body: &'a str,
    pub headers: Headers,
}

/// Credential / auth failure before mapping to `AIError`.
#[derive(Debug, Clone, PartialEq)]
pub enum AuthFailure {
    /// `MissingCredentialError(source, message)`.
    Missing { source: String, message: String },
    /// Already-classified error (e.g. SigV4 signing failure).
    Error(AIError),
}

impl AuthFailure {
    pub fn missing(source: &str) -> Self {
        Self::Missing { source: source.into(), message: format!("Missing auth credential: {source}") }
    }
    /// `toAIError`.
    pub fn into_error(self) -> AIError {
        match self {
            Self::Missing { message, .. } => AIError::authentication(message),
            Self::Error(e) => e,
        }
    }
}

/// Header-producing auth step.
#[async_trait]
pub trait Auth: Send + Sync {
    async fn apply(&self, input: AuthInput<'_>) -> Result<Headers, AuthFailure>;
}

/// Run an auth definition, mapping failures to `AIError` (`Auth.toEffect`).
pub async fn apply(auth: &dyn Auth, input: AuthInput<'_>) -> Result<Headers, AIError> {
    auth.apply(input).await.map_err(AuthFailure::into_error)
}

pub type DynamicSecret = Arc<dyn Fn() -> BoxFuture<'static, Result<String, AuthFailure>> + Send + Sync>;

/// A secret source.
#[derive(Clone)]
pub enum Credential {
    /// Literal value; empty string counts as missing.
    Value { secret: String, source: String },
    /// Absent optional value.
    Missing { source: String },
    /// Environment variable (`Auth.config(NAME)`): unset/empty → `"<NAME> is not set"`.
    Env(String),
    /// Async loader (OAuth refresh, external token sources).
    Dynamic(DynamicSecret),
    OrElse(Box<Credential>, Box<Credential>),
}

impl Credential {
    pub fn value(secret: impl Into<String>) -> Self {
        Self::Value { secret: secret.into(), source: "value".into() }
    }
    /// `Auth.optional(secret, source)`.
    pub fn optional(secret: Option<String>, source: &str) -> Self {
        match secret {
            Some(secret) => Self::Value { secret, source: source.into() },
            None => Self::Missing { source: source.into() },
        }
    }
    pub fn env(name: &str) -> Self {
        Self::Env(name.into())
    }
    pub fn dynamic(f: impl Fn() -> BoxFuture<'static, Result<String, AuthFailure>> + Send + Sync + 'static) -> Self {
        Self::Dynamic(Arc::new(f))
    }
    pub fn or_else(self, other: Credential) -> Self {
        Self::OrElse(Box::new(self), Box::new(other))
    }

    pub fn load(&self) -> BoxFuture<'_, Result<String, AuthFailure>> {
        Box::pin(async move {
            match self {
                Self::Value { secret, source } => {
                    if secret.is_empty() {
                        Err(AuthFailure::missing(source))
                    } else {
                        Ok(secret.clone())
                    }
                }
                Self::Missing { source } => Err(AuthFailure::missing(source)),
                Self::Env(name) => match std::env::var(name) {
                    Ok(v) if !v.is_empty() => Ok(v),
                    _ => Err(AuthFailure::Missing { source: name.clone(), message: format!("{name} is not set") }),
                },
                Self::Dynamic(f) => f().await,
                Self::OrElse(a, b) => match a.load().await {
                    Ok(v) => Ok(v),
                    Err(_) => b.load().await,
                },
            }
        })
    }

    /// Synchronously check whether the credential resolves without a dynamic loader.
    pub fn is_available(&self) -> bool {
        match self {
            Self::Value { secret, .. } => !secret.is_empty(),
            Self::Missing { .. } => false,
            Self::Env(name) => std::env::var(name).map(|v| !v.is_empty()).unwrap_or(false),
            Self::Dynamic(_) => true,
            Self::OrElse(a, b) => a.is_available() || b.is_available(),
        }
    }
}

#[derive(Clone, Debug)]
enum Render {
    Bearer,
    Header(String),
    BearerHeader(String),
    Scheme(String),
}

struct CredentialAuth {
    credential: Credential,
    render: Render,
}

#[async_trait]
impl Auth for CredentialAuth {
    async fn apply(&self, input: AuthInput<'_>) -> Result<Headers, AuthFailure> {
        let secret = self.credential.load().await?;
        let mut headers = input.headers;
        match &self.render {
            Render::Bearer => {
                headers.insert("authorization".into(), format!("Bearer {secret}"));
            }
            Render::Header(name) => {
                headers.insert(name.to_lowercase(), secret);
            }
            Render::BearerHeader(name) => {
                headers.insert(name.to_lowercase(), format!("Bearer {secret}"));
            }
            Render::Scheme(name) => {
                headers.insert("authorization".into(), format!("{name} {secret}"));
            }
        }
        Ok(headers)
    }
}

struct NoAuth;

#[async_trait]
impl Auth for NoAuth {
    async fn apply(&self, input: AuthInput<'_>) -> Result<Headers, AuthFailure> {
        Ok(input.headers)
    }
}

struct SetHeaders(Headers);

#[async_trait]
impl Auth for SetHeaders {
    async fn apply(&self, input: AuthInput<'_>) -> Result<Headers, AuthFailure> {
        let mut headers = input.headers;
        for (k, v) in &self.0 {
            headers.insert(k.to_lowercase(), v.clone());
        }
        Ok(headers)
    }
}

struct Remove(String);

#[async_trait]
impl Auth for Remove {
    async fn apply(&self, input: AuthInput<'_>) -> Result<Headers, AuthFailure> {
        let mut headers = input.headers;
        headers.shift_remove(&self.0.to_lowercase());
        Ok(headers)
    }
}

struct AndThen(Arc<dyn Auth>, Arc<dyn Auth>);

#[async_trait]
impl Auth for AndThen {
    async fn apply(&self, input: AuthInput<'_>) -> Result<Headers, AuthFailure> {
        let (method, url, body) = (input.method, input.url, input.body);
        let headers = self.0.apply(input).await?;
        self.1.apply(AuthInput { method, url, body, headers }).await
    }
}

struct OrElse(Arc<dyn Auth>, Arc<dyn Auth>);

#[async_trait]
impl Auth for OrElse {
    async fn apply(&self, input: AuthInput<'_>) -> Result<Headers, AuthFailure> {
        let (method, url, body) = (input.method, input.url, input.body);
        let original = input.headers.clone();
        match self.0.apply(input).await {
            Ok(h) => Ok(h),
            Err(_) => self.1.apply(AuthInput { method, url, body, headers: original }).await,
        }
    }
}

pub type CustomAuthFn =
    Arc<dyn for<'a> Fn(AuthInput<'a>) -> BoxFuture<'a, Result<Headers, AuthFailure>> + Send + Sync>;

struct Custom(CustomAuthFn);

#[async_trait]
impl Auth for Custom {
    async fn apply(&self, input: AuthInput<'_>) -> Result<Headers, AuthFailure> {
        (self.0)(input).await
    }
}

pub fn none() -> Arc<dyn Auth> {
    Arc::new(NoAuth)
}
pub fn passthrough() -> Arc<dyn Auth> {
    none()
}
/// `authorization: Bearer <secret>`.
pub fn bearer(credential: Credential) -> Arc<dyn Auth> {
    Arc::new(CredentialAuth { credential, render: Render::Bearer })
}
/// `<name>: <secret>`.
pub fn header(name: &str, credential: Credential) -> Arc<dyn Auth> {
    Arc::new(CredentialAuth { credential, render: Render::Header(name.into()) })
}
/// `<name>: Bearer <secret>`.
pub fn bearer_header(name: &str, credential: Credential) -> Arc<dyn Auth> {
    Arc::new(CredentialAuth { credential, render: Render::BearerHeader(name.into()) })
}
/// `authorization: <scheme> <secret>`.
pub fn scheme(name: &str, credential: Credential) -> Arc<dyn Auth> {
    Arc::new(CredentialAuth { credential, render: Render::Scheme(name.into()) })
}
pub fn headers(map: Headers) -> Arc<dyn Auth> {
    Arc::new(SetHeaders(map))
}
pub fn remove(name: &str) -> Arc<dyn Auth> {
    Arc::new(Remove(name.into()))
}
pub fn and_then(a: Arc<dyn Auth>, b: Arc<dyn Auth>) -> Arc<dyn Auth> {
    Arc::new(AndThen(a, b))
}
pub fn or_else(a: Arc<dyn Auth>, b: Arc<dyn Auth>) -> Arc<dyn Auth> {
    Arc::new(OrElse(a, b))
}
pub fn custom(f: CustomAuthFn) -> Arc<dyn Auth> {
    Arc::new(Custom(f))
}

/// `AuthOptions.bearer(options, envVars)`: explicit `auth` wins; else `optional(apiKey).orElse(config(env))...`.
pub fn bearer_options(api_key: Option<String>, env: &[&str]) -> Arc<dyn Auth> {
    bearer(key_credential(api_key, env))
}

/// `optional(apiKey, "apiKey").orElse(config(env1)).orElse(...)`.
pub fn key_credential(api_key: Option<String>, env: &[&str]) -> Credential {
    env.iter().fold(Credential::optional(api_key, "apiKey"), |acc, name| acc.or_else(Credential::env(name)))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn run(auth: Arc<dyn Auth>) -> Result<Headers, AIError> {
        apply(auth.as_ref(), AuthInput { method: "POST", url: "https://x", body: "{}", headers: Headers::new() }).await
    }

    #[tokio::test]
    async fn bearer_and_missing() {
        let h = run(bearer_options(Some("k".into()), &[])).await.unwrap();
        assert_eq!(h.get("authorization").unwrap(), "Bearer k");
        let e = run(bearer_options(None, &[])).await.unwrap_err();
        assert_eq!(e.message, "Missing auth credential: apiKey");
        let e = run(bearer_options(None, &["OC_AI_TEST_UNSET_VAR"])).await.unwrap_err();
        assert_eq!(e.message, "OC_AI_TEST_UNSET_VAR is not set");
        let e = run(bearer_options(Some(String::new()), &[])).await.unwrap_err();
        assert_eq!(e.tag(), "Authentication");
    }
}
