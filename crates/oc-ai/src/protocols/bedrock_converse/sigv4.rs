//! AWS Signature Version 4 (header signing), matching `aws4fetch`'s `AwsV4Signer` defaults:
//! non-S3 paths are normalized and double-encoded, the body is hashed, `host` / `x-amz-date` /
//! `x-amz-security-token` are added, and `authorization`, `content-type`, `content-length`,
//! `user-agent`, `presigned-expires`, `expect`, `x-amzn-trace-id`, `range`, `connection` are not signed.

use crate::route::Headers;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

const UNSIGNABLE: &[&str] = &[
    "authorization",
    "content-type",
    "content-length",
    "user-agent",
    "presigned-expires",
    "expect",
    "x-amzn-trace-id",
    "range",
    "connection",
];

#[derive(Debug, Clone, PartialEq)]
pub struct AwsCredentials {
    pub region: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

fn hmac(key: &[u8], data: &str) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("hmac key");
    mac.update(data.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// JS `encodeURIComponent`.
pub fn encode_uri_component(input: &str) -> String {
    let mut out = String::new();
    for b in input.bytes() {
        let c = b as char;
        if c.is_ascii_alphanumeric() || "-_.!~*'()".contains(c) {
            out.push(c);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `encodeRfc3986`: additionally escape `!'()*`.
fn encode_rfc3986(input: &str) -> String {
    let mut out = String::new();
    for c in input.chars() {
        match c {
            '!' | '\'' | '(' | ')' | '*' => out.push_str(&format!("%{:02X}", c as u32)),
            other => out.push(other),
        }
    }
    out
}

fn decode_uri_component(input: &str) -> String {
    percent_encoding::percent_decode_str(input).decode_utf8_lossy().into_owned()
}

fn canonical_path(path: &str) -> String {
    let normalized = path.replace("//", "/");
    let decoded = decode_uri_component(&normalized);
    let once = encode_uri_component(&decoded).replace("%2F", "/");
    let twice = encode_uri_component(&once).replace("%2F", "/");
    encode_rfc3986(&twice)
}

fn canonical_query(url: &url::Url) -> String {
    let mut pairs: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| k != "X-Amz-Signature")
        .map(|(k, v)| (encode_rfc3986(&encode_uri_component(&k)), encode_rfc3986(&encode_uri_component(&v))))
        .collect();
    pairs.sort();
    pairs.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&")
}

/// Sign a request, returning the headers to merge (`authorization`, `x-amz-date`, optional
/// `x-amz-security-token`, `host`).
pub fn sign(
    method: &str,
    url: &str,
    headers: &Headers,
    body: &str,
    credentials: &AwsCredentials,
    service: &str,
    datetime: chrono::DateTime<chrono::Utc>,
) -> Result<Headers, String> {
    let parsed = url::Url::parse(url).map_err(|e| e.to_string())?;
    let host = match parsed.port() {
        Some(port) => format!("{}:{port}", parsed.host_str().unwrap_or("")),
        None => parsed.host_str().unwrap_or("").to_string(),
    };
    let amz_date = datetime.format("%Y%m%dT%H%M%SZ").to_string();
    let date = &amz_date[..8];
    let mut all: Headers = headers.iter().map(|(k, v)| (k.to_lowercase(), v.clone())).collect();
    all.entry("host".into()).or_insert(host);
    all.insert("x-amz-date".into(), amz_date.clone());
    if let Some(token) = &credentials.session_token {
        all.insert("x-amz-security-token".into(), token.clone());
    }
    let mut signed: Vec<(&String, &String)> = all.iter().filter(|(k, _)| !UNSIGNABLE.contains(&k.as_str())).collect();
    signed.sort_by(|a, b| a.0.cmp(b.0));
    let canonical_headers: String = signed.iter().map(|(k, v)| format!("{k}:{}\n", v.trim())).collect();
    let signed_headers = signed.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");
    let payload_hash = sha256_hex(body.as_bytes());
    let canonical_request = format!(
        "{method}\n{}\n{}\n{canonical_headers}\n{signed_headers}\n{payload_hash}",
        canonical_path(parsed.path()),
        canonical_query(&parsed)
    );
    let scope = format!("{date}/{}/{service}/aws4_request", credentials.region);
    let string_to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", sha256_hex(canonical_request.as_bytes()));
    let k_date = hmac(format!("AWS4{}", credentials.secret_access_key).as_bytes(), date);
    let k_region = hmac(&k_date, &credentials.region);
    let k_service = hmac(&k_region, service);
    let k_signing = hmac(&k_service, "aws4_request");
    let signature = hex::encode(hmac(&k_signing, &string_to_sign));
    let mut out = Headers::new();
    out.insert(
        "authorization".into(),
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            credentials.access_key_id
        ),
    );
    out.insert("x-amz-date".into(), amz_date);
    if let Some(token) = &credentials.session_token {
        out.insert("x-amz-security-token".into(), token.clone());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// AWS SigV4 test suite vector `get-vanilla` (service `service`, region `us-east-1`).
    #[test]
    fn aws_test_vector_get_vanilla() {
        let creds = AwsCredentials {
            region: "us-east-1".into(),
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
            session_token: None,
        };
        let dt = chrono::Utc.with_ymd_and_hms(2015, 8, 30, 12, 36, 0).unwrap();
        let out = sign("GET", "https://example.amazonaws.com/", &Headers::new(), "", &creds, "service", dt).unwrap();
        assert_eq!(
            out["authorization"],
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date, Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    #[test]
    fn double_encodes_non_s3_paths() {
        assert_eq!(canonical_path("/model/us.amazon.nova-micro-v1%3A0/converse-stream"), "/model/us.amazon.nova-micro-v1%253A0/converse-stream");
    }
}
