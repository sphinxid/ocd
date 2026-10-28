//! `aws login` sessions (`login_session` profiles, `@aws-sdk/credential-provider-login`): the cached
//! token in `~/.aws/login/cache/<sha256(session)>.json` (or `AWS_LOGIN_CACHE_DIRECTORY`), refreshed
//! within 5 minutes of expiry through the sign-in service's `CreateOAuth2Token` with a DPoP proof
//! signed by the token's P-256 key.

use super::chain::{Identity, ProviderError};
use super::ini::{self, Preferred, Section};
use super::service::{self, Service};
use crate::protocols::cloud_env::Env;
use base64::Engine;
use indexmap::IndexMap;
use serde_json::{Value, json};
use std::path::PathBuf;

const REFRESH_THRESHOLD_MS: f64 = 5.0 * 60.0 * 1000.0;

fn now_ms() -> f64 {
    chrono::Utc::now().timestamp_millis() as f64
}

/// `fromLoginCredentials({ profile })()`.
pub async fn from_login_profile(env: &Env, name: &str) -> Result<Identity, ProviderError> {
    let profiles = ini::known_profiles(env);
    let Some(profile) = profiles.get(name).filter(|p| p.get("login_session").is_some_and(|v| !v.is_empty())) else {
        return Err(ProviderError::next(format!("Profile {name} does not contain login_session.")));
    };
    let fetcher = Fetcher { env, profile: profile.clone() };
    fetcher.load_credentials().await
}

type Token = IndexMap<String, Value>;

struct Fetcher<'a> {
    env: &'a Env,
    profile: Section,
}

fn expires_at(token: &Token) -> f64 {
    token
        .get("accessToken")
        .and_then(|a| a.get("expiresAt"))
        .and_then(|v| match v {
            Value::String(s) => crate::js_date::parse(s),
            Value::Number(n) => n.as_f64(),
            _ => None,
        })
        .unwrap_or(f64::NAN)
}

fn to_credentials(token: &Token) -> Identity {
    let access = token.get("accessToken").cloned().unwrap_or(Value::Null);
    let s = |k: &str| access.get(k).and_then(|v| v.as_str()).unwrap_or_default().to_string();
    Identity {
        access_key_id: s("accessKeyId"),
        secret_access_key: s("secretAccessKey"),
        session_token: access.get("sessionToken").and_then(|v| v.as_str()).map(String::from),
        expiration: Some(expires_at(token)).filter(|e| !e.is_nan()),
    }
}

impl Fetcher<'_> {
    fn session(&self) -> &str {
        &self.profile["login_session"]
    }

    fn token_path(&self) -> PathBuf {
        let dir = match self.env.var("AWS_LOGIN_CACHE_DIRECTORY") {
            Some(dir) => PathBuf::from(dir),
            None => self.env.os_home().join(".aws").join("login").join("cache"),
        };
        let digest = ring::digest::digest(&ring::digest::SHA256, self.session().as_bytes());
        dir.join(format!("{}.json", hex::encode(digest.as_ref())))
    }

    /// `loadToken`.
    fn load_token(&self) -> Result<Token, ProviderError> {
        let path = self.token_path();
        let path_text = path.to_string_lossy().to_string();
        let fail = |error: String| ProviderError::stop(format!("Failed to load token from {path_text}: {error}"));
        let text = std::fs::read_to_string(&path).map_err(|e| fail(format!("Error: {}", super::chain::fs_error(&e, "open", &path_text))))?;
        let token: Token =
            super::imds::parse_json(&text).and_then(|v| serde_json::from_value(v).map_err(|e| e.to_string())).map_err(|e| fail(format!("SyntaxError: {e}")))?;
        let truthy = |v: Option<&Value>| match v {
            None | Some(Value::Null) => false,
            Some(Value::Bool(b)) => *b,
            Some(Value::String(s)) => !s.is_empty(),
            Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
            _ => true,
        };
        let mut missing: Vec<&str> = ["accessToken", "clientId", "refreshToken", "dpopKey"].into_iter().filter(|k| !truthy(token.get(*k))).collect();
        if !truthy(token.get("accessToken").and_then(|a| a.get("accountId"))) {
            missing.push("accountId");
        }
        if !missing.is_empty() {
            return Err(fail(format!("CredentialsProviderError: Token validation failed, missing fields: {}", missing.join(", "))));
        }
        Ok(token)
    }

    async fn load_credentials(&self) -> Result<Identity, ProviderError> {
        let token = self.load_token()?;
        if expires_at(&token) - now_ms() <= REFRESH_THRESHOLD_MS {
            return self.refresh(token).await;
        }
        Ok(to_credentials(&token))
    }

    async fn refresh(&self, token: Token) -> Result<Identity, ProviderError> {
        let disk = self.load_token().unwrap_or_else(|_| token.clone());
        let now = now_ms();
        let fresh = if expires_at(&disk) <= now && expires_at(&token) > now {
            token
        } else {
            disk
        };
        if expires_at(&fresh) - now_ms() > REFRESH_THRESHOLD_MS {
            return Ok(to_credentials(&fresh));
        }
        match self.create_token(&fresh).await {
            Ok(identity) => Ok(identity),
            Err(Failure::Service { name, error, text }) => {
                if name == "AccessDeniedException" {
                    let message = match error.as_deref() {
                        Some("TOKEN_EXPIRED") => "Your session has expired. Please reauthenticate.".to_string(),
                        Some("USER_CREDENTIALS_CHANGED") => {
                            "Unable to refresh credentials because of a change in your password. Please reauthenticate with your new password.".to_string()
                        }
                        Some("INSUFFICIENT_PERMISSIONS") => {
                            "Unable to refresh credentials due to insufficient permissions. You may be missing permission for the 'CreateOAuth2Token' action."
                                .to_string()
                        }
                        _ => format!("Failed to refresh token: {text}. Please re-authenticate using `aws login`"),
                    };
                    return Err(ProviderError::stop(message));
                }
                if expires_at(&fresh) > now_ms() {
                    return Ok(to_credentials(&fresh));
                }
                Err(ProviderError::next(format!("Failed to refresh token: {text}. Please re-authenticate using aws login")))
            }
        }
    }

    /// The region `SigninClient` gets: the profile's, `AWS_REGION`, then its own region loader.
    async fn region(&self) -> Option<String> {
        if let Some(region) = self.profile.get("region") {
            return Some(region.clone());
        }
        if let Some(region) = self.env.var("AWS_REGION") {
            return Some(region);
        }
        if let Some(region) = ini::load_config(self.env, None, Preferred::Credentials, |_| None, |profile, _| profile.get("region").cloned()) {
            return Some(region);
        }
        super::imds::instance_metadata_region(self.env).await
    }

    async fn create_token(&self, fresh: &Token) -> Result<Identity, Failure> {
        let service_error = |text: String| Failure::Service { name: "Error".into(), error: None, text };
        let Some(region) = self.region().await else { return Err(service_error("Error: Region is missing".into())) };
        let endpoint = service::endpoint(self.env, Service::Signin, &region).map_err(|e| service_error(format!("Error: {e}")))?;
        let url = format!("{}/v1/token", endpoint.url);
        let mut body = serde_json::Map::new();
        if let Some(v) = fresh.get("clientId") {
            body.insert("clientId".into(), v.clone());
        }
        body.insert("grantType".into(), json!("refresh_token"));
        if let Some(v) = fresh.get("refreshToken") {
            body.insert("refreshToken".into(), v.clone());
        }
        let body = Value::Object(body).to_string();
        let parsed = url::Url::parse(&url).map_err(|e| service_error(format!("TypeError: {e}")))?;
        let htu = format!(
            "{}://{}{}{}",
            parsed.scheme(),
            parsed.host_str().unwrap_or_default(),
            parsed.port().map(|p| format!(":{p}")).unwrap_or_default(),
            parsed.path()
        );
        // The DPoP middleware throws inside `client.send`, like any other failure of the call.
        let provider_error = |e: ProviderError| Failure::Service {
            name: "CredentialsProviderError".into(),
            error: None,
            text: format!("CredentialsProviderError: {}", e.message),
        };
        self.dpop("POST", &htu).map_err(provider_error)?;
        let client = service::http_client();
        let response = service::send(
            self.env,
            || {
                let request = client.post(&url).header("content-type", "application/json").body(body.clone());
                match self.dpop("POST", &htu) {
                    Ok(proof) => request.header("DPoP", proof),
                    Err(_) => request,
                }
            },
            |r| Some(service::json_exception(r).0),
        );
        let response = response.await.map_err(|e| service_error(format!("Error: {e}")))?;
        if response.status >= 300 {
            let (name, message) = service::json_exception(&response);
            let error = serde_json::from_str::<Value>(&response.body).ok().and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from));
            return Err(Failure::Service { text: format!("{name}: {message}"), name, error });
        }
        let parsed: Value = super::imds::parse_json(&response.body).map_err(|e| service_error(format!("SyntaxError: {e}")))?;
        let access = parsed.get("accessToken").cloned().unwrap_or(Value::Null);
        let s = |v: &Value, k: &str| v.get(k).and_then(|x| x.as_str()).filter(|x| !x.is_empty()).map(String::from);
        let (Some(access_key_id), Some(secret_access_key), Some(session_token), Some(refresh_token)) =
            (s(&access, "accessKeyId"), s(&access, "secretAccessKey"), s(&access, "sessionToken"), s(&parsed, "refreshToken"))
        else {
            return Err(provider_error(ProviderError::stop("Token refresh response missing required fields")));
        };
        let expires_in = parsed.get("expiresIn").and_then(|v| v.as_f64()).unwrap_or(900.0);
        let expiration = chrono::DateTime::from_timestamp_millis((now_ms() + expires_in * 1000.0) as i64).unwrap_or_default();
        let mut updated = fresh.clone();
        let mut access_token = match fresh.get("accessToken") {
            Some(Value::Object(map)) => map.clone(),
            _ => serde_json::Map::new(),
        };
        access_token.insert("accessKeyId".into(), json!(access_key_id));
        access_token.insert("secretAccessKey".into(), json!(secret_access_key));
        access_token.insert("sessionToken".into(), json!(session_token));
        access_token.insert("expiresAt".into(), json!(expiration.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()));
        updated.insert("accessToken".into(), Value::Object(access_token));
        updated.insert("refreshToken".into(), json!(refresh_token));
        let path = self.token_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let text = serde_json::to_string_pretty(&updated).unwrap_or_default();
        std::fs::write(&path, text).map_err(|e| service_error(format!("Error: {}", super::chain::fs_error(&e, "open", &path.to_string_lossy()))))?;
        Ok(to_credentials(&updated))
    }

    /// `generateDpop`: an ES256 `dpop+jwt` with the public key as JWK.
    fn dpop(&self, method: &str, endpoint: &str) -> Result<String, ProviderError> {
        let token = self.load_token()?;
        let pem = token.get("dpopKey").and_then(|v| v.as_str()).unwrap_or_default();
        dpop_proof(pem, method, endpoint).map_err(|e| ProviderError::stop(format!("Failed to generate Dpop proof: {e}")))
    }
}

enum Failure {
    /// A thrown error: its `name`, the `error` field of an `AccessDeniedException`, and `String(error)`.
    Service { name: String, error: Option<String>, text: String },
}

/// Reads a SEC1 `EC PRIVATE KEY` PEM: the private scalar and the uncompressed public point.
fn sec1_key(pem: &str) -> Result<(Vec<u8>, Vec<u8>), String> {
    let body: String = pem.lines().filter(|l| !l.starts_with("-----")).collect::<Vec<_>>().join("");
    let der = base64::engine::general_purpose::STANDARD.decode(body.trim()).map_err(|e| e.to_string())?;
    fn element(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
        let tag = *input.first()?;
        let first = *input.get(1)? as usize;
        let (len, rest) = if first < 0x80 {
            (first, &input[2..])
        } else {
            let n = first & 0x7f;
            let bytes = input.get(2..2 + n)?;
            (bytes.iter().fold(0usize, |acc, &b| (acc << 8) | b as usize), &input[2 + n..])
        };
        (rest.len() >= len).then(|| (tag, &rest[..len], &rest[len..]))
    }
    let invalid = || "Invalid EC private key".to_string();
    let (_, sequence, _) = element(&der).ok_or_else(invalid)?;
    let (_, _version, rest) = element(sequence).ok_or_else(invalid)?;
    let (_, private, mut rest) = element(rest).ok_or_else(invalid)?;
    let mut public = None;
    while let Some((tag, content, next)) = element(rest) {
        if tag == 0xa1 {
            let (_, bits, _) = element(content).ok_or_else(invalid)?;
            public = bits.get(1..).map(|b| b.to_vec());
        }
        rest = next;
    }
    let public = public.ok_or_else(invalid)?;
    Ok((private.to_vec(), public))
}

/// The DPoP proof for one request.
pub fn dpop_proof(pem: &str, method: &str, endpoint: &str) -> Result<String, String> {
    use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair};
    let (private, public) = sec1_key(pem)?;
    let rng = ring::rand::SystemRandom::new();
    let key = EcdsaKeyPair::from_private_key_and_public_key(&ECDSA_P256_SHA256_FIXED_SIGNING, &private, &public, &rng).map_err(|e| e.to_string())?;
    if public.len() != 65 {
        return Err("Invalid EC public key".into());
    }
    let b64 = |data: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data);
    let header = json!({"alg": "ES256", "typ": "dpop+jwt", "jwk": {"kty": "EC", "crv": "P-256", "x": b64(&public[1..33]), "y": b64(&public[33..65])}});
    let payload = json!({"jti": uuid::Uuid::new_v4().to_string(), "htm": method, "htu": endpoint, "iat": chrono::Utc::now().timestamp()});
    let message = format!("{}.{}", b64(header.to_string().as_bytes()), b64(payload.to_string().as_bytes()));
    let signature = key.sign(&rng, message.as_bytes()).map_err(|e| e.to_string())?;
    Ok(format!("{message}.{}", b64(signature.as_ref())))
}
