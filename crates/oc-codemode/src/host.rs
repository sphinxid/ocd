//! The host interface: how the embedding application provides tools, hooks, and extension globals (such as
//! `fetch`).

use std::fmt;
use std::sync::Arc;

use futures::future::BoxFuture;

pub type Json = serde_json::Value;

/// Why a host tool call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolCallError {
    /// The tool refused or failed; reported as `ToolFailure` with this message.
    Failure(String),
    /// The tool produced output that failed validation; reported as
    /// `InvalidToolOutput: Invalid output from tool '<name>': <message>`.
    InvalidOutput(String),
}

impl ToolCallError {
    pub fn failure(message: impl Into<String>) -> Self {
        ToolCallError::Failure(message.into())
    }
}

impl fmt::Display for ToolCallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ToolCallError::Failure(m) | ToolCallError::InvalidOutput(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for ToolCallError {}

/// A tool call the program is making, with its (decoded) input. `name` is the canonical dotted path, or `search`.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolInvocation {
    pub id: u64,
    pub name: String,
    /// `None` when the program passed `undefined`.
    pub input: Option<Json>,
}

/// How a call ended; `after` hooks observe it and cannot change it.
#[derive(Debug, Clone, PartialEq)]
pub enum CallResult {
    Success(Option<Json>),
    Failure(String),
    Interrupted,
}

impl CallResult {
    pub fn status(&self) -> &'static str {
        match self {
            CallResult::Success(_) => "success",
            CallResult::Failure(_) => "failure",
            CallResult::Interrupted => "interrupted",
        }
    }
}

/// A call the program is making to an extension global.
#[derive(Debug, Clone)]
pub struct ExtensionInvocation {
    pub id: u64,
    pub extension: String,
    pub name: String,
    pub args: Vec<HostValue>,
}

/// The embedding application: executes tools and observes every call the program makes into the host.
///
/// Futures returned here run on the caller's tokio runtime context (they are polled from the interpreter thread,
/// which has entered the runtime of the thread that started the execution).
pub trait Host: Send + Sync + 'static {
    /// Executes one admitted tool call. `path` is the canonical dotted path (`"ns.tool"`), `input` the decoded input
    /// (`None` when the program passed `undefined`). The returned JSON is what the program receives (after the
    /// output schema copy); `Ok(None)` is `undefined`.
    fn call(&self, path: &str, input: Option<Json>) -> BoxFuture<'static, Result<Option<Json>, ToolCallError>>;

    /// Validates/decodes a tool input before the call is admitted. An `Err(message)` becomes
    /// `InvalidToolInput: Invalid input for tool '<path>': <message>`. Default: inputs pass through unchanged.
    fn decode_input(&self, _path: &str, input: Option<Json>) -> Result<Option<Json>, String> {
        Ok(input)
    }

    /// Runs before every tool call (including `search`); an error denies the call and the program catches it.
    fn tool_before(&self, _call: &ToolInvocation) -> BoxFuture<'static, Result<(), ToolCallError>> {
        Box::pin(async { Ok(()) })
    }

    /// Runs after every tool call that passed `tool_before`, with how it ended.
    fn tool_after(&self, _call: &ToolInvocation, _result: &CallResult) {}

    /// Runs before every call to an extension global; an error denies the call (the program catches the value).
    fn extension_before(&self, _call: &ExtensionInvocation) -> BoxFuture<'static, Result<(), HostValue>> {
        Box::pin(async { Ok(()) })
    }

    /// Runs after every extension global call that passed `extension_before`.
    fn extension_after(&self, _call: &ExtensionInvocation, _result: &CallResult) {}
}

/// A value crossing the extension boundary. Everything is copied in either direction.
#[derive(Clone)]
pub enum HostValue {
    Undefined,
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<HostValue>),
    /// A plain object with ordered keys.
    Object(Vec<(String, HostValue)>),
    Date(f64),
    RegExp { source: String, flags: String },
    Url(String),
    SearchParams(Vec<(String, String)>),
    Headers(Vec<(String, String)>),
    Map(Vec<(HostValue, HostValue)>),
    Set(Vec<HostValue>),
    Bytes(Vec<u8>),
    /// A Node `Buffer` (2.3.1): bytes that cross into the program as a `Buffer`, and that a program's `Buffer` crosses
    /// out as.
    Buffer(Vec<u8>),
    /// An error: its name, message, `cause`, and own enumerable fields.
    Error { name: String, message: String, cause: Option<Box<HostValue>>, fields: Vec<(String, HostValue)> },
    /// A host function; inside results it becomes a callable program function.
    Function(Arc<dyn HostFunction>),
    /// A host value the program cannot hold, described as "a symbol", "a bigint", "a Uint16Array", ...
    /// Crossing into the program throws `<label> produced <description>, which the program cannot hold.`
    Unrepresentable(String),
    /// A live host object (upstream builds these for a streaming `fetch` response): properties in order, each a
    /// value copied once, or an accessor read from the host on every access (non-enumerable, like upstream's
    /// `defineAccessor`); a key may be `Symbol.asyncIterator`. Crossing in, it becomes an ordinary object.
    Live(Vec<(HostKey, HostProperty)>),
}

/// A property key of a [`HostValue::Live`] object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostKey {
    String(String),
    AsyncIterator,
}

/// A property of a [`HostValue::Live`] object.
#[derive(Clone)]
pub enum HostProperty {
    Value(HostValue),
    Getter(Arc<dyn Fn() -> HostValue + Send + Sync>),
}

impl fmt::Debug for HostValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HostValue::Undefined => write!(f, "undefined"),
            HostValue::Null => write!(f, "null"),
            HostValue::Bool(b) => write!(f, "{b}"),
            HostValue::Number(n) => write!(f, "{n}"),
            HostValue::String(s) => write!(f, "{s:?}"),
            HostValue::Array(a) => f.debug_list().entries(a).finish(),
            HostValue::Object(o) => f.debug_map().entries(o.iter().map(|(k, v)| (k, v))).finish(),
            HostValue::Date(t) => write!(f, "Date({t})"),
            HostValue::RegExp { source, flags } => write!(f, "/{source}/{flags}"),
            HostValue::Url(u) => write!(f, "URL({u})"),
            HostValue::SearchParams(p) => write!(f, "URLSearchParams({p:?})"),
            HostValue::Headers(h) => write!(f, "Headers({h:?})"),
            HostValue::Map(m) => write!(f, "Map({m:?})"),
            HostValue::Set(s) => write!(f, "Set({s:?})"),
            HostValue::Bytes(b) => write!(f, "Uint8Array({b:?})"),
            HostValue::Buffer(b) => write!(f, "Buffer({b:?})"),
            HostValue::Error { name, message, .. } => write!(f, "{name}: {message}"),
            HostValue::Function(func) => write!(f, "[Function {}]", func.name()),
            HostValue::Unrepresentable(d) => write!(f, "<{d}>"),
            HostValue::Live(props) => f
                .debug_map()
                .entries(props.iter().map(|(k, p)| {
                    let value = match p {
                        HostProperty::Value(v) => format!("{v:?}"),
                        HostProperty::Getter(_) => "<getter>".to_string(),
                    };
                    (k, value)
                }))
                .finish(),
        }
    }
}

impl HostValue {
    pub fn str(s: impl Into<String>) -> HostValue {
        HostValue::String(s.into())
    }
    pub fn error(name: impl Into<String>, message: impl Into<String>) -> HostValue {
        HostValue::Error { name: name.into(), message: message.into(), cause: None, fields: Vec::new() }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            HostValue::String(s) => Some(s),
            _ => None,
        }
    }
    /// A field of a plain object.
    pub fn field(&self, key: &str) -> Option<&HostValue> {
        match self {
            HostValue::Object(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
}

/// What a host function call produces. `Err` values are thrown into the program (or reject its promise).
pub enum HostCall {
    /// A value now.
    Ready(Result<HostValue, HostValue>),
    /// Work still running, as a host `Promise`: the program gets a promise that settles with it.
    Pending(BoxFuture<'static, Result<HostValue, HostValue>>),
    /// Work the caller waits for (upstream 2.3.1, #64363: a synchronous global that returns an Effect blocks, as
    /// `fs.readFileSync` does): the program's current job stops until it ends, then gets its value or catches what it
    /// failed with, as from `Ready`; nothing else in the program runs meanwhile. From an asynchronous host function
    /// it is the same as `Pending`.
    Block(BoxFuture<'static, Result<HostValue, HostValue>>),
}

/// A host function callable from the program.
pub trait HostFunction: Send + Sync {
    /// The function's `name` as the program sees it.
    fn name(&self) -> &str;
    /// The function's `length`.
    fn length(&self) -> usize {
        0
    }
    fn call(&self, args: Vec<HostValue>) -> HostCall;
    /// Whether the global is asynchronous (upstream: an `async` function or one marked with `Extension.async`). A
    /// call to it always returns a promise, so a call the `extension.before` hook denies rejects that promise; a
    /// synchronous global's denied call throws, and the hook holds the program's current job.
    fn asynchronous(&self) -> bool {
        false
    }
    /// Whether the program may call it with `new`, as it would a class such as `Bun.Glob` (upstream 2.3.1
    /// `Extension.constructible`, #64472): `new f(...)` calls it exactly as `f(...)` does, and the program gets what it
    /// returns. Otherwise `new f(...)` is `TypeError: new f(...) is not supported; call f(...) without new instead.`
    fn constructible(&self) -> bool {
        false
    }
}

/// A host function from a closure, with upstream's options (see [`HostFunction`]):
///
/// ```
/// use oc_codemode::host::{HostCall, HostFn, HostValue};
/// let glob = HostFn::new("Glob", 1, |args| HostCall::Ready(Ok(args.into_iter().next().unwrap_or(HostValue::Undefined))))
///     .constructible()
///     .into_value();
/// ```
pub struct HostFn<F> {
    name: String,
    length: usize,
    asynchronous: bool,
    constructible: bool,
    f: F,
}

impl<F> HostFn<F>
where
    F: Fn(Vec<HostValue>) -> HostCall + Send + Sync + 'static,
{
    pub fn new(name: impl Into<String>, length: usize, f: F) -> Self {
        HostFn { name: name.into(), length, asynchronous: false, constructible: false, f }
    }
    /// Marks it asynchronous (upstream `Extension.async`).
    pub fn asynchronous(mut self) -> Self {
        self.asynchronous = true;
        self
    }
    /// Lets the program call it with `new` (upstream `Extension.constructible`).
    pub fn constructible(mut self) -> Self {
        self.constructible = true;
        self
    }
    pub fn into_arc(self) -> Arc<dyn HostFunction> {
        Arc::new(self)
    }
    pub fn into_value(self) -> HostValue {
        HostValue::Function(Arc::new(self))
    }
}

impl<F> HostFunction for HostFn<F>
where
    F: Fn(Vec<HostValue>) -> HostCall + Send + Sync,
{
    fn name(&self) -> &str {
        &self.name
    }
    fn length(&self) -> usize {
        self.length
    }
    fn call(&self, args: Vec<HostValue>) -> HostCall {
        (self.f)(args)
    }
    fn asynchronous(&self) -> bool {
        self.asynchronous
    }
    fn constructible(&self) -> bool {
        self.constructible
    }
}

/// A host function from a closure.
pub struct FnHostFunction<F> {
    pub name: String,
    pub length: usize,
    pub f: F,
}

impl<F> HostFunction for FnHostFunction<F>
where
    F: Fn(Vec<HostValue>) -> HostCall + Send + Sync,
{
    fn name(&self) -> &str {
        &self.name
    }
    fn length(&self) -> usize {
        self.length
    }
    fn call(&self, args: Vec<HostValue>) -> HostCall {
        (self.f)(args)
    }
}

/// An asynchronous host function from a closure (upstream `Extension.async`).
pub struct AsyncFnHostFunction<F>(pub FnHostFunction<F>);

impl<F> HostFunction for AsyncFnHostFunction<F>
where
    F: Fn(Vec<HostValue>) -> HostCall + Send + Sync,
{
    fn name(&self) -> &str {
        &self.0.name
    }
    fn length(&self) -> usize {
        self.0.length
    }
    fn call(&self, args: Vec<HostValue>) -> HostCall {
        (self.0.f)(args)
    }
    fn asynchronous(&self) -> bool {
        true
    }
}

/// An asynchronous host function value (see [`HostFunction::asynchronous`]).
pub fn host_fn_async(
    name: impl Into<String>,
    length: usize,
    f: impl Fn(Vec<HostValue>) -> HostCall + Send + Sync + 'static,
) -> HostValue {
    HostValue::Function(Arc::new(AsyncFnHostFunction(FnHostFunction { name: name.into(), length, f })))
}

pub fn host_fn(
    name: impl Into<String>,
    length: usize,
    f: impl Fn(Vec<HostValue>) -> HostCall + Send + Sync + 'static,
) -> HostValue {
    HostValue::Function(Arc::new(FnHostFunction { name: name.into(), length, f }))
}

/// Host values a program reaches directly as globals (upstream `Extension.make`): functions, or objects of
/// functions such as `fs` or `path`.
///
/// An implementation gives either `globals` (functions only) or `values` (any global: a [`HostValue::Function`], or
/// an object such as a [`HostValue::Object`] whose functions, at any depth of its plain objects, are called the same
/// way and named by their path, `fs.readFile`, in diagnostics and in the hooks' [`ExtensionInvocation::name`]; data
/// inside it crosses as a copy). Each global must be a function or an object, else `CodeMode::new` fails with
/// `Extension "<name>" global "<global>" must be a function or an object.`
///
/// A function among the globals, or inside a global object, never crosses back to the host: passed as an argument it
/// is left out like the program's own functions. A function inside a value a host function returned does: passed back
/// to any extension function, on its own or inside an argument, it arrives as the same `HostValue::Function` (the same
/// `Arc`, so `Arc::ptr_eq` tells which it was; upstream 2.3.1 `takesFunctions`, #64472). Tools never receive
/// functions.
pub trait Extension: Send + Sync {
    fn name(&self) -> &str;
    /// The extension's function globals.
    fn globals(&self) -> Vec<(String, Arc<dyn HostFunction>)> {
        Vec::new()
    }
    /// The extension's globals as values; by default its function [`Extension::globals`].
    fn values(&self) -> Vec<(String, HostValue)> {
        self.globals().into_iter().map(|(name, f)| (name, HostValue::Function(f))).collect()
    }
    /// Module names that load one of these globals, beyond its own name (upstream 2.3.2 `Extension.modules`,
    /// #64516): `("bun", "Bun")` makes `import { file } from "bun"` load `Bun`.
    fn modules(&self) -> Vec<(String, String)> {
        Vec::new()
    }
}

/// A simple extension from a list of globals.
pub struct SimpleExtension {
    pub name: String,
    pub globals: Vec<(String, Arc<dyn HostFunction>)>,
}

impl Extension for SimpleExtension {
    fn name(&self) -> &str {
        &self.name
    }
    fn globals(&self) -> Vec<(String, Arc<dyn HostFunction>)> {
        self.globals.clone()
    }
}

/// An extension from a list of global values, functions or objects (see [`Extension`]).
pub struct GlobalsExtension {
    pub name: String,
    pub globals: Vec<(String, HostValue)>,
    /// Module names that load one of these globals (see [`Extension::modules`]).
    pub modules: Vec<(String, String)>,
}

impl Extension for GlobalsExtension {
    fn name(&self) -> &str {
        &self.name
    }
    fn values(&self) -> Vec<(String, HostValue)> {
        self.globals.clone()
    }
    fn modules(&self) -> Vec<(String, String)> {
        self.modules.clone()
    }
}

/// A host without tools: every tool call fails (useful for programs that only compute).
pub struct NoTools;

impl Host for NoTools {
    fn call(&self, path: &str, _input: Option<Json>) -> BoxFuture<'static, Result<Option<Json>, ToolCallError>> {
        let message = format!("Tool '{path}' is unavailable.");
        Box::pin(async move { Err(ToolCallError::Failure(message)) })
    }
}

/// An outbound HTTP request (used by the `fetch` extension and OpenAPI tools).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    pub timeout: Option<std::time::Duration>,
}

/// A fully buffered HTTP response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    /// Final URL after redirects.
    pub url: String,
    pub status: u16,
    pub status_text: String,
    pub redirected: bool,
    /// Header names lowercased, in response order.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// A response body as it arrives: chunks, or the error that ended it (a program sees it as `Error: <message>`).
pub type BodyStream = futures::stream::BoxStream<'static, Result<Vec<u8>, String>>;

/// An HTTP response whose body is still arriving (the `fetch` extension's responses since 2.3.0).
pub struct HttpStreamingResponse {
    /// Final URL after redirects.
    pub url: String,
    pub status: u16,
    pub status_text: String,
    pub redirected: bool,
    /// Header names lowercased, in response order.
    pub headers: Vec<(String, String)>,
    /// Dropping it cancels the body.
    pub body: BodyStream,
}

impl From<HttpResponse> for HttpStreamingResponse {
    fn from(response: HttpResponse) -> HttpStreamingResponse {
        let body = response.body;
        let chunks: Vec<Result<Vec<u8>, String>> = if body.is_empty() { Vec::new() } else { vec![Ok(body)] };
        HttpStreamingResponse {
            url: response.url,
            status: response.status,
            status_text: response.status_text,
            redirected: response.redirected,
            headers: response.headers,
            body: Box::pin(futures::stream::iter(chunks)),
        }
    }
}

/// The host's HTTP transport. Errors are the message a program sees (thrown as a `TypeError`, like `fetch`).
pub trait HttpClient: Send + Sync {
    fn send(&self, request: HttpRequest) -> BoxFuture<'static, Result<HttpResponse, String>>;

    /// Sends a request and returns once the response head arrives; the body streams after. The default buffers the
    /// body with [`HttpClient::send`] and streams it as one chunk.
    fn send_streaming(&self, request: HttpRequest) -> BoxFuture<'static, Result<HttpStreamingResponse, String>> {
        let sent = self.send(request);
        Box::pin(async move { sent.await.map(HttpStreamingResponse::from) })
    }
}
