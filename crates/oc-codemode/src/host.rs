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
    /// An error: its name, message, `cause`, and own enumerable fields.
    Error { name: String, message: String, cause: Option<Box<HostValue>>, fields: Vec<(String, HostValue)> },
    /// A host function; inside results it becomes a callable program function.
    Function(Arc<dyn HostFunction>),
    /// A host value the program cannot hold, described as "a symbol", "a bigint", "a Uint16Array", ...
    /// Crossing into the program throws `<label> produced <description>, which the program cannot hold.`
    Unrepresentable(String),
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
            HostValue::Error { name, message, .. } => write!(f, "{name}: {message}"),
            HostValue::Function(func) => write!(f, "[Function {}]", func.name()),
            HostValue::Unrepresentable(d) => write!(f, "<{d}>"),
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

/// What a host function call produces: a value now (a synchronous host function), or a future (a host `Promise`,
/// which the program sees as a promise). `Err` values are thrown into the program.
pub enum HostCall {
    Ready(Result<HostValue, HostValue>),
    Pending(BoxFuture<'static, Result<HostValue, HostValue>>),
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

/// Host functions a program calls directly as globals (upstream `Extension.make`).
pub trait Extension: Send + Sync {
    fn name(&self) -> &str;
    fn globals(&self) -> Vec<(String, Arc<dyn HostFunction>)>;
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

/// The host's HTTP transport. Errors are the message a program sees (thrown as a `TypeError`, like `fetch`).
pub trait HttpClient: Send + Sync {
    fn send(&self, request: HttpRequest) -> BoxFuture<'static, Result<HttpResponse, String>>;
}
