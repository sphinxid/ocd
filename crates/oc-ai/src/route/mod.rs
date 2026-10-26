//! Routes: protocol × endpoint × auth × framing (`packages/ai/src/route/*`).

pub mod auth;
pub mod framing;
pub mod transport;
pub mod websocket;

use crate::error::ProviderConfigurationError;
use crate::protocols::Protocol;
use crate::types::{Compatibility, GenerationOptions, HttpOptions, LLMRequest, ModelDefaults, ProviderOptions};
use indexmap::IndexMap;
use serde_json::Value;
use std::fmt;
use std::sync::Arc;

pub use auth::{Auth, AuthInput, Credential};
pub use framing::Framing;

/// Lower-cased header map in insertion order.
pub type Headers = IndexMap<String, String>;

/// Lower-case all keys (later duplicates win, keeping the first position).
pub fn normalize_headers(input: &IndexMap<String, String>) -> Headers {
    let mut out = Headers::new();
    for (k, v) in input {
        out.insert(k.to_lowercase(), v.clone());
    }
    out
}

pub type PathFn = Arc<dyn Fn(&LLMRequest, &Value) -> String + Send + Sync>;
pub type HeadersFn = Arc<dyn Fn(&LLMRequest) -> Headers + Send + Sync>;
/// Transport hook run before header assembly with access to the protocol body (e.g. Anthropic betas).
pub type PrepareHeadersFn = Arc<dyn Fn(&LLMRequest, &Value, &mut Headers) + Send + Sync>;

#[derive(Clone)]
pub enum EndpointPath {
    Static(String),
    Dynamic(PathFn),
}

impl fmt::Debug for EndpointPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Static(s) => write!(f, "Static({s:?})"),
            Self::Dynamic(_) => f.write_str("Dynamic(..)"),
        }
    }
}

/// `Endpoint.Definition`.
#[derive(Clone, Debug)]
pub struct Endpoint {
    pub base_url: Option<String>,
    pub path: EndpointPath,
    pub query: IndexMap<String, String>,
}

impl Endpoint {
    pub fn new(base_url: Option<&str>, path: &str) -> Self {
        Self { base_url: base_url.map(String::from), path: EndpointPath::Static(path.into()), query: IndexMap::new() }
    }
    pub fn dynamic(base_url: Option<&str>, path: impl Fn(&LLMRequest, &Value) -> String + Send + Sync + 'static) -> Self {
        Self { base_url: base_url.map(String::from), path: EndpointPath::Dynamic(Arc::new(path)), query: IndexMap::new() }
    }

    /// `render`: `trimTrailingSlashes(baseURL) + path`, then static query entries are `set`.
    pub fn render(&self, request: &LLMRequest, body: &Value) -> Result<url::Url, String> {
        let base = self.base_url.as_deref().unwrap_or("").trim_end_matches('/');
        let path = match &self.path {
            EndpointPath::Static(p) => p.clone(),
            EndpointPath::Dynamic(f) => f(request, body),
        };
        let mut url = url::Url::parse(&format!("{base}{path}")).map_err(|e| format!("Invalid URL: {e}"))?;
        set_query(&mut url, &self.query);
        Ok(url)
    }
}

/// `URLSearchParams.set` for each entry.
pub fn set_query(url: &mut url::Url, query: &IndexMap<String, String>) {
    if query.is_empty() {
        return;
    }
    let mut pairs: Vec<(String, String)> = url.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
    for (key, value) in query {
        if let Some(pos) = pairs.iter().position(|(k, _)| k == key) {
            pairs[pos].1 = value.clone();
            let mut i = pos + 1;
            while i < pairs.len() {
                if pairs[i].0 == *key {
                    pairs.remove(i);
                } else {
                    i += 1;
                }
            }
        } else {
            pairs.push((key.clone(), value.clone()));
        }
    }
    // URLSearchParams serializes with application/x-www-form-urlencoded rules.
    let encoded: String = url::form_urlencoded::Serializer::new(String::new()).extend_pairs(pairs.iter()).finish();
    url.set_query(Some(&encoded));
}

/// A route that streams without the HTTP transport (upstream `route.streamPrepared` overrides, e.g.
/// the AI SDK bridge): receives the compiled request and its protocol body.
pub trait PreparedStream: Send + Sync {
    fn stream(
        &self,
        request: LLMRequest,
        body: Value,
        hook: Option<Arc<dyn transport::HttpHook>>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> crate::client::LLMStream;
    fn as_any(&self) -> &dyn std::any::Any;
}

/// A runnable route.
#[derive(Clone)]
pub struct Route {
    pub id: String,
    pub provider: Option<String>,
    /// ProviderMetadata namespace emitted and consumed by this route.
    pub provider_metadata_key: Option<String>,
    pub protocol: Arc<dyn Protocol>,
    pub endpoint: Endpoint,
    pub auth: Arc<dyn Auth>,
    pub framing: Framing,
    /// Deployment headers resolved for every request, before transport auth.
    pub headers: Option<HeadersFn>,
    /// Transport-level header hook with access to the body.
    pub prepare_headers: Option<PrepareHeadersFn>,
    pub defaults: ModelDefaults,
    /// Non-HTTP execution (`streamPrepared`); `None` = the HTTP transport.
    pub stream: Option<Arc<dyn PreparedStream>>,
    /// The Open Responses WebSocket channel (`OpenResponsesChannel.transport`): used when the caller
    /// passes a channel executor (`StreamOptions.webSocket`); `None` = HTTP only.
    pub channel: Option<Arc<crate::protocols::open_responses_channel::ChannelOptions>>,
    /// `Gemini.transport`: SSE that continues responses Gemini splits across requests
    /// (`continuationToken` checkpoints, `CONTINUATION` finishes, resumed dropped connections).
    pub continuation: bool,
}

impl fmt::Debug for Route {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Route")
            .field("id", &self.id)
            .field("provider", &self.provider)
            .field("provider_metadata_key", &self.provider_metadata_key)
            .field("protocol", &self.protocol.id())
            .field("endpoint", &self.endpoint)
            .finish()
    }
}

impl Route {
    pub fn new(id: &str, protocol: Arc<dyn Protocol>, endpoint: Endpoint, framing: Framing) -> Self {
        Self {
            id: id.into(),
            provider: None,
            provider_metadata_key: None,
            protocol,
            endpoint,
            auth: auth::none(),
            framing,
            headers: None,
            prepare_headers: None,
            defaults: ModelDefaults::default(),
            stream: None,
            channel: None,
            continuation: false,
        }
    }

    pub fn id(mut self, id: &str) -> Self {
        self.id = id.into();
        self
    }

    /// Set the provider; a provider change re-keys the metadata namespace (`route.with({provider})`).
    pub fn provider(mut self, provider: &str) -> Self {
        if self.provider.as_deref() != Some(provider) {
            self.provider_metadata_key = Some(provider.into());
        }
        self.provider = Some(provider.into());
        self
    }

    /// Set the provider without re-keying the metadata namespace (route construction).
    pub fn initial_provider(mut self, provider: &str, metadata_key: Option<&str>) -> Self {
        self.provider = Some(provider.into());
        self.provider_metadata_key = metadata_key.map(String::from);
        self
    }

    pub fn metadata_key(mut self, key: &str) -> Self {
        self.provider_metadata_key = Some(key.into());
        self
    }

    pub fn auth(mut self, auth: Arc<dyn Auth>) -> Self {
        self.auth = auth;
        self
    }

    pub fn base_url(mut self, base_url: Option<String>) -> Self {
        if base_url.is_some() {
            self.endpoint.base_url = base_url;
        }
        self
    }

    pub fn path(mut self, path: EndpointPath) -> Self {
        self.endpoint.path = path;
        self
    }

    pub fn query(mut self, query: IndexMap<String, String>) -> Self {
        for (k, v) in query {
            self.endpoint.query.insert(k, v);
        }
        self
    }

    /// Set (or clear) the WebSocket channel transport.
    pub fn channel(mut self, channel: Option<crate::protocols::open_responses_channel::ChannelOptions>) -> Self {
        self.channel = channel.map(Arc::new);
        self
    }

    /// Use the Gemini continuation transport.
    pub fn continuation(mut self) -> Self {
        self.continuation = true;
        self
    }

    pub fn framing(mut self, framing: Framing) -> Self {
        self.framing = framing;
        self
    }

    pub fn headers_fn(mut self, f: impl Fn(&LLMRequest) -> Headers + Send + Sync + 'static) -> Self {
        self.headers = Some(Arc::new(f));
        self
    }

    pub fn prepare_headers(mut self, f: impl Fn(&LLMRequest, &Value, &mut Headers) + Send + Sync + 'static) -> Self {
        self.prepare_headers = Some(Arc::new(f));
        self
    }

    /// `mergeRouteDefaults`: headers are folded into `defaults.http.headers`.
    pub fn defaults(mut self, patch: RouteDefaults) -> Self {
        let generation = GenerationOptions::merge(&[self.defaults.generation.as_ref(), patch.generation.as_ref()]);
        let provider_options =
            crate::json::merge_records(&[self.defaults.provider_options.as_ref(), patch.provider_options.as_ref()]);
        let header_http = patch.headers.as_ref().filter(|h| !h.is_empty()).map(|h| HttpOptions::headers(normalize_headers(h)));
        let http = HttpOptions::merge(&[self.defaults.http.as_ref(), patch.http.as_ref(), header_http.as_ref()]);
        self.defaults = ModelDefaults { generation, provider_options, http };
        self
    }

    pub fn with_headers(self, headers: IndexMap<String, String>) -> Self {
        self.defaults(RouteDefaults { headers: Some(headers), ..Default::default() })
    }

    pub fn with_provider_options(self, provider_options: ProviderOptions) -> Self {
        self.defaults(RouteDefaults { provider_options: Some(provider_options), ..Default::default() })
    }

    pub fn with_http(self, http: HttpOptions) -> Self {
        self.defaults(RouteDefaults { http: Some(http), ..Default::default() })
    }

    /// `Route.model(...)`.
    pub fn model(self, id: &str) -> Result<LanguageModel, ProviderConfigurationError> {
        let provider = self.provider.clone().unwrap_or_default();
        if self.endpoint.base_url.as_deref().map(|b| b.is_empty()).unwrap_or(true) {
            return Err(ProviderConfigurationError::new(
                provider,
                format!("Route.model({}) requires an endpoint baseURL — configure it on the route first", self.id),
            ));
        }
        Ok(LanguageModel { id: id.into(), provider, route: Arc::new(self), defaults: None, compatibility: None })
    }
}

/// Patch for route defaults.
#[derive(Debug, Clone, Default)]
pub struct RouteDefaults {
    pub headers: Option<IndexMap<String, String>>,
    pub generation: Option<GenerationOptions>,
    pub provider_options: Option<ProviderOptions>,
    pub http: Option<HttpOptions>,
}

/// `LanguageModel`: model id, provider, route and per-model defaults/compatibility.
#[derive(Clone, Debug)]
pub struct LanguageModel {
    pub id: String,
    pub provider: String,
    pub route: Arc<Route>,
    pub defaults: Option<ModelDefaults>,
    pub compatibility: Option<Compatibility>,
}

impl LanguageModel {
    pub fn with_compatibility(mut self, compatibility: Compatibility) -> Self {
        self.compatibility = Some(match &self.compatibility {
            Some(existing) => existing.merged(&compatibility),
            None => compatibility,
        });
        self
    }
    pub fn with_defaults(mut self, defaults: ModelDefaults) -> Self {
        self.defaults = Some(defaults);
        self
    }
    /// Replace the route (e.g. to apply `Route::with...` patches after construction).
    pub fn map_route(mut self, f: impl FnOnce(Route) -> Route) -> Self {
        let route = (*self.route).clone();
        self.route = Arc::new(f(route));
        self
    }
    pub fn with_provider(mut self, provider: &str) -> Self {
        self.provider = provider.into();
        self
    }
}
