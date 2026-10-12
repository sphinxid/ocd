//! PKCE, state/nonce generation, URL encoding and JWT claim helpers shared by the OAuth flows.

use base64::Engine;
use base64::engine::general_purpose::{URL_SAFE_NO_PAD, URL_SAFE};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// PKCE verifier and S256 challenge.
#[derive(Debug, Clone, PartialEq)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

const VERIFIER_CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";

/// `Buffer.toString("base64url")` (no padding).
pub fn base64url(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Lenient `Buffer.from(s, "base64url")` decoding (padding optional).
pub fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    let trimmed = input.trim_end_matches('=');
    URL_SAFE_NO_PAD.decode(trimmed).ok().or_else(|| URL_SAFE.decode(input).ok())
}

pub fn random_bytes(len: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; len];
    rand::fill(&mut bytes[..]);
    bytes
}

/// base64url(SHA-256(verifier)).
pub fn challenge(verifier: &str) -> String {
    base64url(&Sha256::digest(verifier.as_bytes()))
}

/// OpenAI/ChatGPT verifier: 43 chars of `A-Za-z0-9-._~` chosen by `byte % 66`.
pub fn verifier_from_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|b| VERIFIER_CHARS[(*b as usize) % VERIFIER_CHARS.len()] as char).collect()
}

/// OpenAI/ChatGPT PKCE (`generatePKCE`).
pub fn openai_pkce() -> Pkce {
    let verifier = verifier_from_bytes(&random_bytes(43));
    let challenge = challenge(&verifier);
    Pkce { verifier, challenge }
}

/// PKCE whose verifier is base64url of `bytes` random bytes (GitLab/Poe: 32, Snowflake: 48).
pub fn base64_pkce(bytes: usize) -> Pkce {
    let verifier = base64url(&random_bytes(bytes));
    let challenge = challenge(&verifier);
    Pkce { verifier, challenge }
}

/// OAuth `state` / `nonce`: base64url of 32 random bytes.
pub fn random_state() -> String {
    base64url(&random_bytes(32))
}

/// `new URLSearchParams(pairs).toString()` (application/x-www-form-urlencoded, space → `+`).
pub fn form_encode<K: AsRef<str>, V: AsRef<str>>(pairs: &[(K, V)]) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in pairs {
        serializer.append_pair(key.as_ref(), value.as_ref());
    }
    serializer.finish()
}

/// `encodeURIComponent`.
pub fn encode_uri_component(value: &str) -> String {
    const SET: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'_')
        .remove(b'.')
        .remove(b'!')
        .remove(b'~')
        .remove(b'*')
        .remove(b'\'')
        .remove(b'(')
        .remove(b')');
    percent_encoding::utf8_percent_encode(value, SET).to_string()
}

/// Decodes the (unverified) claims of a JWT: base64url JSON of the second segment.
pub fn jwt_claims(token: &str) -> Option<Value> {
    let part = token.split('.').nth(1)?;
    if part.is_empty() {
        return None;
    }
    let bytes = base64url_decode(part)?;
    serde_json::from_slice(&bytes).ok()
}

/// ChatGPT account id from one token's claims (`openai.ts` `claim`): `chatgpt_account_id` ??
/// `["https://api.openai.com/auth"].chatgpt_account_id` ?? `organizations[0].id`. Claims that do not match the
/// expected shape (wrong field types) yield `None`, like the upstream schema decode.
pub fn chatgpt_account_id(token: &str) -> Option<String> {
    let claims = jwt_claims(token)?;
    let obj = claims.as_object()?;
    let direct = match obj.get("chatgpt_account_id") {
        None => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return None,
    };
    let organizations = match obj.get("organizations") {
        None => None,
        Some(Value::Array(items)) => {
            let mut ids = Vec::new();
            for item in items {
                match item.get("id") {
                    Some(Value::String(id)) => ids.push(id.clone()),
                    _ => return None,
                }
            }
            Some(ids)
        }
        Some(_) => return None,
    };
    let nested = match obj.get("https://api.openai.com/auth") {
        None => None,
        Some(Value::Object(auth)) => match auth.get("chatgpt_account_id") {
            None => None,
            Some(Value::String(s)) => Some(s.clone()),
            Some(_) => return None,
        },
        Some(_) => return None,
    };
    direct.or(nested).or_else(|| organizations.and_then(|ids| ids.into_iter().next()))
}

/// Codex `extractAccountID`: id_token claims first, then the access token.
pub fn extract_account_id(id_token: Option<&str>, access_token: &str) -> Option<String> {
    id_token.and_then(chatgpt_account_id).or_else(|| chatgpt_account_id(access_token))
}
