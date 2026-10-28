//! STS `AssumeRole` / `AssumeRoleWithWebIdentity` (`getDefaultRoleAssumer`,
//! `getDefaultRoleAssumerWithWebIdentity` of `@aws-sdk/nested-clients/sts`): AWS query protocol,
//! SigV4 for `AssumeRole`, unsigned web identity calls.

use super::chain::{Identity, ProviderError};
use super::ini::{self, Preferred};
use super::service::{self, Service};
use crate::protocols::bedrock_converse::sigv4::{self, AwsCredentials};
use crate::protocols::cloud_env::Env;
use crate::route::Headers;

/// `resolveRegion`: the profile / caller region, else `stsRegionDefaultResolver` (`AWS_REGION`,
/// the profile's `region` with the credentials file preferred, then `us-east-1` with a warning).
pub fn region(env: &Env, explicit: Option<&str>) -> String {
    if let Some(region) = explicit {
        return region.to_string();
    }
    ini::load_config(env, None, Preferred::Credentials, |env| env.var("AWS_REGION"), |profile, _| profile.get("region").cloned()).unwrap_or_else(|| {
        tracing::warn!("@aws-sdk - WARN - default STS region of us-east-1 used. See @aws-sdk/credential-providers README and set a region explicitly.");
        "us-east-1".into()
    })
}

pub struct AssumeRoleParams {
    pub role_arn: String,
    pub role_session_name: String,
    pub external_id: Option<String>,
    /// `parseInt(duration_seconds || "3600", 10)`; `None` is `NaN`.
    pub duration_seconds: Option<i64>,
}

fn form(action: &str, fields: &[(&str, Option<String>)]) -> String {
    let mut body = format!("Action={action}&Version=2011-06-15");
    for (key, value) in fields {
        if let Some(value) = value {
            body.push('&');
            body.push_str(key);
            body.push('=');
            body.push_str(&service::encode_component(value));
        }
    }
    body
}

/// The thrown service exception's `message` (`Error.Message`, else `"Unknown"`).
fn exception(response: &service::Response) -> ProviderError {
    let message = service::xml_text(&response.body, "Message").or_else(|| service::xml_text(&response.body, "message")).unwrap_or_else(|| "Unknown".into());
    ProviderError::stop(message)
}

async fn call(env: &Env, region: &str, body: String, sign_with: Option<&Identity>) -> Result<String, ProviderError> {
    let endpoint = service::endpoint(env, Service::Sts, region).map_err(ProviderError::stop)?;
    let url = format!("{}/", endpoint.url);
    let client = service::http_client();
    let mut headers = Headers::new();
    headers.insert("content-type".into(), "application/x-www-form-urlencoded".into());
    if let Some(source) = sign_with {
        let credentials = AwsCredentials {
            region: endpoint.signing_region.clone(),
            access_key_id: source.access_key_id.clone(),
            secret_access_key: source.secret_access_key.clone(),
            session_token: source.session_token.clone(),
        };
        let signed = sigv4::sign("POST", &url, &headers, &body, &credentials, "sts", chrono::Utc::now()).map_err(ProviderError::stop)?;
        headers.extend(signed);
    }
    let response = service::send(
        env,
        || {
            let mut request = client.post(&url).body(body.clone());
            for (k, v) in &headers {
                if k != "host" {
                    request = request.header(k.as_str(), v.as_str());
                }
            }
            request
        },
        service::query_error_code,
    )
    .await
    .map_err(ProviderError::stop)?;
    if response.status >= 300 {
        return Err(exception(&response));
    }
    Ok(response.body)
}

fn credentials(body: &str, operation: &str, role_arn: &str) -> Result<Identity, ProviderError> {
    let block = match (body.find("<Credentials>"), body.find("</Credentials>")) {
        (Some(start), Some(end)) if end > start => &body[start..end],
        _ => "",
    };
    let decode = |tag: &str| service::xml_text(block, tag).filter(|v| !v.is_empty());
    match (decode("AccessKeyId"), decode("SecretAccessKey")) {
        (Some(access_key_id), Some(secret_access_key)) => Ok(Identity {
            access_key_id,
            secret_access_key,
            session_token: decode("SessionToken"),
            expiration: decode("Expiration").and_then(|e| crate::js_date::parse(&e)),
        }),
        _ => Err(ProviderError::stop(format!("Invalid response from STS.{operation} call with role {role_arn}"))),
    }
}

/// `roleAssumer(sourceCreds, params)`.
pub async fn assume_role(env: &Env, region: &str, source: &Identity, params: &AssumeRoleParams) -> Result<Identity, ProviderError> {
    let duration = Some(params.duration_seconds.map(|d| d.to_string()).unwrap_or_else(|| "NaN".into()));
    let body = form(
        "AssumeRole",
        &[
            ("RoleArn", Some(params.role_arn.clone())),
            ("RoleSessionName", Some(params.role_session_name.clone())),
            ("DurationSeconds", duration),
            ("ExternalId", params.external_id.clone()),
        ],
    );
    let response = call(env, region, body, Some(source)).await?;
    credentials(&response, "assumeRole", &params.role_arn)
}

/// `roleAssumerWithWebIdentity(params)`.
pub async fn assume_role_with_web_identity(env: &Env, role_arn: &str, session_name: &str, token: &str) -> Result<Identity, ProviderError> {
    let region = region(env, None);
    let body = form(
        "AssumeRoleWithWebIdentity",
        &[("RoleArn", Some(role_arn.to_string())), ("RoleSessionName", Some(session_name.to_string())), ("WebIdentityToken", Some(token.to_string()))],
    );
    let response = call(env, &region, body, None).await?;
    credentials(&response, "assumeRoleWithWebIdentity", role_arn)
}
