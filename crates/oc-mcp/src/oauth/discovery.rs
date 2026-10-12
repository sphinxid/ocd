//! OAuth discovery (SDK `auth.ts`): RFC 9728 protected resource metadata, RFC 8414 / OIDC
//! authorization server metadata, `WWW-Authenticate` parsing and resource URL checks.

use once_cell::sync::Lazy;
use regex::Regex;
use reqwest::header::HeaderMap;
use serde_json::{Map, Value};
use url::Url;

use super::fetch::{Fetcher, HttpRequest, HttpResponse};
use crate::error::McpError;

/// `LATEST_PROTOCOL_VERSION`.
pub const LATEST_PROTOCOL_VERSION: &str = "2025-11-25";

/// RFC 8414 / OIDC authorization server metadata (loose object, kept verbatim).
#[derive(Debug, Clone, PartialEq)]
pub struct AuthServerMetadata(pub Map<String, Value>);

impl AuthServerMetadata {
    fn s(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(Value::as_str)
    }
    fn list(&self, key: &str) -> Option<Vec<String>> {
        self.0.get(key).and_then(Value::as_array).map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
    }
    pub fn issuer(&self) -> &str {
        self.s("issuer").unwrap_or("")
    }
    pub fn authorization_endpoint(&self) -> &str {
        self.s("authorization_endpoint").unwrap_or("")
    }
    pub fn token_endpoint(&self) -> &str {
        self.s("token_endpoint").unwrap_or("")
    }
    pub fn registration_endpoint(&self) -> Option<&str> {
        self.s("registration_endpoint")
    }
    pub fn scopes_supported(&self) -> Option<Vec<String>> {
        self.list("scopes_supported")
    }
    pub fn response_types_supported(&self) -> Vec<String> {
        self.list("response_types_supported").unwrap_or_default()
    }
    pub fn code_challenge_methods_supported(&self) -> Option<Vec<String>> {
        self.list("code_challenge_methods_supported")
    }
    pub fn token_endpoint_auth_methods_supported(&self) -> Option<Vec<String>> {
        self.list("token_endpoint_auth_methods_supported")
    }
    pub fn client_id_metadata_document_supported(&self) -> bool {
        self.0.get("client_id_metadata_document_supported") == Some(&Value::Bool(true))
    }
    pub fn iss_parameter_supported(&self) -> bool {
        self.0.get("authorization_response_iss_parameter_supported") == Some(&Value::Bool(true))
    }

    /// `OAuthMetadataSchema` (RFC 8414).
    pub fn parse_oauth(value: Value) -> Result<Self, McpError> {
        let obj = value.as_object().cloned().ok_or_else(|| invalid("OAuth metadata", "expected object"))?;
        require_string(&obj, "issuer", "OAuth metadata")?;
        require_url(&obj, "authorization_endpoint", "OAuth metadata")?;
        require_url(&obj, "token_endpoint", "OAuth metadata")?;
        require_string_array(&obj, "response_types_supported", "OAuth metadata")?;
        Ok(Self(obj))
    }

    /// `OpenIdProviderDiscoveryMetadataSchema`.
    pub fn parse_oidc(value: Value) -> Result<Self, McpError> {
        let obj = value.as_object().cloned().ok_or_else(|| invalid("OpenID provider metadata", "expected object"))?;
        let what = "OpenID provider metadata";
        require_string(&obj, "issuer", what)?;
        require_url(&obj, "authorization_endpoint", what)?;
        require_url(&obj, "token_endpoint", what)?;
        require_url(&obj, "jwks_uri", what)?;
        require_string_array(&obj, "response_types_supported", what)?;
        require_string_array(&obj, "subject_types_supported", what)?;
        require_string_array(&obj, "id_token_signing_alg_values_supported", what)?;
        Ok(Self(obj))
    }
}

/// RFC 9728 protected resource metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct ResourceMetadata(pub Map<String, Value>);

impl ResourceMetadata {
    pub fn resource(&self) -> &str {
        self.0.get("resource").and_then(Value::as_str).unwrap_or("")
    }
    pub fn authorization_servers(&self) -> Vec<String> {
        self.0
            .get("authorization_servers")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default()
    }
    pub fn scopes_supported(&self) -> Option<Vec<String>> {
        self.0
            .get("scopes_supported")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
    }

    pub fn parse(value: Value) -> Result<Self, McpError> {
        let obj = value.as_object().cloned().ok_or_else(|| invalid("protected resource metadata", "expected object"))?;
        require_url(&obj, "resource", "protected resource metadata")?;
        Ok(Self(obj))
    }
}

fn invalid(what: &str, detail: &str) -> McpError {
    McpError::other(format!("Invalid {what}: {detail}"))
}

fn require_string(obj: &Map<String, Value>, key: &str, what: &str) -> Result<(), McpError> {
    if obj.get(key).is_some_and(Value::is_string) {
        Ok(())
    } else {
        Err(invalid(what, &format!("{key} must be a string")))
    }
}

fn require_url(obj: &Map<String, Value>, key: &str, what: &str) -> Result<(), McpError> {
    match obj.get(key).and_then(Value::as_str).map(Url::parse) {
        Some(Ok(url)) if !matches!(url.scheme(), "javascript" | "data" | "vbscript") => Ok(()),
        _ => Err(invalid(what, &format!("{key} must be a valid URL"))),
    }
}

fn require_string_array(obj: &Map<String, Value>, key: &str, what: &str) -> Result<(), McpError> {
    match obj.get(key).and_then(Value::as_array) {
        Some(items) if items.iter().all(Value::is_string) => Ok(()),
        _ => Err(invalid(what, &format!("{key} must be an array of strings"))),
    }
}

/// `OAuthDiscoveryState`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DiscoveryState {
    pub authorization_server_url: String,
    pub resource_metadata_url: Option<String>,
    pub resource_metadata: Option<ResourceMetadata>,
    pub authorization_server_metadata: Option<AuthServerMetadata>,
}

/// Result of `extractWWWAuthenticateParams`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WwwAuthenticate {
    pub resource_metadata_url: Option<Url>,
    pub scope: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

fn www_field(header: &str, field: &str) -> Option<String> {
    static CACHE: Lazy<parking_lot::Mutex<std::collections::HashMap<String, Regex>>> =
        Lazy::new(|| parking_lot::Mutex::new(Default::default()));
    let mut cache = CACHE.lock();
    let re = cache
        .entry(field.to_string())
        .or_insert_with(|| Regex::new(&format!(r#"{}=(?:"([^"]+)"|([^\s,]+))"#, regex::escape(field))).unwrap());
    let captures = re.captures(header)?;
    captures.get(1).or_else(|| captures.get(2)).map(|m| m.as_str().to_string()).filter(|s| !s.is_empty())
}

/// `extractWWWAuthenticateParams`.
pub fn parse_www_authenticate(headers: &HeaderMap) -> WwwAuthenticate {
    let Some(header) = headers.get("www-authenticate").and_then(|v| v.to_str().ok()) else {
        return WwwAuthenticate::default();
    };
    parse_www_authenticate_value(header)
}

pub fn parse_www_authenticate_value(header: &str) -> WwwAuthenticate {
    let mut parts = header.split(' ');
    let kind = parts.next().unwrap_or("");
    let scheme = parts.next().unwrap_or("");
    if kind.to_lowercase() != "bearer" || scheme.is_empty() {
        return WwwAuthenticate::default();
    }
    WwwAuthenticate {
        resource_metadata_url: www_field(header, "resource_metadata").and_then(|u| Url::parse(&u).ok()),
        scope: www_field(header, "scope"),
        error: www_field(header, "error"),
        error_description: www_field(header, "error_description"),
    }
}

/// `resourceUrlFromServerUrl`: the server URL with the fragment stripped.
pub fn resource_url_from_server_url(url: &Url) -> Url {
    let mut url = url.clone();
    url.set_fragment(None);
    url
}

/// `checkResourceAllowed`.
pub fn check_resource_allowed(requested: &Url, configured: &Url) -> bool {
    if requested.origin() != configured.origin() {
        return false;
    }
    if requested.path().len() < configured.path().len() {
        return false;
    }
    let requested_path =
        if requested.path().ends_with('/') { requested.path().to_string() } else { format!("{}/", requested.path()) };
    let configured_path =
        if configured.path().ends_with('/') { configured.path().to_string() } else { format!("{}/", configured.path()) };
    requested_path.starts_with(&configured_path)
}

fn build_well_known_path(prefix: &str, pathname: &str) -> String {
    let pathname = pathname.strip_suffix('/').unwrap_or(pathname);
    format!("/.well-known/{prefix}{pathname}")
}

fn should_attempt_fallback(response: &HttpResponse, pathname: &str) -> bool {
    if pathname == "/" {
        return false;
    }
    (400..500).contains(&response.status) || response.status == 502
}

async fn try_metadata(fetch: &Fetcher, url: Url) -> Result<HttpResponse, McpError> {
    fetch.fetch(HttpRequest::get(url).header("MCP-Protocol-Version", LATEST_PROTOCOL_VERSION)).await
}

/// `discoverOAuthProtectedResourceMetadata`.
pub async fn discover_protected_resource(
    fetch: &Fetcher,
    server_url: &Url,
    resource_metadata_url: Option<&Url>,
) -> Result<ResourceMetadata, McpError> {
    let response = match resource_metadata_url {
        Some(url) => try_metadata(fetch, url.clone()).await?,
        None => {
            let path = build_well_known_path("oauth-protected-resource", server_url.path());
            let url = server_url.join(&path).map_err(|e| McpError::other(e.to_string()))?;
            let response = try_metadata(fetch, url).await?;
            if should_attempt_fallback(&response, server_url.path()) {
                let url = server_url
                    .join("/.well-known/oauth-protected-resource")
                    .map_err(|e| McpError::other(e.to_string()))?;
                try_metadata(fetch, url).await?
            } else {
                response
            }
        }
    };
    if response.status == 404 {
        return Err(McpError::other("Resource server does not implement OAuth 2.0 Protected Resource Metadata."));
    }
    if !response.ok() {
        return Err(McpError::other(format!(
            "HTTP {} trying to load well-known OAuth protected resource metadata.",
            response.status
        )));
    }
    ResourceMetadata::parse(response.json()?)
}

/// `buildDiscoveryUrls`.
pub fn build_discovery_urls(authorization_server_url: &Url) -> Vec<(Url, &'static str)> {
    let origin = authorization_server_url.origin().ascii_serialization();
    let join = |path: &str| Url::parse(&format!("{origin}{path}")).expect("valid discovery url");
    if authorization_server_url.path() == "/" {
        return vec![
            (join("/.well-known/oauth-authorization-server"), "oauth"),
            (join("/.well-known/openid-configuration"), "oidc"),
        ];
    }
    let path = authorization_server_url.path();
    let path = path.strip_suffix('/').unwrap_or(path);
    vec![
        (join(&format!("/.well-known/oauth-authorization-server{path}")), "oauth"),
        (join(&format!("/.well-known/openid-configuration{path}")), "oidc"),
        (join(&format!("{path}/.well-known/openid-configuration")), "oidc"),
    ]
}

/// `issuersMatch`: tolerates a single trailing `/` difference.
pub fn issuers_match(a: &str, b: &str) -> bool {
    a == b || (a.ends_with('/') && &a[..a.len() - 1] == b) || (b.ends_with('/') && &b[..b.len() - 1] == a)
}

/// `discoverAuthorizationServerMetadata`. `authorization_server_url` is the string form the
/// issuer echo is compared against.
pub async fn discover_authorization_server(
    fetch: &Fetcher,
    authorization_server_url: &str,
) -> Result<Option<AuthServerMetadata>, McpError> {
    let parsed = Url::parse(authorization_server_url).map_err(|_| McpError::Network {
        message: format!("Invalid URL: {authorization_server_url}"),
        code: Some("ERR_INVALID_URL".into()),
    })?;
    for (url, kind) in build_discovery_urls(&parsed) {
        let response = fetch
            .fetch(
                HttpRequest::get(url.clone())
                    .header("MCP-Protocol-Version", LATEST_PROTOCOL_VERSION)
                    .header("Accept", "application/json"),
            )
            .await?;
        if !response.ok() {
            if (400..500).contains(&response.status) || response.status == 502 {
                continue;
            }
            return Err(McpError::other(format!(
                "HTTP {} trying to load {} metadata from {}",
                response.status,
                if kind == "oauth" { "OAuth" } else { "OpenID provider" },
                url
            )));
        }
        let body = response.json()?;
        let metadata =
            if kind == "oauth" { AuthServerMetadata::parse_oauth(body)? } else { AuthServerMetadata::parse_oidc(body)? };
        let expected = authorization_server_url;
        let issuer = metadata.issuer();
        if !(issuer == expected || (expected.ends_with('/') && issuer == &expected[..expected.len() - 1])) {
            return Err(McpError::other(format!(
                "Issuer mismatch in authorization server metadata (RFC 8414 §3.3): expected {}, received {}",
                serde_json::to_string(expected).unwrap(),
                serde_json::to_string(issuer).unwrap()
            )));
        }
        return Ok(Some(metadata));
    }
    Ok(None)
}

/// Result of `discoverOAuthServerInfo`.
#[derive(Debug, Clone)]
pub struct ServerInfo {
    pub authorization_server_url: String,
    pub authorization_server_metadata: Option<AuthServerMetadata>,
    pub resource_metadata: Option<ResourceMetadata>,
}

/// `discoverOAuthServerInfo`: RFC 9728 first, falling back to treating the server origin as the
/// authorization server. Network failures propagate; other PRM failures are ignored.
pub async fn discover_server_info(
    fetch: &Fetcher,
    server_url: &Url,
    resource_metadata_url: Option<&Url>,
) -> Result<ServerInfo, McpError> {
    let mut authorization_server_url = None;
    let mut resource_metadata = None;
    match discover_protected_resource(fetch, server_url, resource_metadata_url).await {
        Ok(metadata) => {
            authorization_server_url = metadata.authorization_servers().into_iter().next();
            resource_metadata = Some(metadata);
        }
        Err(error @ McpError::Network { .. }) => return Err(error),
        Err(_) => {}
    }
    let authorization_server_url = authorization_server_url
        .unwrap_or_else(|| format!("{}/", server_url.origin().ascii_serialization()));
    let authorization_server_metadata = discover_authorization_server(fetch, &authorization_server_url).await?;
    Ok(ServerInfo { authorization_server_url, authorization_server_metadata, resource_metadata })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn www_authenticate() {
        let parsed = parse_www_authenticate_value(
            r#"Bearer resource_metadata="https://x.test/.well-known/oauth-protected-resource", scope="a b", error=insufficient_scope"#,
        );
        assert_eq!(parsed.resource_metadata_url.unwrap().as_str(), "https://x.test/.well-known/oauth-protected-resource");
        assert_eq!(parsed.scope.as_deref(), Some("a b"));
        assert_eq!(parsed.error.as_deref(), Some("insufficient_scope"));
        assert_eq!(parse_www_authenticate_value("Basic realm=x"), WwwAuthenticate::default());
        assert_eq!(parse_www_authenticate_value("Bearer"), WwwAuthenticate::default());
    }

    #[test]
    fn discovery_urls() {
        let urls = build_discovery_urls(&Url::parse("https://auth.test/tenant/").unwrap());
        let urls: Vec<String> = urls.into_iter().map(|(u, _)| u.to_string()).collect();
        assert_eq!(
            urls,
            vec![
                "https://auth.test/.well-known/oauth-authorization-server/tenant",
                "https://auth.test/.well-known/openid-configuration/tenant",
                "https://auth.test/tenant/.well-known/openid-configuration"
            ]
        );
        let urls = build_discovery_urls(&Url::parse("https://auth.test").unwrap());
        assert_eq!(urls.len(), 2);
    }

    #[test]
    fn resource_checks() {
        let u = |s: &str| Url::parse(s).unwrap();
        assert!(check_resource_allowed(&u("https://a.test/mcp/x"), &u("https://a.test/mcp")));
        assert!(check_resource_allowed(&u("https://a.test/mcp"), &u("https://a.test/")));
        assert!(!check_resource_allowed(&u("https://a.test/mcpx"), &u("https://a.test/mcp")));
        assert!(!check_resource_allowed(&u("https://b.test/mcp"), &u("https://a.test/mcp")));
        assert_eq!(
            resource_url_from_server_url(&u("https://a.test/mcp?x=1#frag")).as_str(),
            "https://a.test/mcp?x=1"
        );
        assert!(issuers_match("https://a/", "https://a"));
        assert!(!issuers_match("https://a/x", "https://a"));
        assert_eq!(build_well_known_path("oauth-protected-resource", "/"), "/.well-known/oauth-protected-resource");
        assert_eq!(build_well_known_path("oauth-protected-resource", "/mcp/"), "/.well-known/oauth-protected-resource/mcp");
    }
}
