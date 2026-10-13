//! AI SDK bridge for `aisdk:` packages (`core/src/aisdk.ts` `AISDK.Service`, a location node):
//! [`oc_ai::aisdk::AiSdkBridge`] over the oc-plugin sidecar. The sidecar is started on demand,
//! also for locations without plugins; the location's plugins' `aisdk.sdk` / `aisdk.language`
//! hooks and the internal provider plugins that register them (cohere, gitlab, perplexity,
//! sap-ai-core, vercel, dynamic) run there, gated like the other built-ins.

use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use futures::future::BoxFuture;
use oc_ai::aisdk::{AiSdkBridge, BridgeError, PartStream, StreamCall};
use oc_ai::error::TransportOperation;
use oc_ai::route::transport::{HttpHook, WireRequest, WireResponse};
use oc_plugin::{AisdkError, AisdkHttp, AisdkStreamRequest, LocationRef};
use serde_json::{Value, json};

use crate::plugins::Plugins;

/// The bridge of one location.
pub struct LocationBridge {
    plugins: Arc<Plugins>,
    key: String,
}

impl LocationBridge {
    pub fn new(plugins: Arc<Plugins>, key: impl Into<String>) -> Self {
        Self { plugins, key: key.into() }
    }

    fn location(&self) -> LocationRef {
        LocationRef::new(self.key.clone())
    }
}

fn operation(name: &str) -> TransportOperation {
    if name == "request" { TransportOperation::Request } else { TransportOperation::Read }
}

fn bridge_error(error: AisdkError) -> BridgeError {
    match error {
        AisdkError::Provider { operation: op, shape } => BridgeError::Provider { operation: operation(&op), shape },
        // A model that failed to load at stream time (e.g. after a sidecar restart).
        AisdkError::Init { detail } => BridgeError::Provider {
            operation: TransportOperation::Request,
            shape: json!({ "kind": "unknown", "message": detail.unwrap_or_else(|| "Provider request failed".into()) }),
        },
        AisdkError::Host(message) => BridgeError::Host(message),
    }
}

impl AiSdkBridge for LocationBridge {
    fn load(&self, model: Value) -> BoxFuture<'static, Result<(), Option<String>>> {
        let plugins = self.plugins.clone();
        let location = self.location();
        let enabled = plugins.enabled(&self.key);
        Box::pin(async move {
            let host = plugins.host_or_create().clone();
            host.aisdk_load(&location, model, enabled).await.map_err(|error| match error {
                AisdkError::Init { detail } => detail,
                AisdkError::Provider { shape, .. } => shape.get("message").and_then(Value::as_str).map(String::from),
                AisdkError::Host(message) => Some(message),
            })
        })
    }

    fn stream(&self, call: StreamCall) -> BoxFuture<'static, Result<PartStream, BridgeError>> {
        let plugins = self.plugins.clone();
        let location = self.location();
        let enabled = plugins.enabled(&self.key);
        Box::pin(async move {
            let host = plugins.host_or_create().clone();
            let http = call.hook.map(|hook| Arc::new(HookBridge(hook)) as Arc<dyn AisdkHttp>);
            let parts = host
                .aisdk_stream(AisdkStreamRequest { location, model: call.model, options: call.options, http, enabled })
                .await
                .map_err(bridge_error)?;
            let parts: PartStream = Box::pin(parts.map(|item| item.map_err(bridge_error)));
            Ok(parts)
        })
    }
}

/// `session.http.*` hooks (oc-ai [`HttpHook`]) seen from the sidecar's fetch middleware.
struct HookBridge(Arc<dyn HttpHook>);

fn headers(value: &Value) -> indexmap::IndexMap<String, String> {
    value
        .as_object()
        .map(|o| o.iter().map(|(k, v)| (k.to_ascii_lowercase(), v.as_str().map(String::from).unwrap_or_else(|| v.to_string()))).collect())
        .unwrap_or_default()
}

fn wire_request(value: &Value) -> WireRequest {
    WireRequest {
        url: value.get("url").and_then(Value::as_str).unwrap_or_default().to_string(),
        method: value.get("method").and_then(Value::as_str).unwrap_or("GET").to_string(),
        headers: headers(value.get("headers").unwrap_or(&Value::Null)),
        body: value.get("body").and_then(Value::as_str).map(String::from),
    }
}

fn request_json(request: &WireRequest) -> Value {
    json!({ "url": request.url, "method": request.method, "headers": request.headers, "body": request.body })
}

#[async_trait]
impl AisdkHttp for HookBridge {
    async fn request(&self, request: Value) -> Result<Value, String> {
        let out = self.0.request(wire_request(&request)).await?;
        Ok(request_json(&out))
    }

    async fn response(&self, request: Value, response: Value) -> Result<Value, String> {
        let response = WireResponse {
            status: response.get("status").and_then(Value::as_u64).unwrap_or(200) as u16,
            status_text: response.get("statusText").and_then(Value::as_str).unwrap_or_default().to_string(),
            headers: headers(response.get("headers").unwrap_or(&Value::Null)),
            body: response.get("body").and_then(Value::as_str).map(String::from),
        };
        let out = self.0.response(&wire_request(&request), response).await?;
        Ok(json!({ "status": out.status, "statusText": out.status_text, "headers": out.headers, "body": out.body }))
    }

    fn wants_response(&self) -> bool {
        self.0.wants_response()
    }
}
