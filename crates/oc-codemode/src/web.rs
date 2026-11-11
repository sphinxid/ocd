//! The `fetch` extension (core's `codemode/fetch.ts`): a host-provided outbound HTTP capability with a 30 s timeout
//! that buffers the whole body and returns `{ url, status, statusText, ok, redirected, headers: { get, has,
//! entries }, text(), json(), bytes() }`. The host may check each request first (core asserts the `webfetch`
//! permission); a rejection rejects the program's promise and nothing is sent. URLs with credentials are refused, the
//! `#fragment` is dropped before any check, and redirects are followed here (up to 20, one 30 s deadline for the whole
//! chain) so the host can check each target against the same rules (#60502, #61701, #62373).

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;

use crate::host::{host_fn, host_fn_async, HostCall, HostFunction, HostValue, HttpClient, HttpRequest, HttpResponse, SimpleExtension};

pub const TIMEOUT_MS: u64 = 30_000;

/// Checks a `fetch` call before anything is sent: `(url, args)`, where `url` is `String(input)` without its
/// `#fragment`. An `Err` is the rejection the program sees.
pub type Authorize = Arc<dyn Fn(String, Vec<HostValue>) -> BoxFuture<'static, Result<(), HostValue>> + Send + Sync>;

/// Checks each redirect before it is followed: `(next, args, url)`, the target without its fragment, the call's
/// arguments and the URL the program fetched. An `Err` is the rejection the program sees.
pub type CheckRedirect = Arc<dyn Fn(String, Vec<HostValue>, String) -> BoxFuture<'static, Result<(), HostValue>> + Send + Sync>;

/// Redirects followed before a fetch fails.
pub const MAX_REDIRECTS: usize = 20;
const REDIRECT_STATUSES: [u16; 5] = [301, 302, 303, 307, 308];

/// The `fetch` extension exposing `fetch`, without a permission check.
pub fn extension(client: Arc<dyn HttpClient>) -> Arc<dyn crate::host::Extension> {
    extension_with(client, None)
}

/// The `fetch` extension, checking each call with `authorize` after the `file:` refusal.
pub fn extension_with(client: Arc<dyn HttpClient>, authorize: Option<Authorize>) -> Arc<dyn crate::host::Extension> {
    extension_checked(client, authorize, None)
}

/// The `fetch` extension, checking each call with `authorize` and each redirect target with `check_redirect`.
/// Redirects are followed here, so `client` must not follow them itself (see [`ReqwestClient::manual`]).
pub fn extension_checked(
    client: Arc<dyn HttpClient>,
    authorize: Option<Authorize>,
    check_redirect: Option<CheckRedirect>,
) -> Arc<dyn crate::host::Extension> {
    let fetch = Fetch { client, authorize, check_redirect };
    Arc::new(SimpleExtension { name: "fetch".into(), globals: vec![("fetch".into(), Arc::new(fetch) as Arc<dyn HostFunction>)] })
}

struct Fetch {
    client: Arc<dyn HttpClient>,
    authorize: Option<Authorize>,
    check_redirect: Option<CheckRedirect>,
}

/// Rules match the URL, so `https://user@evil.com` would slip past a rule for `https://evil.com/*`.
fn has_credentials(url: &url::Url) -> bool {
    !url.username().is_empty() || url.password().is_some_and(|p| !p.is_empty())
}

impl HostFunction for Fetch {
    fn name(&self) -> &str {
        "fetch"
    }
    fn length(&self) -> usize {
        1
    }
    // Core's `fetch` is an `async` function.
    fn asynchronous(&self) -> bool {
        true
    }
    fn call(&self, args: Vec<HostValue>) -> HostCall {
        // Rules match the URL as written. The fragment is never sent, and would keep a rule for the exact URL from
        // matching.
        let input = string_of(args.first().unwrap_or(&HostValue::Undefined));
        let url = input.split('#').next().unwrap_or_default().to_string();
        let parsed = url::Url::parse(&url).ok();
        // `URL.parse(url)?.protocol === "file:"`: the host never reads local files for a program.
        if parsed.as_ref().is_some_and(|u| u.scheme() == "file") {
            return HostCall::Pending(Box::pin(async {
                Err(HostValue::error("Error", "fetch cannot be used for filesystem access."))
            }));
        }
        if parsed.as_ref().is_some_and(has_credentials) {
            return HostCall::Pending(Box::pin(async {
                Err(HostValue::error("TypeError", "fetch cannot be used with credentials in the URL."))
            }));
        }
        let client = self.client.clone();
        let authorize = self.authorize.clone();
        let check_redirect = self.check_redirect.clone();
        HostCall::Pending(Box::pin(async move {
            if let Some(authorize) = authorize {
                authorize(url.clone(), args.clone()).await?;
            }
            let mut request = build_request(&args)?;
            // One deadline covers the whole redirect chain and the body.
            let deadline = tokio::time::Instant::now() + Duration::from_millis(TIMEOUT_MS);
            let mut target = url.clone();
            let mut redirects = 0;
            loop {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    return Err(HostValue::error("TypeError", "The operation timed out."));
                }
                request.url = target.clone();
                request.timeout = Some(remaining);
                let mut response = client.send(request.clone()).await.map_err(|message| HostValue::error("TypeError", message))?;
                let location = response.headers.iter().rev().find(|(k, _)| k.eq_ignore_ascii_case("location")).map(|(_, v)| v.clone());
                let Some(location) = location.filter(|_| REDIRECT_STATUSES.contains(&response.status)) else {
                    response.url = target;
                    response.redirected = redirects > 0;
                    return Ok(response_value(response));
                };
                if redirects >= MAX_REDIRECTS {
                    return Err(HostValue::error("Error", format!("Too many redirects (more than {MAX_REDIRECTS})")));
                }
                let mut next = url::Url::parse(&target)
                    .and_then(|base| base.join(&location))
                    .map_err(|_| HostValue::error("TypeError", format!("{} cannot be parsed as a URL.", crate::data::quote(&location))))?;
                if next.scheme() != "http" && next.scheme() != "https" {
                    return Err(HostValue::error("Error", format!("fetch cannot follow a redirect to {}: URLs.", next.scheme())));
                }
                if has_credentials(&next) {
                    return Err(HostValue::error("Error", "fetch cannot follow a redirect with credentials in the URL."));
                }
                next.set_fragment(None);
                if let Some(check) = &check_redirect {
                    check(next.to_string(), args.clone(), url.clone()).await?;
                }
                request = redirected(request, response.status, &target, &next);
                target = next.to_string();
                redirects += 1;
            }
        }))
    }
}

/// The request a browser sends after a redirect: 303, and 301/302 after a POST, become a GET without a body, and
/// credentials are not sent to another origin.
fn redirected(request: HttpRequest, status: u16, from: &str, to: &url::Url) -> HttpRequest {
    let method = request.method.to_uppercase();
    let to_get = if status == 303 { method != "HEAD" } else { (status == 301 || status == 302) && method == "POST" };
    let cross_origin = url::Url::parse(from).map(|from| from.origin() != to.origin()).unwrap_or(true);
    let dropped: &[&str] = &["content-type", "content-length", "content-encoding", "content-language", "content-location"];
    let credentials: &[&str] = &["authorization", "cookie", "proxy-authorization"];
    let headers = request
        .headers
        .into_iter()
        .filter(|(name, _)| {
            let name = name.to_ascii_lowercase();
            !(to_get && dropped.contains(&name.as_str())) && !(cross_origin && credentials.contains(&name.as_str()))
        })
        .collect();
    HttpRequest {
        method: if to_get { "GET".into() } else { request.method },
        url: request.url,
        headers,
        body: if to_get { None } else { request.body },
        timeout: request.timeout,
    }
}
pub fn fetch_function(client: Arc<dyn HttpClient>) -> Arc<dyn HostFunction> {
    Arc::new(Fetch { client, authorize: None, check_redirect: None })
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
        host_fn_async("text", 0, move |_| {
            let t = text_of(&b);
            HostCall::Pending(Box::pin(async move { Ok(HostValue::String(t)) }))
        })
    };
    let json = {
        let b = body.clone();
        host_fn_async("json", 0, move |_| {
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
        host_fn_async("bytes", 0, move |_| {
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
    /// A client that returns redirects instead of following them (`redirect: "manual"`), for the `fetch` extension,
    /// which follows them itself.
    pub fn manual() -> Self {
        let client = oc_util::http::client_builder().redirect(reqwest::redirect::Policy::none()).build().unwrap_or_default();
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
