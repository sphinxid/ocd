//! Executes one planned operation over a host `HttpClient`. Port of `openapi/runtime.ts` (plus the parts of Effect's
//! `HttpClientRequest`/`Headers`/`UrlParams` semantics it relies on).

use std::sync::Arc;

use base64::Engine;
use serde_json::{Map, Value as Json};

use super::types::{
    ApiKeyLocation, BodyMode, Credential, InputField, InputLocation, ParameterStyle, Plan, SecurityScheme,
};
use crate::data::{js_ordered, stringify};
use crate::host::{HttpClient, HttpRequest, HttpResponse, ToolCallError};

const MAX_ERROR_BODY_CHARS: usize = 1_024;
const MAX_RESPONSE_BODY_BYTES: usize = 50 * 1024 * 1024;

fn tool_error(message: impl Into<String>) -> ToolCallError {
    ToolCallError::Failure(message.into())
}

/// `toolError(message, cause)` as the tool runtime reports it: `Formatter.format` of an error with a cause.
fn tool_error_with_cause(message: &str, cause: &str) -> ToolCallError {
    ToolCallError::Failure(format!("ToolError: {message} (cause: {cause})"))
}

fn is_array_index(key: &str) -> bool {
    crate::object::parse_array_index(&crate::value::JsStr::from(key)).is_some()
}

/// Effect `Headers`: a JS record with lowercased names (integer-like names first; `__proto__` cannot be set).
#[derive(Default, Clone, Debug)]
pub(crate) struct Headers(Vec<(String, String)>);

impl Headers {
    pub fn set(&mut self, name: &str, value: &str) {
        let name = name.to_lowercase();
        if name == "__proto__" {
            return;
        }
        match self.0.iter_mut().find(|(k, _)| *k == name) {
            Some(entry) => entry.1 = value.to_string(),
            None => self.0.push((name, value.to_string())),
        }
    }
    /// `Headers.setAll(self, record)` for a record given in JS order.
    pub fn set_all(&mut self, record: &[(String, String)]) {
        let mut normalized = Headers::default();
        for (name, value) in record {
            normalized.set(name, value);
        }
        for (name, value) in normalized.entries() {
            self.set(&name, &value);
        }
    }
    pub fn entries(&self) -> Vec<(String, String)> {
        let mut ints: Vec<(u64, (String, String))> = Vec::new();
        let mut rest = Vec::new();
        for (k, v) in &self.0 {
            if is_array_index(k) {
                ints.push((k.parse().unwrap_or(0), (k.clone(), v.clone())));
            } else {
                rest.push((k.clone(), v.clone()));
            }
        }
        ints.sort_by_key(|(i, _)| *i);
        ints.into_iter().map(|(_, e)| e).chain(rest).collect()
    }
}

/// A JS record from ordered entries (later duplicates replace earlier values in place; integer-like keys first).
pub(crate) fn js_record(entries: &[(String, String)]) -> Vec<(String, String)> {
    js_ordered(entries.iter().map(|(k, v)| (k.clone(), Json::String(v.clone()))).collect())
        .into_iter()
        .map(|(k, v)| (k, v.as_str().unwrap_or("").to_string()))
        .collect()
}

/// `String(value)` for JSON scalars.
fn js_string(value: &Json) -> String {
    match value {
        Json::Null => "null".into(),
        Json::Bool(b) => b.to_string(),
        Json::Number(n) => crate::number::to_string(n.as_f64().unwrap_or(0.0)),
        Json::String(s) => s.clone(),
        Json::Array(_) | Json::Object(_) => stringify_object(value),
    }
}

fn stringify_object(value: &Json) -> String {
    match value {
        Json::Array(items) => items
            .iter()
            .map(|item| match item {
                Json::Null => String::new(),
                other => js_string(other),
            })
            .collect::<Vec<_>>()
            .join(","),
        _ => "[object Object]".into(),
    }
}

fn is_scalar(value: &Json) -> bool {
    !matches!(value, Json::Array(_) | Json::Object(_))
}

/// `encodeURIComponent` plus escaping of `!'()*`.
fn encode_path_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
            out.push(c);
        } else {
            let mut buf = [0u8; 4];
            for b in c.encode_utf8(&mut buf).bytes() {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    out
}

fn unsupported_nested(field: &InputField) -> ToolCallError {
    tool_error(format!("Parameter '{}' contains an unsupported nested value.", field.input_name))
}

fn serialize_simple(
    field: &InputField,
    value: &Json,
    encode: &dyn Fn(&str) -> String,
) -> Result<String, ToolCallError> {
    let scalar = |item: &Json| -> Result<String, ToolCallError> {
        if is_scalar(item) { Ok(encode(&js_string(item))) } else { Err(unsupported_nested(field)) }
    };
    match value {
        Json::Array(items) => {
            let rendered: Vec<Result<String, ToolCallError>> = items.iter().map(scalar).collect();
            let mut parts = Vec::with_capacity(rendered.len());
            for item in rendered {
                parts.push(item?);
            }
            Ok(parts.join(","))
        }
        Json::Object(entries) => {
            let mut parts = Vec::new();
            for (name, item) in entries {
                let rendered = scalar(item)?;
                if field.explode == Some(true) {
                    parts.push(format!("{}={rendered}", encode(name)));
                } else {
                    parts.push(encode(name));
                    parts.push(rendered);
                }
            }
            Ok(parts.join(","))
        }
        other => scalar(other),
    }
}

fn serialize_query(field: &InputField, value: &Json) -> Result<Vec<(String, String)>, ToolCallError> {
    let identity = |s: &str| s.to_string();
    if field.style == Some(ParameterStyle::DeepObject) {
        let Json::Object(entries) = value else {
            return Err(tool_error(format!("Deep-object parameter '{}' must be an object.", field.input_name)));
        };
        let mut parameters = Vec::new();
        for (name, item) in entries {
            if !is_scalar(item) {
                return Err(tool_error(format!(
                    "Deep-object parameter '{}' contains an unsupported nested value.",
                    field.input_name
                )));
            }
            parameters.push((format!("{}[{name}]", field.name), js_string(item)));
        }
        return Ok(parameters);
    }
    if let Json::Array(items) = value {
        if field.explode != Some(true) {
            return Ok(vec![(field.name.clone(), serialize_simple(field, value, &identity)?)]);
        }
        let mut parameters = Vec::new();
        for item in items {
            if !is_scalar(item) {
                return Err(unsupported_nested(field));
            }
            parameters.push((field.name.clone(), js_string(item)));
        }
        return Ok(parameters);
    }
    if let Json::Object(entries) = value
        && field.explode == Some(true)
    {
        let mut parameters = Vec::new();
        for (name, item) in entries {
            if !is_scalar(item) {
                return Err(tool_error(format!(
                    "Query parameter '{}' contains an unsupported nested value.",
                    field.input_name
                )));
            }
            parameters.push((name.clone(), js_string(item)));
        }
        return Ok(parameters);
    }
    Ok(vec![(field.name.clone(), serialize_simple(field, value, &identity)?)])
}

fn find_unresolved(url: &str) -> Option<String> {
    let chars: Vec<char> = url.chars().collect();
    let mut start: Option<usize> = None;
    for (i, c) in chars.iter().enumerate() {
        match c {
            '{' => start = Some(i),
            '}' => {
                if let Some(s) = start
                    && i > s + 1
                {
                    return Some(chars[s..=i].iter().collect());
                }
                start = None;
            }
            _ => {}
        }
    }
    None
}

fn build_url(plan: &Plan, input: &Map<String, Json>) -> Result<String, ToolCallError> {
    let mut url = plan.url.clone();
    for field in plan.fields.iter().filter(|f| f.location == InputLocation::Path) {
        let Some(item) = input.get(&field.input_name) else {
            return Err(tool_error(format!("Missing required path parameter '{}'.", field.input_name)));
        };
        let value = serialize_simple(field, item, &encode_path_value)?;
        // URL normalization collapses encoded `.` and `..`, which could retarget the request.
        if value.is_empty() || value == "." || value == ".." {
            return Err(tool_error(format!("Invalid path parameter '{}'.", field.input_name)));
        }
        url = url.replace(&format!("{{{}}}", field.name), &value);
    }
    if let Some(unresolved) = find_unresolved(&url) {
        return Err(tool_error(format!("Unresolved path parameter {unresolved}.")));
    }
    Ok(url)
}

/// The request as Effect's `HttpClientRequest` holds it.
struct Request {
    url: String,
    params: Vec<(String, String)>,
    headers: Headers,
    body: Option<Vec<u8>>,
}

impl Request {
    fn set_body_json(&mut self, value: &Json, media_type: &str) {
        let bytes = stringify(value, "").into_bytes();
        self.headers.set("content-type", "application/json");
        self.headers.set("content-length", &bytes.len().to_string());
        self.body = Some(bytes);
        self.headers.set("content-type", media_type);
    }

    /// `HttpClientRequest.toUrl`: `None` when the URL is invalid.
    fn to_url(&self) -> Option<String> {
        let mut url = url::Url::parse(&self.url).ok()?;
        if !self.params.is_empty() {
            let mut serializer = url::form_urlencoded::Serializer::new(String::new());
            if let Some(existing) = url.query() {
                for (k, v) in url::form_urlencoded::parse(existing.as_bytes()) {
                    serializer.append_pair(&k, &v);
                }
            }
            for (k, v) in &self.params {
                serializer.append_pair(k, v);
            }
            url.set_query(Some(&serializer.finish()));
        }
        Some(url.to_string())
    }
}

fn build_request(plan: &Plan, input: &Map<String, Json>) -> Result<Request, ToolCallError> {
    // Validate model input before auth resolution can refresh credentials.
    let url = build_url(plan, input)?;
    if let Some(missing) = plan
        .fields
        .iter()
        .find(|f| f.required && f.location != InputLocation::Path && !input.contains_key(&f.input_name))
    {
        let label = if missing.location == InputLocation::Body {
            "body field".to_string()
        } else {
            format!("{} parameter", missing.location.as_str())
        };
        return Err(tool_error(format!("Missing required {label} '{}'.", missing.input_name)));
    }
    let mut request = Request { url, params: Vec::new(), headers: Headers::default(), body: None };
    for field in plan.fields.iter().filter(|f| f.location == InputLocation::Query) {
        let Some(item) = input.get(&field.input_name) else { continue };
        request.params.extend(serialize_query(field, item)?);
    }
    request.headers.set_all(&plan.headers);
    for field in plan.fields.iter().filter(|f| f.location == InputLocation::Header) {
        let Some(item) = input.get(&field.input_name) else { continue };
        let serialized = serialize_simple(field, item, &|s: &str| s.to_string())?;
        request.headers.set(&field.name, &serialized);
    }
    if let Some(body) = &plan.body {
        match body.mode {
            BodyMode::Value => {
                let field = plan.fields.iter().find(|f| f.location == InputLocation::Body);
                if let Some(value) = field.and_then(|f| input.get(&f.input_name)) {
                    request.set_body_json(value, &body.media_type);
                }
            }
            BodyMode::Object => {
                let entries: Vec<(String, Json)> = plan
                    .fields
                    .iter()
                    .filter(|f| f.location == InputLocation::Body)
                    .filter_map(|f| input.get(&f.input_name).map(|item| (f.name.clone(), item.clone())))
                    .collect();
                if body.required || !entries.is_empty() {
                    request.set_body_json(&Json::Object(js_ordered(entries)), &body.media_type);
                }
            }
        }
    }
    Ok(request)
}

#[derive(Default)]
struct AppliedAuth {
    headers: Vec<(String, String)>,
    query: Vec<(String, String)>,
}

async fn resolve_auth(plan: &Plan) -> Result<AppliedAuth, ToolCallError> {
    if plan.security.is_empty() {
        return Ok(AppliedAuth::default());
    }
    let mut unavailable: Vec<String> = Vec::new();
    'alternatives: for requirement in &plan.security {
        if requirement.is_empty() {
            return Ok(AppliedAuth::default());
        }
        let mut credentials: Vec<(String, SecurityScheme, Credential)> = Vec::new();
        for (name, scopes) in requirement {
            let scheme = plan.schemes.iter().find(|(n, _)| n == name).map(|(_, s)| s.clone());
            let (Some(scheme), Some(auth)) = (scheme, plan.auth.as_ref()) else {
                unavailable.push(name.clone());
                continue 'alternatives;
            };
            let context = super::types::AuthContext {
                name: name.clone(),
                definition: scheme.clone(),
                scopes: scopes.clone(),
                operation: plan.operation.clone(),
            };
            match auth.resolve(context).await.map_err(ToolCallError::Failure)? {
                None => {
                    unavailable.push(name.clone());
                    continue 'alternatives;
                }
                Some(credential) => credentials.push((name.clone(), scheme, credential)),
            }
        }
        return apply_credentials(credentials);
    }
    let mut unique: Vec<String> = Vec::new();
    for name in unavailable {
        if !unique.contains(&name) {
            unique.push(name);
        }
    }
    Err(tool_error(format!(
        "{} {} requires authentication; no credential available for: {}.",
        plan.operation.method,
        plan.operation.path,
        unique.join(", ")
    )))
}

fn apply_credentials(credentials: Vec<(String, SecurityScheme, Credential)>) -> Result<AppliedAuth, ToolCallError> {
    let mut headers: Vec<(String, String)> = Vec::new();
    let mut query: Vec<(String, String)> = Vec::new();
    fn add(
        target: &mut Vec<(String, String)>,
        carrier: &str,
        name: String,
        value: String,
    ) -> Result<(), ToolCallError> {
        if target.iter().any(|(k, _)| *k == name) {
            return Err(tool_error(format!("Authentication resolves multiple credentials for {carrier} '{name}'.")));
        }
        target.push((name, value));
        Ok(())
    }
    for (name, definition, credential) in credentials {
        match credential {
            Credential::Bearer { token } => {
                add(&mut headers, "header", "authorization".into(), format!("Bearer {token}"))?
            }
            Credential::Basic { username, password } => {
                let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
                add(&mut headers, "header", "authorization".into(), format!("Basic {encoded}"))?
            }
            Credential::Header { name, value } => add(&mut headers, "header", name.to_lowercase(), value)?,
            Credential::ApiKey { value } => {
                let SecurityScheme::ApiKey { name: parameter, location } = definition else {
                    return Err(tool_error(format!(
                        "Security scheme '{name}' is not an apiKey scheme; resolve a bearer, basic, or header credential for it."
                    )));
                };
                match location {
                    ApiKeyLocation::Cookie => {
                        return Err(tool_error(format!("Cookie authentication '{name}' is not supported.")));
                    }
                    ApiKeyLocation::Header => add(&mut headers, "header", parameter.to_lowercase(), value)?,
                    ApiKeyLocation::Query => add(&mut query, "query", parameter, value)?,
                }
            }
        }
    }
    Ok(AppliedAuth { headers: js_record(&headers), query: js_record(&query) })
}

fn header<'r>(response: &'r HttpResponse, name: &str) -> Option<String> {
    let values: Vec<&'r str> =
        response.headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str()).collect();
    if values.is_empty() { None } else { Some(values.join(", ")) }
}

/// `Number.parseInt(value, 10)` restricted to what the size check needs.
fn parse_int(value: &str) -> f64 {
    let units: Vec<u16> = value.encode_utf16().collect();
    crate::number::parse_int(&units, 10.0)
}

fn read_response_body(response: &HttpResponse, plan: &Plan) -> Result<String, ToolCallError> {
    let exceeds = || tool_error(format!("{} {} response exceeds 50 MiB.", plan.operation.method, plan.operation.path));
    if let Some(length) = header(response, "content-length") {
        let parsed = parse_int(&length);
        let safe = parsed.is_finite() && parsed.fract() == 0.0 && parsed.abs() <= 9_007_199_254_740_991.0;
        if safe && parsed >= 0.0 && parsed > MAX_RESPONSE_BODY_BYTES as f64 {
            return Err(exceeds());
        }
    }
    if response.body.len() > MAX_RESPONSE_BODY_BYTES {
        return Err(exceeds());
    }
    let text = String::from_utf8_lossy(&response.body);
    Ok(text.strip_prefix('\u{FEFF}').unwrap_or(&text).to_string())
}

/// `JSON.parse` (JavaScriptCore semantics: correctly rounded numbers, last duplicate key wins, JS key order).
fn parse_json(text: &str) -> Option<Json> {
    use crate::stdlib::json::Parsed;
    fn convert(value: Parsed) -> Json {
        match value {
            Parsed::Null => Json::Null,
            Parsed::Bool(b) => Json::Bool(b),
            Parsed::Number(n) => crate::data::json_number(n),
            Parsed::String(s) => Json::String(s.to_rust()),
            Parsed::Array(items) => Json::Array(items.into_iter().map(convert).collect()),
            Parsed::Object(entries) => {
                Json::Object(js_ordered(entries.into_iter().map(|(k, v)| (k.to_rust(), convert(v))).collect()))
            }
        }
    }
    crate::stdlib::json::parse_json(&crate::value::JsStr::from(text)).ok().map(convert)
}

/// JSON objects in JS property order.
fn js_ordered_json(value: Json) -> Json {
    match value {
        Json::Object(map) => Json::Object(js_ordered(map.into_iter().map(|(k, v)| (k, js_ordered_json(v))).collect())),
        Json::Array(items) => Json::Array(items.into_iter().map(js_ordered_json).collect()),
        other => other,
    }
}

fn truncate_units(text: &str, max: usize) -> Option<String> {
    let units: Vec<u16> = text.encode_utf16().collect();
    if units.len() <= max {
        return None;
    }
    Some(String::from_utf16_lossy(&units[..max]))
}

pub(crate) async fn invoke(
    plan: Arc<Plan>,
    client: Arc<dyn HttpClient>,
    input: Option<Json>,
) -> Result<Option<Json>, ToolCallError> {
    let input = match input.map(js_ordered_json) {
        Some(Json::Object(map)) => map,
        _ => Map::new(),
    };
    let mut request = build_request(&plan, &input)?;
    let auth = resolve_auth(&plan).await?;
    for (name, item) in auth.query {
        request.params.retain(|(k, _)| *k != name);
        request.params.push((name, item));
    }
    request.headers.set_all(&auth.headers);

    let method = &plan.operation.method;
    let path = &plan.operation.path;
    let transport = format!("{method} {path} failed: transport error");
    // Effect's client errors describe the request by its URL without query parameters.
    let base = request.url.clone();
    let Some(url) = request.to_url() else {
        return Err(tool_error_with_cause(
            &transport,
            &format!(
                "HttpClientError: InvalidUrl error ({method} {base}) (cause: UrlError (cause: TypeError: \"{base}\" cannot be parsed as a URL.))"
            ),
        ));
    };
    let http = HttpRequest {
        method: method.clone(),
        url,
        headers: request.headers.entries(),
        body: request.body,
        timeout: None,
    };
    let response = client.send(http).await.map_err(|cause| {
        tool_error_with_cause(
            &transport,
            &format!("HttpClientError: Transport error ({method} {base}) (cause: Error: {cause})"),
        )
    })?;
    let text = read_response_body(&response, &plan)?;
    let media_type = header(&response, "content-type").map(|v| super::spec::normalize_media_type(&v));
    let json = media_type.as_deref().is_some_and(|m| m == "application/json" || m.ends_with("+json"));
    let decoded: Option<Json> = if text.is_empty() {
        Some(Json::Null)
    } else if json {
        parse_json(&text)
    } else {
        None
    };
    let parsed = if json {
        decoded.clone().unwrap_or_else(|| Json::String(text.clone()))
    } else if text.is_empty() {
        Json::Null
    } else {
        Json::String(text.clone())
    };
    if !(200..300).contains(&response.status) {
        let rendered = match &parsed {
            Json::String(s) => s.clone(),
            other => stringify(other, ""),
        };
        let summary = if rendered.is_empty() || rendered == "null" {
            "no response body".to_string()
        } else {
            match truncate_units(&rendered, MAX_ERROR_BODY_CHARS) {
                Some(prefix) => format!("{prefix}..."),
                None => rendered,
            }
        };
        return Err(tool_error(format!("{method} {path} failed with HTTP {}: {summary}", response.status)));
    }
    if json && decoded.is_none() {
        return Err(tool_error(format!("{method} {path} returned malformed JSON.")));
    }
    Ok(Some(parsed))
}
