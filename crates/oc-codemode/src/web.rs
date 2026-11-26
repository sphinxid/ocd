//! The `fetch` extension (core's `codemode/fetch.ts`): a host-provided outbound HTTP capability with a 30 s timeout.
//! Since 2.3.0 (#63436) the response streams like Bun's (`Extension.StreamingResponse`, converted by the VM's
//! `runtime/extensions.ts`): `{ url, status, statusText, ok, headers, redirected, body, bodyUsed, text(), json(),
//! bytes(), arrayBuffer() }`, where `headers` is a real `Headers`, `body` is `null` (HEAD, 204, 205, 304) or a stream
//! (`locked`, `cancel()`, `getReader()` → `{ read, cancel, releaseLock }`, `for await`), and the whole-body readers
//! consume it once. The host may check each request first (core asserts the `webfetch`
//! permission); a rejection rejects the program's promise and nothing is sent. URLs with credentials are refused, the
//! `#fragment` is dropped before any check, and redirects are followed here (up to 20, one 30 s deadline for the whole
//! chain) so the host can check each target against the same rules (#60502, #61701, #62373).

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;

use futures::StreamExt;

use crate::host::{
    host_fn, host_fn_async, BodyStream, HostCall, HostFunction, HostKey, HostProperty, HostValue, HttpClient, HttpRequest,
    HttpResponse, HttpStreamingResponse, SimpleExtension,
};

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
    extension_api(client, authorize, check_redirect, None)
}

/// [`extension_checked`] where an `opencode:///api/...` request fails with `api_refusal` (the session's
/// `CodeModeApi.disabled` in a release build), or [`OPENCODE_UNAVAILABLE`] without one.
pub fn extension_api(
    client: Arc<dyn HttpClient>,
    authorize: Option<Authorize>,
    check_redirect: Option<CheckRedirect>,
    api_refusal: Option<String>,
) -> Arc<dyn crate::host::Extension> {
    let fetch = Fetch { client, authorize, check_redirect, api_refusal };
    Arc::new(SimpleExtension { name: "fetch".into(), globals: vec![("fetch".into(), Arc::new(fetch) as Arc<dyn HostFunction>)] })
}

/// `CodeModeApi.unavailable`: an `opencode://` request where no OpenCode API is provided.
pub const OPENCODE_UNAVAILABLE: &str = "opencode:// URLs are unavailable in this environment.";
/// `CodeModeApi.disabled`: what a session's `opencode://` request gets while the local-only `opencode_api` flag is
/// off, as in every release build (2.4.1, #65725).
pub const OPENCODE_DISABLED: &str =
    "opencode:// URLs are off. In a local or dev build, enable them with OPENCODE_FLAGS=opencode_api or \"experimental\": { \"opencode_api\": true }.";

/// `CodeModeApi.path(raw)`'s check: only `opencode:///api/...` is accepted, with no host and nothing outside `/api/`.
fn opencode_path_error(raw: &str, parsed: Option<&url::Url>) -> Option<HostValue> {
    let valid = raw.starts_with("opencode:///") && parsed.is_some_and(|u| u.path().starts_with("/api/"));
    (!valid).then(|| {
        HostValue::error(
            "TypeError",
            format!("Invalid opencode URI \"{raw}\": use opencode:///api/..., for example \"opencode:///api/session\"."),
        )
    })
}

struct Fetch {
    client: Arc<dyn HttpClient>,
    authorize: Option<Authorize>,
    check_redirect: Option<CheckRedirect>,
    api_refusal: Option<String>,
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
        // `fetchOpencode`: no permission check; ocd has no in-process OpenCode API, so a valid URL is refused.
        if parsed.as_ref().is_some_and(|u| u.scheme() == "opencode") {
            let error = opencode_path_error(&url, parsed.as_ref())
                .unwrap_or_else(|| HostValue::error("Error", self.api_refusal.as_deref().unwrap_or(OPENCODE_UNAVAILABLE)));
            return HostCall::Pending(Box::pin(async move { Err(error) }));
        }
        let client = self.client.clone();
        let authorize = self.authorize.clone();
        let check_redirect = self.check_redirect.clone();
        HostCall::Pending(Box::pin(async move {
            if let Some(authorize) = authorize {
                authorize(url.clone(), args.clone()).await?;
            }
            // Bun's fetch refuses a URL that does not parse (`fetch(undefined)`, a relative path).
            if parsed.is_none() {
                return Err(type_error("fetch() URL is invalid"));
            }
            let mut request = build_request(&args)?;
            // One deadline covers the whole redirect chain and the body.
            let deadline = tokio::time::Instant::now() + Duration::from_millis(TIMEOUT_MS);
            let mut target = url.clone();
            let mut redirects = 0;
            loop {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    return Err(transport_error("The operation timed out.".into()));
                }
                request.url = target.clone();
                request.timeout = Some(remaining);
                let mut response =
                    client.send_streaming(request.clone()).await.map_err(transport_error)?;
                let location = response.headers.iter().rev().find(|(k, _)| k.eq_ignore_ascii_case("location")).map(|(_, v)| v.clone());
                let Some(location) = location.filter(|_| REDIRECT_STATUSES.contains(&response.status)) else {
                    response.url = target;
                    response.redirected = redirects > 0;
                    return Ok(response_value(response, &request.method));
                };
                // The redirect's body is discarded (cancelled) before the next hop.
                drop(std::mem::replace(&mut response.body, Box::pin(futures::stream::empty())));
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
    Arc::new(Fetch { client, authorize: None, check_redirect: None, api_refusal: None })
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
        Some(HostValue::Bytes(b) | HostValue::Buffer(b)) => Some(b.clone()),
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

/// The body of a streaming response, shared by the response's readers (a `ReadableStream` as Bun's `fetch` gives it).
struct Body {
    state: std::sync::Mutex<BodyState>,
    /// `None` once the body ended or was cancelled. Reads queue on the lock, so they settle in order.
    stream: tokio::sync::Mutex<Option<BodyStream>>,
}

#[derive(Default)]
struct BodyState {
    /// Bun's `bodyUsed`: set by any read, a reader, async iteration or a cancel.
    used: bool,
    /// The reader holding the lock (readers are numbered; a whole-body read holds it for good).
    lock: Option<u64>,
    readers: u64,
    /// The error the body ended with, for every later read.
    error: Option<HostValue>,
}

const LOCKED: &str = "Invalid state: ReadableStream is locked";
const DETACHED: &str = "Invalid state: The reader is not attached to a stream";

fn type_error(message: &str) -> HostValue {
    HostValue::error("TypeError", message)
}

/// `{ value, done }`, a read result.
fn read_result(value: Option<Vec<u8>>) -> HostValue {
    let done = value.is_none();
    HostValue::Object(vec![
        ("value".into(), value.map(HostValue::Bytes).unwrap_or(HostValue::Undefined)),
        ("done".into(), HostValue::Bool(done)),
    ])
}

/// A transport or body error as the program catches it: the 30 s timeout is Bun's `TimeoutError` DOMException, which
/// crosses as an `Error`; anything else is a `TypeError`, like Bun's network errors.
fn transport_error(message: String) -> HostValue {
    if message == "The operation timed out." { HostValue::error("Error", message) } else { type_error(&message) }
}

impl Body {
    fn new(stream: BodyStream) -> Arc<Body> {
        Arc::new(Body { state: std::sync::Mutex::new(BodyState::default()), stream: tokio::sync::Mutex::new(Some(stream)) })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, BodyState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Takes the lock for a new reader.
    fn acquire(&self) -> Result<u64, HostValue> {
        let mut state = self.state();
        if state.lock.is_some() {
            return Err(type_error(LOCKED));
        }
        state.readers += 1;
        let reader = state.readers;
        state.lock = Some(reader);
        state.used = true;
        Ok(reader)
    }

    fn attached(&self, reader: u64) -> Result<(), HostValue> {
        if self.state().lock == Some(reader) { Ok(()) } else { Err(type_error(DETACHED)) }
    }

    fn release(&self, reader: u64) {
        let mut state = self.state();
        if state.lock == Some(reader) {
            state.lock = None;
        }
    }

    /// The next chunk, or `None` at the end.
    async fn next(&self) -> Result<Option<Vec<u8>>, HostValue> {
        if let Some(error) = self.state().error.clone() {
            return Err(error);
        }
        let mut stream = self.stream.lock().await;
        loop {
            let Some(body) = stream.as_mut() else { return Ok(None) };
            match body.next().await {
                Some(Ok(chunk)) if chunk.is_empty() => continue,
                Some(Ok(chunk)) => return Ok(Some(chunk)),
                None => {
                    *stream = None;
                    return Ok(None);
                }
                Some(Err(message)) => {
                    *stream = None;
                    let error = transport_error(message);
                    self.state().error = Some(error.clone());
                    return Err(error);
                }
            }
        }
    }

    async fn read(&self, reader: u64) -> Result<HostValue, HostValue> {
        self.attached(reader)?;
        self.state().used = true;
        Ok(read_result(self.next().await?))
    }

    async fn cancel(&self) {
        self.state().used = true;
        *self.stream.lock().await = None;
    }

    /// The whole body, once (`text`, `json`, `bytes`, `arrayBuffer`).
    async fn consume(&self) -> Result<Vec<u8>, HostValue> {
        {
            let mut state = self.state();
            if state.used || state.lock.is_some() {
                return Err(type_error("Body already used"));
            }
            state.used = true;
            state.lock = Some(u64::MAX);
        }
        let mut out = Vec::new();
        while let Some(chunk) = self.next().await? {
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }
}

/// Response text: UTF-8, its BOM dropped, malformed bytes replaced.
fn decode_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes)).into_owned()
}

/// What a whole-body reader returns for the body's bytes.
#[derive(Clone, Copy)]
enum Whole {
    Text,
    Json,
    Bytes,
}

fn whole_value(kind: Whole, bytes: Vec<u8>) -> Result<HostValue, HostValue> {
    Ok(match kind {
        Whole::Text => HostValue::String(decode_text(&bytes)),
        Whole::Bytes => HostValue::Bytes(bytes),
        // Bun reads an empty body (a null one too) as no JSON text at all.
        Whole::Json if bytes.is_empty() => return Err(HostValue::error("SyntaxError", "Unexpected end of JSON input")),
        Whole::Json => {
            let text = decode_text(&bytes);
            let parsed = crate::stdlib::json::parse_json(&crate::value::JsStr::from(text.as_str()))
                .map_err(|m| HostValue::error("SyntaxError", m))?;
            parsed_to_host(&parsed)
        }
    })
}

/// The body stream object: `locked`, `cancel(reason)`, `getReader(options)` and `[Symbol.asyncIterator]()`.
fn stream_value(body: &Arc<Body>) -> HostValue {
    let locked = {
        let body = body.clone();
        HostProperty::Getter(Arc::new(move || HostValue::Bool(body.state().lock.is_some())))
    };
    let cancel = {
        let body = body.clone();
        host_fn_async("", 1, move |_| {
            let body = body.clone();
            HostCall::Pending(Box::pin(async move {
                if body.state().lock.is_some() {
                    return Err(type_error(LOCKED));
                }
                body.cancel().await;
                Ok(HostValue::Undefined)
            }))
        })
    };
    let get_reader = {
        let body = body.clone();
        host_fn("", 1, move |args| {
            let mode = args.first().and_then(|options| options.field("mode"));
            if mode.is_some_and(|mode| !matches!(mode, HostValue::Undefined)) {
                return HostCall::Ready(Err(type_error("Only the default stream reader is supported.")));
            }
            HostCall::Ready(body.acquire().map(|reader| reader_value(&body, reader)))
        })
    };
    let iterator = {
        let body = body.clone();
        host_fn("", 0, move |_| HostCall::Ready(body.acquire().map(|reader| iterator_value(&body, reader))))
    };
    HostValue::Live(vec![
        (HostKey::String("locked".into()), locked),
        (HostKey::String("cancel".into()), HostProperty::Value(cancel)),
        (HostKey::String("getReader".into()), HostProperty::Value(get_reader)),
        (HostKey::AsyncIterator, HostProperty::Value(iterator)),
    ])
}

/// A default reader: `{ read, cancel, releaseLock }`.
fn reader_value(body: &Arc<Body>, reader: u64) -> HostValue {
    let read = {
        let body = body.clone();
        host_fn_async("read", 0, move |_| {
            let body = body.clone();
            HostCall::Pending(Box::pin(async move { body.read(reader).await }))
        })
    };
    let cancel = {
        let body = body.clone();
        host_fn_async("cancel", 1, move |_| {
            let body = body.clone();
            HostCall::Pending(Box::pin(async move {
                body.attached(reader)?;
                body.cancel().await;
                Ok(HostValue::Undefined)
            }))
        })
    };
    let release = {
        let body = body.clone();
        host_fn("releaseLock", 0, move |_| {
            body.release(reader);
            HostCall::Ready(Ok(HostValue::Undefined))
        })
    };
    HostValue::Object(vec![("read".into(), read), ("cancel".into(), cancel), ("releaseLock".into(), release)])
}

/// `body.values()` as `{ next, return }`: the lock is released at the end, and `return` cancels the body.
fn iterator_value(body: &Arc<Body>, reader: u64) -> HostValue {
    let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let next = {
        let (body, finished) = (body.clone(), finished.clone());
        host_fn_async("next", 0, move |_| {
            let (body, finished) = (body.clone(), finished.clone());
            HostCall::Pending(Box::pin(async move {
                if finished.load(std::sync::atomic::Ordering::SeqCst) {
                    return Ok(read_result(None));
                }
                let result = body.read(reader).await;
                if !matches!(result, Ok(HostValue::Object(ref fields)) if matches!(fields[1].1, HostValue::Bool(false))) {
                    finished.store(true, std::sync::atomic::Ordering::SeqCst);
                    body.release(reader);
                }
                result
            }))
        })
    };
    let finish = {
        let (body, finished) = (body.clone(), finished.clone());
        host_fn_async("return", 0, move |_| {
            let (body, finished) = (body.clone(), finished.clone());
            HostCall::Pending(Box::pin(async move {
                if !finished.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    body.cancel().await;
                    body.release(reader);
                }
                Ok(read_result(None))
            }))
        })
    };
    HostValue::Object(vec![("next".into(), next), ("return".into(), finish)])
}

fn response_value(response: HttpStreamingResponse, method: &str) -> HostValue {
    let status = response.status;
    // Bun gives no body for a HEAD request or a null-body status.
    let null_body = method.eq_ignore_ascii_case("HEAD") || matches!(status, 101 | 103 | 204 | 205 | 304);
    let body = Body::new(response.body);
    let whole = |kind: Whole| {
        let body = body.clone();
        host_fn_async("", 0, move |_| {
            let body = body.clone();
            HostCall::Pending(Box::pin(async move {
                let bytes = if null_body { Vec::new() } else { body.consume().await? };
                whole_value(kind, bytes)
            }))
        })
    };
    let used = {
        let body = body.clone();
        HostProperty::Getter(Arc::new(move || HostValue::Bool(!null_body && body.state().used)))
    };
    let value = |v: HostValue| HostProperty::Value(v);
    let key = |k: &str| HostKey::String(k.into());
    HostValue::Live(vec![
        (key("url"), value(HostValue::String(response.url))),
        (key("status"), value(HostValue::Number(status as f64))),
        (key("statusText"), value(HostValue::String(response.status_text))),
        (key("ok"), value(HostValue::Bool((200..300).contains(&status)))),
        (key("headers"), value(HostValue::Headers(response.headers.into_iter().map(|(k, v)| (k.to_lowercase(), v)).collect()))),
        (key("redirected"), value(HostValue::Bool(response.redirected))),
        (key("body"), value(if null_body { HostValue::Null } else { stream_value(&body) })),
        (key("bodyUsed"), used),
        (key("text"), value(whole(Whole::Text))),
        (key("json"), value(whole(Whole::Json))),
        (key("bytes"), value(whole(Whole::Bytes))),
        // `arrayBuffer()` crosses as a `Uint8Array`.
        (key("arrayBuffer"), value(whole(Whole::Bytes))),
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

fn reqwest_error(e: reqwest::Error) -> String {
    if e.is_timeout() { "The operation timed out.".to_string() } else { e.to_string() }
}

/// Sends a request and returns the response head: `(response, url, redirected, headers)`.
async fn send_head(
    client: reqwest::Client,
    request: HttpRequest,
) -> Result<(reqwest::Response, String, bool, Vec<(String, String)>), String> {
    let method = reqwest::Method::from_bytes(request.method.as_bytes())
        .map_err(|_| format!("'{}' is not a valid HTTP method.", request.method))?;
    let mut builder = client.request(method, &request.url);
    for (k, v) in &request.headers {
        builder = builder.header(k, v);
    }
    if let Some(body) = request.body {
        builder = builder.body(body);
    }
    // The timeout covers the body too.
    if let Some(t) = request.timeout {
        builder = builder.timeout(t);
    }
    let response = builder.send().await.map_err(reqwest_error)?;
    let url = response.url().to_string();
    let headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
        .collect();
    let redirected = url != request.url && url.trim_end_matches('/') != request.url.trim_end_matches('/');
    Ok((response, url, redirected, headers))
}

impl HttpClient for ReqwestClient {
    fn send(&self, request: HttpRequest) -> BoxFuture<'static, Result<HttpResponse, String>> {
        let client = self.client.clone();
        Box::pin(async move {
            let (response, url, redirected, headers) = send_head(client, request).await?;
            let status = response.status();
            let body = response.bytes().await.map_err(reqwest_error)?;
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

    fn send_streaming(&self, request: HttpRequest) -> BoxFuture<'static, Result<HttpStreamingResponse, String>> {
        let client = self.client.clone();
        Box::pin(async move {
            let (response, url, redirected, headers) = send_head(client, request).await?;
            let status = response.status();
            let body: BodyStream =
                Box::pin(response.bytes_stream().map(|chunk| chunk.map(|bytes| bytes.to_vec()).map_err(reqwest_error)));
            Ok(HttpStreamingResponse {
                url,
                status: status.as_u16(),
                status_text: status.canonical_reason().unwrap_or("").to_string(),
                redirected,
                headers,
                body,
            })
        })
    }
}
