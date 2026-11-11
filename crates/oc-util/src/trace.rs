//! Trace propagation headers of upstream's Effect HTTP client.
//!
//! Every request upstream sends through `effect/http/HttpClient` (model requests, `webfetch`,
//! web search providers, models.dev, remote skills, well-known manifests, provider auth calls)
//! runs inside an `http.client <METHOD>` span and carries `HttpTraceContext.toHeaders(span)`:
//!
//! ```text
//! b3:          <traceId>-<spanId>-1-<parentSpanId>
//! traceparent: 00-<traceId>-<spanId>-01
//! ```
//!
//! Effect's native tracer generates IDs with `Hex.random` (32 hex digits for a trace, 16 for a
//! span) and a span inherits its parent's trace ID. Upstream builds all its services inside one
//! long-lived root span, so a request outside a session turn shares one trace ID per process
//! (observed: across locations, and an incoming `traceparent` is not adopted). Since 2.2.7 each
//! session drain (`SessionRunner.drain`, `{ root: true }`) is one turn under a trace of its own,
//! whatever fiber woke it: its model requests, and the work it forks (title, tools, jobs,
//! background compaction), use that trace; a later drain of the same session starts another one.
//! [`root`] scopes a future in a new trace, [`inherit`] carries the current one into a spawned
//! task (forked fibers keep their parent's span).
//!
//! Each request gets a fresh client span whose parent is the innermost span around it: for most
//! requests a per-request span (a different parent for every request, including retries); work
//! that sends several requests inside one span (a compaction's summary and its reminder) shares
//! it, see [`span`]. Spans are always sampled. Requests upstream sends with plain `fetch` (MCP
//! transports) carry no trace headers.

use std::cell::RefCell;
use std::future::Future;
use std::sync::Arc;

use once_cell::sync::Lazy;

fn hex(len: usize) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(len);
    while out.len() < len {
        let _ = write!(out, "{:016x}", rand::random::<u64>());
    }
    out.truncate(len);
    out
}

static TRACE_ID: Lazy<String> = Lazy::new(|| hex(32));

/// The process-wide trace ID (32 lowercase hex digits): the trace of requests outside a session turn.
pub fn trace_id() -> &'static str {
    &TRACE_ID
}

/// The span context a task runs in: its trace and, inside a [`span`], the span every request is a child of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Context {
    pub trace_id: Arc<str>,
    pub parent_span_id: Option<Arc<str>>,
}

impl Context {
    fn root() -> Self {
        Self { trace_id: hex(32).into(), parent_span_id: None }
    }
}

tokio::task_local! {
    static CURRENT: RefCell<Option<Context>>;
}

/// The current task's span context; `None` outside a [`root`] (the process trace).
pub fn current() -> Option<Context> {
    CURRENT.try_with(|c| c.borrow().clone()).ok().flatten()
}

/// The current task's trace ID: the scoped one, else the process trace.
pub fn current_trace_id() -> String {
    current().map(|c| c.trace_id.to_string()).unwrap_or_else(|| TRACE_ID.clone())
}

/// Runs `f` as a root span (`Effect.fn(name, { root: true })`): a new trace, whatever the caller's.
pub fn root<F: Future>(f: F) -> impl Future<Output = F::Output> {
    CURRENT.scope(RefCell::new(Some(Context::root())), f)
}

/// Starts a new trace in place for the rest of the current [`root`] scope: a drain that returns and is called
/// again (`DrainResult.Moved`) runs under a new root. Outside a scope it does nothing. Work already forked keeps
/// the trace it captured.
pub fn reroot() {
    let _ = CURRENT.try_with(|c| {
        let mut c = c.borrow_mut();
        if c.is_some() {
            *c = Some(Context::root());
        }
    });
}

/// Runs `f` inside a new span of the current trace (`Effect.fn(name)` around work that sends several requests):
/// every request in it is a child of that one span.
/// The span starts when this is called.
pub fn span<F: Future>(f: F) -> impl Future<Output = F::Output> {
    let trace_id = current().map(|c| c.trace_id).unwrap_or_else(|| TRACE_ID.as_str().into());
    let context = Context { trace_id, parent_span_id: Some(hex(16).into()) };
    CURRENT.scope(RefCell::new(Some(context)), f)
}

/// `f` in the caller's span context, captured now: for a task spawned from it (a forked fiber keeps its parent's
/// span). Without one, `f` runs as is.
pub fn inherit<F: Future>(f: F) -> impl Future<Output = F::Output> {
    CURRENT.scope(RefCell::new(current()), f)
}

/// One `http.client` span: fresh span ID, under the current trace and span (see [`current`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientSpan {
    pub trace_id: String,
    pub span_id: String,
    pub parent_span_id: String,
}

impl ClientSpan {
    pub fn new() -> Self {
        let context = current();
        let trace_id = context.as_ref().map(|c| c.trace_id.to_string()).unwrap_or_else(|| TRACE_ID.clone());
        let parent_span_id = context.and_then(|c| c.parent_span_id).map(|p| p.to_string()).unwrap_or_else(|| hex(16));
        Self { trace_id, span_id: hex(16), parent_span_id }
    }

    /// `b3` single-header format (`HttpTraceContext.toHeaders`).
    pub fn b3(&self) -> String {
        format!("{}-{}-1-{}", self.trace_id, self.span_id, self.parent_span_id)
    }

    /// W3C `traceparent` (`HttpTraceContext.toHeaders`).
    pub fn traceparent(&self) -> String {
        format!("00-{}-{}-01", self.trace_id, self.span_id)
    }

    /// Both headers, in the order upstream sets them.
    pub fn headers(&self) -> [(&'static str, String); 2] {
        [("b3", self.b3()), ("traceparent", self.traceparent())]
    }
}

impl Default for ClientSpan {
    fn default() -> Self {
        Self::new()
    }
}

/// Headers for a new client span (one call per outgoing request).
pub fn headers() -> [(&'static str, String); 2] {
    ClientSpan::new().headers()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_hex(s: &str, len: usize) -> bool {
        s.len() == len && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }

    #[test]
    fn header_formats() {
        let span = ClientSpan::new();
        assert!(is_hex(&span.trace_id, 32));
        assert!(is_hex(&span.span_id, 16));
        assert!(is_hex(&span.parent_span_id, 16));
        assert_eq!(span.b3(), format!("{}-{}-1-{}", span.trace_id, span.span_id, span.parent_span_id));
        assert_eq!(span.traceparent(), format!("00-{}-{}-01", span.trace_id, span.span_id));
        let [(b3, _), (tp, _)] = span.headers();
        assert_eq!((b3, tp), ("b3", "traceparent"));
    }

    #[test]
    fn one_trace_per_process_fresh_spans_per_request() {
        let a = ClientSpan::new();
        let b = ClientSpan::new();
        assert_eq!(a.trace_id, b.trace_id);
        assert_eq!(a.trace_id, trace_id());
        assert_ne!(a.span_id, b.span_id);
        assert_ne!(a.parent_span_id, b.parent_span_id);
        assert_ne!(a.span_id, a.parent_span_id);
        assert!(current().is_none());
        assert_eq!(current_trace_id(), trace_id());
    }

    #[tokio::test]
    async fn roots_spans_and_inheritance() {
        let (first, second) = (root(async { ClientSpan::new() }).await, root(async { ClientSpan::new() }).await);
        // Each root is a trace of its own, neither the process trace.
        assert!(is_hex(&first.trace_id, 32));
        assert_ne!(first.trace_id, second.trace_id);
        assert_ne!(first.trace_id, trace_id());
        assert_eq!(ClientSpan::new().trace_id, trace_id());

        root(async {
            let trace = current_trace_id();
            let (a, b) = (ClientSpan::new(), ClientSpan::new());
            assert_eq!((a.trace_id.as_str(), b.trace_id.as_str()), (trace.as_str(), trace.as_str()));
            // Outside a span every request has a parent of its own.
            assert_ne!(a.parent_span_id, b.parent_span_id);
            // Requests in one span share it as their parent, under the same trace.
            let (c, d) = span(async { (ClientSpan::new(), ClientSpan::new()) }).await;
            assert_eq!(c.trace_id, trace);
            assert_eq!(c.parent_span_id, d.parent_span_id);
            assert_ne!(c.span_id, d.span_id);
            assert_ne!(c.parent_span_id, a.parent_span_id);
            let (e, f) = span(async { (ClientSpan::new(), ClientSpan::new()) }).await;
            assert_eq!(e.parent_span_id, f.parent_span_id);
            assert_ne!(e.parent_span_id, c.parent_span_id);

            // A spawned task keeps the trace it was forked under, also after the scope moves on.
            let forked = tokio::spawn(inherit(async { current_trace_id() }));
            let plain = tokio::spawn(async { current_trace_id() });
            reroot();
            let next = current_trace_id();
            assert_ne!(next, trace);
            assert_eq!(ClientSpan::new().trace_id, next);
            assert_eq!(forked.await.unwrap(), trace);
            assert_eq!(plain.await.unwrap(), trace_id());
        })
        .await;
        // Outside any scope: no-ops and the process trace.
        reroot();
        assert_eq!(inherit(async { current_trace_id() }).await, trace_id());
        assert_eq!(span(async { ClientSpan::new() }).await.trace_id, trace_id());
    }
}
