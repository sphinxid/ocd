//! The `fetch` extension (core's `codemode/web.ts`): a host-provided outbound HTTP capability with a 30 s timeout
//! that buffers the whole body and returns `{ url, status, statusText, ok, redirected, headers: { get, has,
//! entries }, text(), json(), bytes() }`.

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;

use crate::host::{host_fn, HostCall, HostFunction, HostValue, HttpClient, HttpRequest, HttpResponse, SimpleExtension};

pub const TIMEOUT_MS: u64 = 30_000;

/// The `web` extension exposing `fetch`.
pub fn extension(client: Arc<dyn HttpClient>) -> Arc<dyn crate::host::Extension> {
    Arc::new(SimpleExtension { name: "web".into(), globals: vec![("fetch".into(), fetch_function(client))] })
}

struct Fetch {
    client: Arc<dyn HttpClient>,
}

impl HostFunction for Fetch {
    fn name(&self) -> &str {
        "fetch"
    }
    fn length(&self) -> usize {
        1
    }
    fn call(&self, args: Vec<HostValue>) -> HostCall {
        let request = match build_request(&args) {
            Ok(r) => r,
            Err(e) => return HostCall::Pending(Box::pin(async move { Err(e) })),
        };
        let client = self.client.clone();
        HostCall::Pending(Box::pin(async move {
            match client.send(request).await {
                Ok(response) => Ok(response_value(response)),
                Err(message) => Err(HostValue::error("TypeError", message)),
            }
        }))
    }
}

pub fn fetch_function(client: Arc<dyn HttpClient>) -> Arc<dyn HostFunction> {
    Arc::new(Fetch { client })
}

fn string_of(value: &HostValue) -> String {
    match value {
        HostValue::String(s) | HostValue::Url(s) => s.clone(),
        HostValue::Number(n) => crate::number::to_string(*n),
        HostValue::Bool(b) => b.to_string(),
        HostValue::Null => "null".into(),
        HostValue::Undefined => "undefined".into(),
        _ => "[object Object]".into(),
    }
}

fn build_request(args: &[HostValue]) -> Result<HttpRequest, HostValue> {
    let url = string_of(args.first().unwrap_or(&HostValue::Undefined));
    let init = args.get(1);
    let method = init.and_then(|i| i.field("method")).filter(|m| !matches!(m, HostValue::Undefined)).map(string_of);
    let mut headers: Vec<(String, String)> = Vec::new();
    match init.and_then(|i| i.field("headers")) {
        Some(HostValue::Object(fields)) => {
            for (k, v) in fields {
                headers.push((k.clone(), string_of(v)));
            }
        }
        Some(HostValue::Array(pairs)) => {
            for pair in pairs {
                match pair {
                    HostValue::Array(kv) if kv.len() == 2 => headers.push((string_of(&kv[0]), string_of(&kv[1]))),
                    _ => return Err(HostValue::error("TypeError", "Header sequence must contain exactly two items")),
                }
            }
        }
        Some(HostValue::Headers(list)) => headers.extend(list.iter().cloned()),
        _ => {}
    }
    let body = match init.and_then(|i| i.field("body")) {
        None | Some(HostValue::Undefined) | Some(HostValue::Null) => None,
        Some(HostValue::Bytes(b)) => Some(b.clone()),
        Some(HostValue::SearchParams(pairs)) => {
            if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-type")) {
                headers.push(("content-type".into(), "application/x-www-form-urlencoded;charset=UTF-8".into()));
            }
            Some(url::form_urlencoded::Serializer::new(String::new()).extend_pairs(pairs.iter()).finish().into_bytes())
        }
        Some(other) => {
            if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-type")) {
                headers.push(("content-type".into(), "text/plain;charset=UTF-8".into()));
            }
            Some(string_of(other).into_bytes())
        }
    };
    let method = method.map(|m| m.to_uppercase()).unwrap_or_else(|| "GET".into());
    Ok(HttpRequest { method, url, headers, body, timeout: Some(Duration::from_millis(TIMEOUT_MS)) })
}

fn response_value(response: HttpResponse) -> HostValue {
    let body = Arc::new(response.body);
    let headers = Arc::new(response.headers.iter().map(|(k, v)| (k.to_lowercase(), v.clone())).collect::<Vec<_>>());
    // Object.fromEntries(response.headers): later duplicates win.
    let lookup = {
        let h = headers.clone();
        move |name: &str| h.iter().rev().find(|(k, _)| k == name).map(|(_, v)| v.clone())
    };
    let get = {
        let lookup = lookup.clone();
        host_fn("get", 1, move |args| {
            let name = string_of(args.first().unwrap_or(&HostValue::Undefined)).to_lowercase();
            HostCall::Ready(Ok(lookup(&name).map(HostValue::String).unwrap_or(HostValue::Null)))
        })
    };
    let has = {
        let lookup = lookup.clone();
        host_fn("has", 1, move |args| {
            let name = string_of(args.first().unwrap_or(&HostValue::Undefined)).to_lowercase();
            HostCall::Ready(Ok(HostValue::Bool(lookup(&name).is_some())))
        })
    };
    let entries = {
        let h = headers.clone();
        host_fn("entries", 0, move |_| {
            let mut seen: Vec<(String, String)> = Vec::new();
            for (k, v) in h.iter() {
                match seen.iter_mut().find(|(x, _)| x == k) {
                    Some(e) => e.1 = v.clone(),
                    None => seen.push((k.clone(), v.clone())),
                }
            }
            HostCall::Ready(Ok(HostValue::Array(
                seen.into_iter().map(|(k, v)| HostValue::Array(vec![HostValue::String(k), HostValue::String(v)])).collect(),
            )))
        })
    };
    let text_of = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    let text = {
        let b = body.clone();
        host_fn("text", 0, move |_| {
            let t = text_of(&b);
            HostCall::Pending(Box::pin(async move { Ok(HostValue::String(t)) }))
        })
    };
    let json = {
        let b = body.clone();
        host_fn("json", 0, move |_| {
            let t = text_of(&b);
            HostCall::Pending(Box::pin(async move {
                let parsed = crate::stdlib::json::parse_json(&crate::value::JsStr::from(t.as_str()))
                    .map_err(|m| HostValue::error("SyntaxError", m))?;
                Ok(parsed_to_host(&parsed))
            }))
        })
    };
    let bytes = {
        let b = body.clone();
        host_fn("bytes", 0, move |_| {
            let v = (*b).clone();
            HostCall::Pending(Box::pin(async move { Ok(HostValue::Bytes(v)) }))
        })
    };
    let status = response.status;
    HostValue::Object(vec![
        ("url".into(), HostValue::String(response.url)),
        ("status".into(), HostValue::Number(status as f64)),
        ("statusText".into(), HostValue::String(response.status_text)),
        ("ok".into(), HostValue::Bool((200..300).contains(&status))),
        ("redirected".into(), HostValue::Bool(response.redirected)),
        ("headers".into(), HostValue::Object(vec![("get".into(), get), ("has".into(), has), ("entries".into(), entries)])),
        ("text".into(), text),
        ("json".into(), json),
        ("bytes".into(), bytes),
    ])
}

fn parsed_to_host(p: &crate::stdlib::json::Parsed) -> HostValue {
    use crate::stdlib::json::Parsed;
    match p {
        Parsed::Null => HostValue::Null,
        Parsed::Bool(b) => HostValue::Bool(*b),
        Parsed::Number(n) => HostValue::Number(*n),
        Parsed::String(s) => HostValue::String(s.to_rust()),
        Parsed::Array(items) => HostValue::Array(items.iter().map(parsed_to_host).collect()),
        Parsed::Object(fields) => HostValue::Object(fields.iter().map(|(k, v)| (k.to_rust(), parsed_to_host(v))).collect()),
    }
}

/// An [`HttpClient`] backed by `reqwest`.
pub struct ReqwestClient {
    client: reqwest::Client,
}

impl ReqwestClient {
    pub fn new() -> Self {
        ReqwestClient { client: oc_util::http::client() }
    }
    pub fn with_client(client: reqwest::Client) -> Self {
        ReqwestClient { client }
    }
}

impl Default for ReqwestClient {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpClient for ReqwestClient {
    fn send(&self, request: HttpRequest) -> BoxFuture<'static, Result<HttpResponse, String>> {
        let client = self.client.clone();
        Box::pin(async move {
            let method = reqwest::Method::from_bytes(request.method.as_bytes()).map_err(|_| {
                format!("'{}' is not a valid HTTP method.", request.method)
            })?;
            let mut builder = client.request(method, &request.url);
            for (k, v) in &request.headers {
                builder = builder.header(k, v);
            }
            if let Some(body) = request.body {
                builder = builder.body(body);
            }
            if let Some(t) = request.timeout {
                builder = builder.timeout(t);
            }
            let response = builder.send().await.map_err(|e| {
                if e.is_timeout() { "The operation timed out.".to_string() } else { e.to_string() }
            })?;
            let url = response.url().to_string();
            let status = response.status();
            let headers: Vec<(String, String)> = response
                .headers()
                .iter()
                .map(|(k, v)| (k.as_str().to_string(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
                .collect();
            let redirected = url != request.url && url.trim_end_matches('/') != request.url.trim_end_matches('/');
            let body = response.bytes().await.map_err(|e| {
                if e.is_timeout() { "The operation timed out.".to_string() } else { e.to_string() }
            })?;
            Ok(HttpResponse {
                url,
                status: status.as_u16(),
                status_text: status.canonical_reason().unwrap_or("").to_string(),
                redirected,
                headers,
                body: body.to_vec(),
            })
        })
    }
}
