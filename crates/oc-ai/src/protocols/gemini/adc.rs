//! Google Application Default Credentials (subset of `google-auth-library` used by
//! `providers/google-vertex-shared.ts`): `authorized_user` and `service_account` JSON files
//! (`GOOGLE_APPLICATION_CREDENTIALS`, `$CLOUDSDK_CONFIG/application_default_credentials.json`,
//! the gcloud well-known file) and the GCE metadata server. Scope `cloud-platform`.

use crate::route::auth::{self, AuthFailure};
use crate::route::{Auth, Credential};
use base64::Engine;
use parking_lot::Mutex;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
const SOURCE: &str = "Google Application Default Credentials";

fn missing() -> AuthFailure {
    AuthFailure::missing(SOURCE)
}

fn home() -> Option<PathBuf> {
    dirs::home_dir()
}

/// Locate the ADC JSON file.
pub fn credentials_file() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("GOOGLE_APPLICATION_CREDENTIALS") {
        if !path.is_empty() {
            return Some(PathBuf::from(path));
        }
    }
    if let Ok(config) = std::env::var("CLOUDSDK_CONFIG") {
        if !config.is_empty() {
            let dir = if config == "~" {
                home()?
            } else if let Some(rest) = config.strip_prefix("~/") {
                home()?.join(rest)
            } else {
                PathBuf::from(config)
            };
            let file = dir.join("application_default_credentials.json");
            return if file.exists() { Some(file) } else { None };
        }
    }
    let base = if cfg!(windows) {
        std::env::var("APPDATA").ok().map(PathBuf::from)?.join("gcloud")
    } else {
        home()?.join(".config").join("gcloud")
    };
    let file = base.join("application_default_credentials.json");
    if file.exists() { Some(file) } else { None }
}

fn b64url(data: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

/// Build a signed RS256 JWT assertion for a service account.
pub fn service_account_assertion(client_email: &str, private_key_pem: &str, token_uri: &str, now: u64) -> Result<String, String> {
    use rsa::pkcs1v15::SigningKey;
    use rsa::pkcs8::DecodePrivateKey;
    use rsa::signature::{SignatureEncoding, Signer};
    let header = b64url(br#"{"alg":"RS256","typ":"JWT"}"#);
    let claims = serde_json::json!({"iss": client_email, "scope": SCOPE, "aud": token_uri, "iat": now, "exp": now + 3600});
    let payload = b64url(claims.to_string().as_bytes());
    let input = format!("{header}.{payload}");
    let key = rsa::RsaPrivateKey::from_pkcs8_pem(private_key_pem).map_err(|e| e.to_string())?;
    let signer = SigningKey::<sha2::Sha256>::new(key);
    let signature = signer.sign(input.as_bytes()).to_bytes();
    Ok(format!("{input}.{}", b64url(&signature)))
}

async fn fetch_token(client: &reqwest::Client) -> Result<(String, u64), AuthFailure> {
    let now = chrono::Utc::now().timestamp() as u64;
    let token_response = if let Some(file) = credentials_file() {
        let text = std::fs::read_to_string(&file).map_err(|_| missing())?;
        let creds: Value = serde_json::from_str(&text).map_err(|_| missing())?;
        let s = |k: &str| creds.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        match creds.get("type").and_then(Value::as_str) {
            Some("authorized_user") => client
                .post("https://oauth2.googleapis.com/token")
                .form(&[
                    ("client_id", s("client_id")),
                    ("client_secret", s("client_secret")),
                    ("refresh_token", s("refresh_token")),
                    ("grant_type", "refresh_token".to_string()),
                ])
                .send()
                .await
                .map_err(|_| missing())?,
            Some("service_account") => {
                let token_uri = creds.get("token_uri").and_then(Value::as_str).unwrap_or("https://oauth2.googleapis.com/token").to_string();
                let assertion = service_account_assertion(&s("client_email"), &s("private_key"), &token_uri, now).map_err(|_| missing())?;
                client
                    .post(&token_uri)
                    .form(&[("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer".to_string()), ("assertion", assertion)])
                    .send()
                    .await
                    .map_err(|_| missing())?
            }
            _ => return Err(missing()),
        }
    } else {
        client
            .get("https://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token")
            .header("Metadata-Flavor", "Google")
            .timeout(Duration::from_secs(3))
            .send()
            .await
            .map_err(|_| missing())?
    };
    if !token_response.status().is_success() {
        return Err(missing());
    }
    let body: Value = token_response.json().await.map_err(|_| missing())?;
    let token = body.get("access_token").and_then(Value::as_str).filter(|t| !t.is_empty()).ok_or_else(missing)?.to_string();
    let expires_in = body.get("expires_in").and_then(Value::as_u64).unwrap_or(3600);
    Ok((token, expires_in))
}

/// `Authorization: Bearer <ADC token>` with in-memory caching (refreshed 60 s before expiry).
pub fn google_adc_auth(_project: Option<String>) -> Arc<dyn Auth> {
    let cache: Arc<Mutex<Option<(String, Instant)>>> = Arc::new(Mutex::new(None));
    let client = oc_util::http::client();
    auth::bearer(Credential::dynamic(move || {
        let cache = cache.clone();
        let client = client.clone();
        Box::pin(async move {
            if let Some((token, until)) = cache.lock().clone() {
                if Instant::now() < until {
                    return Ok(token);
                }
            }
            let (token, expires_in) = fetch_token(&client).await?;
            let until = Instant::now() + Duration::from_secs(expires_in.saturating_sub(60));
            *cache.lock() = Some((token.clone(), until));
            Ok(token)
        })
    }))
}
