//! The [`oc_plugin::PluginApi`] of the server: `ctx.*` API calls of plugins are served by the very
//! same HTTP handlers (an in-process request through the router, so validation, errors and
//! response shapes are the endpoints'), host-service methods by the core.

use std::collections::HashSet;
use std::sync::{Arc, LazyLock, Weak};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::Request;
use oc_plugin::{ApiError, ApiRequest, LocationRef, PluginApi, StorageScan};
use serde_json::{Map, Value, json};
use tower::ServiceExt;

use crate::engine::Engine;

/// Upstream `ClientApi` endpoint identifiers that differ from the OpenAPI operation id.
fn operation_id(method: &str) -> &str {
    match method {
        "persistentPty.read" => "server.experimental.persistentPty.read",
        "generate.text" => "experimental.generate.text",
        "session.wait" => "experimental.session.wait",
        other => other,
    }
}

/// Query parameter names per operation id (from the embedded OpenAPI document).
static QUERY: LazyLock<std::collections::HashMap<String, HashSet<String>>> = LazyLock::new(|| {
    let doc: Value = serde_json::from_str(oc_server::OPENAPI_JSON).unwrap_or_default();
    let mut out = std::collections::HashMap::new();
    for (_, item) in doc.get("paths").and_then(Value::as_object).cloned().unwrap_or_default() {
        for (_, op) in item.as_object().cloned().unwrap_or_default() {
            let Some(id) = op.get("operationId").and_then(Value::as_str) else { continue };
            let names: HashSet<String> = op
                .get("parameters")
                .and_then(Value::as_array)
                .map(|ps| {
                    ps.iter()
                        .filter(|p| p.get("in").and_then(Value::as_str) == Some("query"))
                        .filter_map(|p| p.get("name").and_then(Value::as_str).map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            out.insert(id.to_string(), names);
        }
    }
    out
});

fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn scalar(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn push_query(out: &mut Vec<String>, name: &str, value: &Value) {
    match value {
        Value::Null => {}
        Value::Object(o) => {
            for (k, v) in o {
                push_query(out, &format!("{name}[{k}]"), v);
            }
        }
        Value::Array(a) => {
            for v in a {
                push_query(out, name, v);
            }
        }
        v => out.push(format!("{}={}", encode(name), encode(&scalar(v)))),
    }
}

pub struct BackendApi {
    engine: Weak<Engine>,
    router: axum::Router,
    core: oc_core::plugins::CoreApi,
}

impl BackendApi {
    pub fn new(engine: &Arc<Engine>, router: axum::Router) -> Self {
        BackendApi { engine: Arc::downgrade(engine), router, core: oc_core::plugins::CoreApi::new(Arc::downgrade(&engine.app.plugins)) }
    }

    fn engine(&self) -> Result<Arc<Engine>, ApiError> {
        self.engine.upgrade().ok_or_else(|| ApiError::new("server is shutting down"))
    }

    async fn http(&self, request: &ApiRequest) -> Result<Value, ApiError> {
        let id = operation_id(&request.method);
        let meta = oc_server::OPS.iter().find(|o| o.id == id).ok_or_else(|| ApiError::unsupported(&request.method))?;
        let mut input: Map<String, Value> = request.input.as_object().cloned().unwrap_or_default();
        // `session.create` without a parent defaults to the calling location (plugin/host.ts).
        if id == "session.create" && !input.contains_key("parentID") && !input.contains_key("location") {
            input.insert("location".into(), json!({ "directory": request.location.directory }));
        }
        let mut path = meta.path.to_string();
        while let Some(start) = path.find('{') {
            let end = path[start..].find('}').map(|e| start + e).unwrap_or(path.len());
            let name = path[start + 1..end].to_string();
            let value = input.remove(&name).map(|v| scalar(&v)).unwrap_or_default();
            path.replace_range(start..=end, &encode(&value));
        }
        let query_names = QUERY.get(id).cloned().unwrap_or_default();
        let mut query = Vec::new();
        let mut body = Map::new();
        for (k, v) in input {
            if query_names.contains(&k) {
                push_query(&mut query, &k, &v);
            } else {
                body.insert(k, v);
            }
        }
        let uri = if query.is_empty() { path } else { format!("{path}?{}", query.join("&")) };
        let mut builder = Request::builder().method(meta.method).uri(uri).header(oc_server::location::DIRECTORY_HEADER, encode(&request.location.directory));
        let has_body = matches!(meta.method, "POST" | "PUT" | "PATCH");
        let body = if has_body {
            builder = builder.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&Value::Object(body)).unwrap_or_default())
        } else {
            Body::empty()
        };
        let request = builder.body(body).map_err(|e| ApiError::new(e.to_string()))?;
        let response = self.router.clone().oneshot(request).await.map_err(|e| ApiError::new(e.to_string()))?;
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.map_err(|e| ApiError::new(e.to_string()))?;
        let value: Value = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap_or(Value::Null) };
        if !status.is_success() {
            let message = value.get("message").and_then(Value::as_str).map(String::from).unwrap_or_else(|| format!("Request failed with status {}", status.as_u16()));
            let mut error = ApiError::new(message);
            error.name = value.get("_tag").and_then(Value::as_str).map(String::from);
            error.data = Some(value);
            return Err(error);
        }
        Ok(match meta.envelope {
            oc_server::Envelope::Data => value.get("data").cloned().unwrap_or(Value::Null),
            oc_server::Envelope::NoContent => Value::Null,
            _ => value,
        })
    }

    async fn integration_connection(&self, request: &ApiRequest) -> Result<Value, ApiError> {
        let engine = self.engine()?;
        // The calling plugin's location registry (upstream binds `ctx.integration` per location).
        let integrations = engine.models().integrations_for(Some(&request.location.directory));
        let input = &request.input;
        let connection = |v: Option<&Value>| -> Result<oc_ai::credentials::integration::ConnectionInfo, ApiError> {
            serde_json::from_value(v.cloned().unwrap_or(Value::Null)).map_err(|e| ApiError::new(e.to_string()))
        };
        match request.method.as_str() {
            "integration.connection.active" => {
                let id = input.get("integrationID").and_then(Value::as_str).unwrap_or_default();
                let active = integrations.active(id).await.map_err(|e| ApiError::new(e.to_string()))?;
                Ok(active.map(|c| c.to_json()).unwrap_or(Value::Null))
            }
            "integration.connection.resolve" => {
                let value = integrations.resolve(&connection(input.get("connection"))?).await.map_err(|e| ApiError::new(e.to_string()))?;
                Ok(value.map(|v| serde_json::to_value(v).unwrap_or(Value::Null)).unwrap_or(Value::Null))
            }
            _ => {
                let id = input.get("integrationID").and_then(Value::as_str).unwrap_or_default();
                let status = input.get("status").cloned().filter(|s| !s.is_null()).and_then(|s| serde_json::from_value(s).ok());
                integrations.set_status(id, &connection(input.get("connection"))?, status);
                Ok(Value::Null)
            }
        }
    }
}

#[async_trait]
impl PluginApi for BackendApi {
    async fn call(&self, request: ApiRequest) -> Result<Value, ApiError> {
        if request.method.starts_with("integration.connection.") {
            return self.integration_connection(&request).await;
        }
        if let Some(engine) = self.engine.upgrade()
            && let Some(result) = engine.app.plugins.core_call(&request).await
        {
            return result;
        }
        self.http(&request).await
    }

    async fn invoke(&self, location: &LocationRef, handle: &str, args: Vec<Value>) -> Result<Value, ApiError> {
        if let Some(name) = handle.strip_prefix("rs:command.") {
            let engine = self.engine()?;
            let invocation = args.into_iter().next().unwrap_or(Value::Null);
            engine.native_command(&location.directory, name, invocation).await.map_err(|e| ApiError::new(e.to_string()))?;
            return Ok(Value::Null);
        }
        self.core.invoke(location, handle, args).await
    }

    async fn storage_get(&self, key: &str) -> Result<Option<Value>, ApiError> {
        self.core.storage_get(key).await
    }

    async fn storage_set(&self, key: &str, value: Value) -> Result<(), ApiError> {
        self.core.storage_set(key, value).await
    }

    async fn storage_remove(&self, key: &str) -> Result<(), ApiError> {
        self.core.storage_remove(key).await
    }

    async fn storage_scan(&self, prefix: &str, after: Option<&str>, limit: Option<u64>) -> Result<StorageScan, ApiError> {
        self.core.storage_scan(prefix, after, limit).await
    }
}

/// Installs the backend's plugin API on the app (before any location loads plugins).
pub fn install(engine: &Arc<Engine>, opts: &oc_server::ServerOptions) {
    let inner = oc_server::ServerOptions {
        password: None,
        ready: true,
        cwd: opts.cwd.clone(),
        version: opts.version.clone(),
        tmp_dir: opts.tmp_dir.clone(),
        web_ui: None,
        ..Default::default()
    };
    match oc_server::router(engine.clone() as Arc<dyn oc_server::Backend>, &inner) {
        Ok((router, _)) => engine.app.plugins.set_api(Arc::new(BackendApi::new(engine, router))),
        Err(error) => tracing::error!(%error, "plugin API router unavailable"),
    }
}
