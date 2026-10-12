//! Public and planning types of the OpenAPI adapter. Port of `openapi/types.ts`.

use std::fmt;
use std::sync::Arc;

use futures::future::BoxFuture;
use serde_json::Value as Json;

/// The operation identity handed to auth resolution and errors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Operation {
    pub operation_id: Option<String>,
    /// Uppercase HTTP method.
    pub method: String,
    pub path: String,
    pub summary: Option<String>,
    pub description: Option<String>,
}

/// Where an `apiKey` security scheme carries its credential.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApiKeyLocation {
    Header,
    Query,
    Cookie,
}

impl ApiKeyLocation {
    pub fn as_str(&self) -> &'static str {
        match self {
            ApiKeyLocation::Header => "header",
            ApiKeyLocation::Query => "query",
            ApiKeyLocation::Cookie => "cookie",
        }
    }
}

/// A resolved OpenAPI security scheme from `components.securitySchemes`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SecurityScheme {
    ApiKey {
        name: String,
        location: ApiKeyLocation,
    },
    /// `scheme` is lowercased.
    Http {
        scheme: String,
    },
    OAuth2,
    OpenIdConnect,
}

/// Credential material returned by a host auth resolver. `ApiKey` uses the scheme's carrier; `Header` supports
/// nonstandard schemes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Credential {
    Bearer { token: String },
    Basic { username: String, password: String },
    ApiKey { value: String },
    Header { name: String, value: String },
}

/// What an auth resolver is asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthContext {
    /// The security scheme name.
    pub name: String,
    pub definition: SecurityScheme,
    pub scopes: Vec<String>,
    pub operation: Operation,
}

/// Resolves credentials at call time. `Ok(None)` tries the next OR alternative; `Err(message)` aborts the call
/// (the program sees a tool failure with that message).
pub trait AuthResolver: Send + Sync {
    fn resolve(&self, context: AuthContext) -> BoxFuture<'static, Result<Option<Credential>, String>>;
}

impl<F> AuthResolver for F
where
    F: Fn(AuthContext) -> BoxFuture<'static, Result<Option<Credential>, String>> + Send + Sync,
{
    fn resolve(&self, context: AuthContext) -> BoxFuture<'static, Result<Option<Credential>, String>> {
        self(context)
    }
}

/// `OpenAPI.fromSpec` options.
#[derive(Clone)]
pub struct Options {
    /// A parsed OpenAPI 3.x document (YAML must be parsed by the host).
    pub spec: Json,
    /// Overrides all document, path, and operation `servers`. Required when no applicable absolute server URL exists.
    pub base_url: Option<String>,
    /// Host credential resolution, keyed by security scheme name.
    pub auth: Option<Arc<dyn AuthResolver>>,
    /// Static headers on every request (a record: a repeated name replaces the earlier value). Not model-visible;
    /// declared header params may override them, auth always wins.
    pub headers: Vec<(String, String)>,
}

impl Options {
    pub fn new(spec: Json) -> Options {
        Options { spec, base_url: None, auth: None, headers: Vec::new() }
    }
    pub fn base_url(mut self, base_url: impl Into<String>) -> Options {
        self.base_url = Some(base_url.into());
        self
    }
    pub fn auth(mut self, auth: Arc<dyn AuthResolver>) -> Options {
        self.auth = Some(auth);
        self
    }
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Options {
        self.headers.push((name.into(), value.into()));
        self
    }
}

/// An operation that could not be represented as a tool, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skipped {
    pub method: String,
    pub path: String,
    pub reason: String,
}

/// An error `OpenAPI.fromSpec` throws synchronously upstream (`name` is the JS error class, e.g. `Error`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenApiError {
    pub name: String,
    pub message: String,
}

impl fmt::Display for OpenApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for OpenApiError {}

pub(crate) type Parsed<T> = Result<T, String>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputLocation {
    Path,
    Query,
    Header,
    Body,
}

impl InputLocation {
    pub fn as_str(&self) -> &'static str {
        match self {
            InputLocation::Path => "path",
            InputLocation::Query => "query",
            InputLocation::Header => "header",
            InputLocation::Body => "body",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParameterStyle {
    Simple,
    Form,
    DeepObject,
}

/// One model-visible input property and where it goes in the request.
#[derive(Clone, Debug, PartialEq)]
pub struct InputField {
    /// The property name in the tool input (prefixed/suffixed on collisions).
    pub input_name: String,
    /// The wire name.
    pub name: String,
    pub location: InputLocation,
    pub required: bool,
    pub schema: Json,
    pub style: Option<ParameterStyle>,
    pub explode: Option<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyMode {
    /// Flattened object properties, one input field each.
    Object,
    /// The whole body in a single `body` field.
    Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Body {
    pub required: bool,
    pub mode: BodyMode,
    pub media_type: String,
}

/// A security requirement: scheme names (in declaration order) with their scopes.
pub type SecurityRequirement = Vec<(String, Vec<String>)>;

/// Everything needed to execute one operation.
#[derive(Clone)]
pub struct Plan {
    pub operation: Operation,
    /// The base URL (trailing slashes removed) followed by the templated path.
    pub url: String,
    pub fields: Vec<InputField>,
    pub body: Option<Body>,
    pub security: Vec<SecurityRequirement>,
    pub schemes: Arc<Vec<(String, SecurityScheme)>>,
    pub auth: Option<Arc<dyn AuthResolver>>,
    /// Static headers, lowercased, in JS record order.
    pub headers: Vec<(String, String)>,
}

impl fmt::Debug for Plan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Plan")
            .field("operation", &self.operation)
            .field("url", &self.url)
            .field("fields", &self.fields)
            .field("body", &self.body)
            .field("security", &self.security)
            .field("schemes", &self.schemes)
            .field("auth", &self.auth.is_some())
            .field("headers", &self.headers)
            .finish()
    }
}
