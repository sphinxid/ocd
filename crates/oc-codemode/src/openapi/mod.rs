//! OpenAPI 3.x documents as tools. Port of `openapi/index.ts`.
//!
//! [`from_spec`] builds one tool per representable operation (dotted `operationId`s create namespaces) and reports
//! unrepresentable operations in [`OpenApi::skipped`]. Tools execute over a host [`HttpClient`]: call
//! [`OpenApi::call`] from your own [`Host`], or use [`OpenApiHost`]. Authentication stays host-side
//! ([`AuthResolver`]) and is never model-visible. Request signatures omit `readOnly` properties, response signatures
//! omit `writeOnly` properties; runtime values pass through unchanged.

mod json_schema;
mod runtime;
mod spec;
mod types;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use futures::future::BoxFuture;
use serde_json::{Map, Value as Json};

use crate::data::js_ordered;
use crate::host::{Host, HttpClient, Json as HostJson, ToolCallError};
use crate::tool_runtime::{ToolDef, ToolEntry};
use crate::tool_schema::ToolSchema;

pub use types::{
    ApiKeyLocation, AuthContext, AuthResolver, Body, BodyMode, Credential, InputField, InputLocation, OpenApiError,
    Operation, Options, ParameterStyle, Plan, SecurityRequirement, SecurityScheme, Skipped,
};

use spec::{
    Direction, METHODS, Spec, input_schema, is_record, non_empty_string, operation_path,
    operation_security_requirements, security_requirements, security_schemes, spec_server_url, validate_base_url,
};

/// The generated tools.
pub struct OpenApi {
    /// Tools in declaration order, with dotted paths relative to wherever the host mounts them.
    pub tools: Vec<ToolEntry>,
    pub skipped: Vec<Skipped>,
    plans: Vec<(String, Arc<Plan>)>,
    index: HashMap<String, usize>,
}

impl std::fmt::Debug for OpenApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenApi").field("tools", &self.tools).field("skipped", &self.skipped).finish()
    }
}

impl OpenApi {
    /// The execution plan of each tool, keyed by tool path, in declaration order.
    pub fn plans(&self) -> &[(String, Arc<Plan>)] {
        &self.plans
    }

    pub fn plan(&self, path: &str) -> Option<&Arc<Plan>> {
        self.index.get(path).map(|&i| &self.plans[i].1)
    }

    /// The tools with every path placed under `prefix` (e.g. `"api"` for `tools.api.*`).
    pub fn tools_under(&self, prefix: &str) -> Vec<ToolEntry> {
        self.tools
            .iter()
            .map(|entry| match entry {
                ToolEntry::Tool(def) => {
                    ToolEntry::Tool(ToolDef { path: format!("{prefix}.{}", def.path), ..def.clone() })
                }
                ToolEntry::Namespace { path, description } => {
                    ToolEntry::Namespace { path: format!("{prefix}.{path}"), description: description.clone() }
                }
            })
            .collect()
    }

    /// Executes the tool at `path` (relative to this API) with its decoded input.
    pub fn call(
        &self,
        client: Arc<dyn HttpClient>,
        path: &str,
        input: Option<Json>,
    ) -> BoxFuture<'static, Result<Option<Json>, ToolCallError>> {
        match self.plan(path) {
            Some(plan) => Box::pin(runtime::invoke(plan.clone(), client, input)),
            None => {
                let message = format!("Tool '{path}' is unavailable.");
                Box::pin(async move { Err(ToolCallError::Failure(message)) })
            }
        }
    }
}

/// A [`Host`] that executes OpenAPI tools over an [`HttpClient`]. With a `prefix`, the tools are expected under
/// that namespace (use [`OpenApiHost::tools`] for the matching tool entries); other paths fail as unavailable.
pub struct OpenApiHost {
    pub api: Arc<OpenApi>,
    pub client: Arc<dyn HttpClient>,
    pub prefix: Option<String>,
}

impl OpenApiHost {
    pub fn new(api: Arc<OpenApi>, client: Arc<dyn HttpClient>) -> OpenApiHost {
        OpenApiHost { api, client, prefix: None }
    }

    pub fn with_prefix(mut self, prefix: impl Into<String>) -> OpenApiHost {
        self.prefix = Some(prefix.into());
        self
    }

    /// The tool entries to hand to `CodeMode::new`, placed under the prefix if any.
    pub fn tools(&self) -> Vec<ToolEntry> {
        match &self.prefix {
            Some(prefix) => self.api.tools_under(prefix),
            None => self.api.tools.clone(),
        }
    }

    /// The API-relative path of a canonical tool path, if this host owns it.
    pub fn local_path<'p>(&self, path: &'p str) -> Option<&'p str> {
        match &self.prefix {
            None => Some(path),
            Some(prefix) => path.strip_prefix(prefix.as_str()).and_then(|rest| rest.strip_prefix('.')),
        }
    }
}

impl Host for OpenApiHost {
    fn call(&self, path: &str, input: Option<HostJson>) -> BoxFuture<'static, Result<Option<HostJson>, ToolCallError>> {
        match self.local_path(path) {
            Some(local) => self.api.call(self.client.clone(), local, input),
            None => {
                let message = format!("Tool '{path}' is unavailable.");
                Box::pin(async move { Err(ToolCallError::Failure(message)) })
            }
        }
    }
}

/// Nesting beyond this depth throws upstream (the recursive scans overflow the JS stack around this depth).
const MAX_SPEC_DEPTH: usize = 10_000;

fn depth_exceeds(value: &Json, limit: usize) -> bool {
    let mut stack: Vec<(&Json, usize)> = vec![(value, 1)];
    while let Some((value, depth)) = stack.pop() {
        if depth > limit {
            return true;
        }
        match value {
            Json::Array(items) => stack.extend(items.iter().map(|item| (item, depth + 1))),
            Json::Object(map) => stack.extend(map.values().map(|item| (item, depth + 1))),
            _ => {}
        }
    }
    false
}

/// JSON objects in JS property order (integer-like keys first), as the document would be after `JSON.parse`.
fn canonicalize(value: Json) -> Json {
    match value {
        Json::Object(map) => Json::Object(js_ordered(map.into_iter().map(|(k, v)| (k, canonicalize(v))).collect())),
        Json::Array(items) => Json::Array(items.into_iter().map(canonicalize).collect()),
        other => other,
    }
}

/// Builds one tool per representable OpenAPI 3.x operation (upstream `OpenAPI.fromSpec`). Errors are the ones
/// upstream throws synchronously (e.g. OpenAPI 3.0 schemas using keywords that Draft 2020-12 would activate).
pub fn from_spec(options: Options) -> Result<OpenApi, OpenApiError> {
    // Schema traversal recurses per nesting level; run it (and the drop of the document) on a generous stack.
    std::thread::scope(|scope| {
        let handle = std::thread::Builder::new()
            .name("openapi-from-spec".into())
            .stack_size(512 << 20)
            .spawn_scoped(scope, move || {
                if options.spec.is_null() {
                    return Err(OpenApiError {
                        name: "TypeError".into(),
                        message: "null is not an object (evaluating 'document.components')".into(),
                    });
                }
                if depth_exceeds(&options.spec, MAX_SPEC_DEPTH) {
                    return Err(OpenApiError {
                        name: "RangeError".into(),
                        message: "Maximum call stack size exceeded.".into(),
                    });
                }
                from_spec_impl(options)
            })
            .expect("spawn OpenAPI conversion thread");
        match handle.join() {
            Ok(result) => result,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    })
}

fn from_spec_impl(options: Options) -> Result<OpenApi, OpenApiError> {
    let document = canonicalize(options.spec);
    let spec = Spec::new(&document);
    let empty = Map::new();
    let root = is_record(Some(&document)).unwrap_or(&empty);
    let schemes = Arc::new(security_schemes(&spec));
    let default_security = security_requirements(root.get("security"));
    let request_definitions = spec.component_definitions(Direction::Request)?;
    let response_definitions = if spec.has_directional_schemas() {
        spec.component_definitions(Direction::Response)?
    } else {
        request_definitions.clone()
    };
    let headers = runtime::js_record(&options.headers);
    let mut static_headers = runtime::Headers::default();
    for (name, value) in &headers {
        static_headers.set(name, value);
    }
    let static_headers = static_headers.entries();

    let mut used: HashSet<String> = HashSet::new();
    let mut namespaces: HashSet<String> = HashSet::new();
    let mut skipped = Vec::new();
    let mut tools = Vec::new();
    let mut plans: Vec<(String, Arc<Plan>)> = Vec::new();

    let paths = is_record(root.get("paths")).unwrap_or(&empty);
    for (path, path_value) in paths {
        let Some(path_item) = is_record(Some(path_value)) else { continue };
        for (method, operation_value) in path_item {
            if !METHODS.contains(&method.as_str()) {
                continue;
            }
            let Some(operation_map) = is_record(Some(operation_value)) else { continue };
            let operation = Operation {
                operation_id: non_empty_string(operation_map.get("operationId")).map(String::from),
                method: method.to_uppercase(),
                path: path.clone(),
                summary: non_empty_string(operation_map.get("summary")).map(String::from),
                description: non_empty_string(operation_map.get("description")).map(String::from),
            };
            let skip = |reason: String| Skipped { method: operation.method.clone(), path: path.clone(), reason };
            let output = match spec.operation_output(operation_map, &response_definitions)? {
                Ok(output) => output,
                Err(reason) => {
                    skipped.push(skip(reason));
                    continue;
                }
            };
            let base_url = if let Some(base_url) = &options.base_url {
                validate_base_url(base_url)
            } else if operation_map.contains_key("servers") {
                spec_server_url(operation_map)
            } else if path_item.contains_key("servers") {
                spec_server_url(path_item)
            } else {
                spec_server_url(root)
            };
            let base_url = match base_url {
                Ok(url) => url,
                Err(reason) => {
                    skipped.push(skip(reason));
                    continue;
                }
            };
            let input = match spec.operation_input(path_item, operation_map)? {
                Ok(input) => input,
                Err(reason) => {
                    skipped.push(skip(reason));
                    continue;
                }
            };
            let security =
                match operation_security_requirements(operation_map.get("security"), &default_security, &schemes) {
                    Ok(security) => security,
                    Err(reason) => {
                        skipped.push(skip(reason));
                        continue;
                    }
                };
            let description = operation
                .description
                .clone()
                .or_else(|| operation.summary.clone())
                .unwrap_or_else(|| format!("{} {path}", operation.method));
            let segments = operation_path(method, path, operation_map, &used, &namespaces);
            let tool_path = segments.join(".");
            used.insert(tool_path.clone());
            for index in 0..segments.len().saturating_sub(1) {
                namespaces.insert(segments[..=index].join("."));
            }
            let schema = input_schema(&input.fields, &request_definitions);
            tools.push(ToolEntry::Tool(ToolDef {
                path: tool_path.clone(),
                description,
                input: ToolSchema::Json(schema),
                output: output.map(ToolSchema::Json),
            }));
            let plan = Plan {
                url: format!("{}{path}", base_url.trim_end_matches('/')),
                operation,
                fields: input.fields,
                body: input.body,
                security,
                schemes: schemes.clone(),
                auth: options.auth.clone(),
                headers: static_headers.clone(),
            };
            plans.push((tool_path, Arc::new(plan)));
        }
    }
    let index = plans.iter().enumerate().map(|(i, (path, _))| (path.clone(), i)).collect();
    Ok(OpenApi { tools, skipped, plans, index })
}
