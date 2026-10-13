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
//! long-lived root span, so every outgoing request of a process shares one trace ID (observed:
//! across executions, sessions and locations, and an incoming `traceparent` is not adopted), while
//! each request gets a fresh client span whose parent is a per-request span (a different parent
//! for every request, including retries). Spans are always sampled. Requests upstream sends with
//! plain `fetch` (MCP transports) carry no trace headers.

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

/// The process-wide trace ID (32 lowercase hex digits).
pub fn trace_id() -> &'static str {
    &TRACE_ID
}

/// One `http.client` span: fresh span ID, fresh parent span ID, the process trace ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientSpan {
    pub trace_id: String,
    pub span_id: String,
    pub parent_span_id: String,
}

impl ClientSpan {
    pub fn new() -> Self {
        Self { trace_id: TRACE_ID.clone(), span_id: hex(16), parent_span_id: hex(16) }
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
    }
}
