//! Error model mirroring the subset of `@modelcontextprotocol/client` error classes that
//! upstream inspects (`SdkError`, `SdkHttpError`, `ProtocolError`, `UnauthorizedError`,
//! `OAuthError`, network `TypeError`s) plus the `describe`/`isTransient` helpers from
//! `packages/core/src/mcp/client.ts`.

use serde_json::Value;

/// `SdkErrorCode` string values (verbatim from the SDK).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SdkErrorCode {
    NotConnected,
    AlreadyConnected,
    NotInitialized,
    CapabilityNotSupported,
    RequestTimeout,
    ConnectionClosed,
    SendFailed,
    InvalidResult,
    UnsupportedResultType,
    InputRequiredRoundsExceeded,
    ListPaginationExceeded,
    MethodNotSupportedByProtocolVersion,
    EraNegotiationFailed,
    ClientHttpNotImplemented,
    ClientHttpAuthentication,
    ClientHttpForbidden,
    ClientHttpUnexpectedContent,
    ClientHttpFailedToOpenStream,
    ClientHttpFailedToTerminateSession,
}

impl SdkErrorCode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NotConnected => "NOT_CONNECTED",
            Self::AlreadyConnected => "ALREADY_CONNECTED",
            Self::NotInitialized => "NOT_INITIALIZED",
            Self::CapabilityNotSupported => "CAPABILITY_NOT_SUPPORTED",
            Self::RequestTimeout => "REQUEST_TIMEOUT",
            Self::ConnectionClosed => "CONNECTION_CLOSED",
            Self::SendFailed => "SEND_FAILED",
            Self::InvalidResult => "INVALID_RESULT",
            Self::UnsupportedResultType => "UNSUPPORTED_RESULT_TYPE",
            Self::InputRequiredRoundsExceeded => "INPUT_REQUIRED_ROUNDS_EXCEEDED",
            Self::ListPaginationExceeded => "LIST_PAGINATION_EXCEEDED",
            Self::MethodNotSupportedByProtocolVersion => "METHOD_NOT_SUPPORTED_BY_PROTOCOL_VERSION",
            Self::EraNegotiationFailed => "ERA_NEGOTIATION_FAILED",
            Self::ClientHttpNotImplemented => "CLIENT_HTTP_NOT_IMPLEMENTED",
            Self::ClientHttpAuthentication => "CLIENT_HTTP_AUTHENTICATION",
            Self::ClientHttpForbidden => "CLIENT_HTTP_FORBIDDEN",
            Self::ClientHttpUnexpectedContent => "CLIENT_HTTP_UNEXPECTED_CONTENT",
            Self::ClientHttpFailedToOpenStream => "CLIENT_HTTP_FAILED_TO_OPEN_STREAM",
            Self::ClientHttpFailedToTerminateSession => "CLIENT_HTTP_FAILED_TO_TERMINATE_SESSION",
        }
    }
}

/// JSON-RPC / MCP protocol error codes used by the client.
pub mod codes {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;
    pub const URL_ELICITATION_REQUIRED: i64 = -32042;
    pub const UNSUPPORTED_PROTOCOL_VERSION: i64 = -32022;
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum McpError {
    /// `SdkError` (non-HTTP).
    #[error("{message}")]
    Sdk { code: SdkErrorCode, message: String },
    /// `SdkHttpError`: carries the HTTP status and (when read) body text.
    #[error("{message}")]
    Http {
        code: SdkErrorCode,
        message: String,
        status: u16,
        status_text: String,
        text: Option<String>,
    },
    /// `ProtocolError`: a JSON-RPC error response from the server.
    #[error("{message}")]
    Protocol { code: i64, message: String, data: Option<Value> },
    /// `UnsupportedProtocolVersionError`.
    #[error("{message}")]
    UnsupportedProtocolVersion { message: String, supported: Vec<String>, requested: String },
    /// SDK `UnauthorizedError` (default message `Unauthorized`).
    #[error("{0}")]
    Unauthorized(String),
    /// `OAuthError` (code is the OAuth `error` value).
    #[error("{message}")]
    OAuth { code: String, message: String },
    /// A fetch-level network failure (a `TypeError` carrying `code` upstream). Transient.
    #[error("{message}")]
    Network { message: String, code: Option<String> },
    /// `MCP server session expired: <server>` (`MCP.SessionExpiredError`).
    #[error("MCP server session expired: {server}")]
    SessionExpired { server: String },
    /// Request aborted by the caller (`AbortError`).
    #[error("{0}")]
    Aborted(String),
    /// Any other `Error`.
    #[error("{0}")]
    Other(String),
}

impl McpError {
    pub fn other(message: impl Into<String>) -> Self {
        Self::Other(message.into())
    }

    pub fn sdk(code: SdkErrorCode, message: impl Into<String>) -> Self {
        Self::Sdk { code, message: message.into() }
    }

    pub fn connection_closed() -> Self {
        Self::sdk(SdkErrorCode::ConnectionClosed, "Connection closed")
    }

    pub fn request_timeout() -> Self {
        Self::sdk(SdkErrorCode::RequestTimeout, "Request timed out")
    }

    pub fn is_connection_closed(&self) -> bool {
        matches!(self, Self::Sdk { code: SdkErrorCode::ConnectionClosed, .. })
    }

    pub fn http_status(&self) -> Option<u16> {
        match self {
            Self::Http { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// JS `Error.name`, used where upstream stringifies an error (`String(error)`).
    pub fn name(&self) -> &'static str {
        match self {
            Self::Sdk { .. } => "SdkError",
            Self::Http { .. } => "SdkHttpError",
            Self::Protocol { .. } | Self::UnsupportedProtocolVersion { .. } => "ProtocolError",
            Self::Unauthorized(_) => "UnauthorizedError",
            Self::OAuth { .. } => "OAuthError",
            Self::Network { .. } => "TypeError",
            Self::SessionExpired { .. } => "MCP.SessionExpiredError",
            Self::Aborted(_) => "AbortError",
            Self::Other(_) => "Error",
        }
    }

    /// `String(error)` in JS: `<name>: <message>`.
    pub fn to_js_string(&self) -> String {
        format!("{}: {}", self.name(), self)
    }

    /// `describe` (`client.ts:41-50`): HTTP statuses and network error codes live on error
    /// properties and are lost once flattened to a message, so append them.
    pub fn describe(&self) -> String {
        let message = self.to_string();
        let detail = match self {
            Self::Http { status, .. } => Some(format!("HTTP {status}")),
            Self::Network { code: Some(code), .. } => Some(code.clone()),
            _ => None,
        };
        match detail {
            Some(detail) if !message.contains(&detail) => format!("{message} ({detail})"),
            _ => message,
        }
    }

    /// `isTransient` (`client.ts:55-58`): 408, 429, >=500 except 501, or a network failure.
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Http { status, .. } => *status == 408 || *status == 429 || (*status >= 500 && *status != 501),
            Self::Network { code: Some(_), .. } => true,
            _ => false,
        }
    }

    pub fn from_rpc_error(code: i64, message: String, data: Option<Value>) -> Self {
        if code == codes::UNSUPPORTED_PROTOCOL_VERSION {
            if let Some(Value::Object(obj)) = &data {
                if let (Some(Value::Array(supported)), Some(Value::String(requested))) =
                    (obj.get("supported"), obj.get("requested"))
                {
                    return Self::UnsupportedProtocolVersion {
                        message,
                        supported: supported.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
                        requested: requested.clone(),
                    };
                }
            }
        }
        Self::Protocol { code, message, data }
    }

    /// Converts a reqwest failure into the fetch-shaped error upstream sees.
    pub fn from_reqwest(error: &reqwest::Error) -> Self {
        let code = if error.is_connect() {
            Some("ConnectionRefused".to_string())
        } else if error.is_timeout() {
            Some("ETIMEDOUT".to_string())
        } else if error.is_request() || error.is_body() || error.is_decode() {
            Some("ECONNRESET".to_string())
        } else {
            None
        };
        let message = if error.is_connect() {
            "Unable to connect. Is the computer able to access the url?".to_string()
        } else {
            let mut text = error.to_string();
            let mut source = std::error::Error::source(error);
            while let Some(inner) = source {
                text = format!("{text}: {inner}");
                source = inner.source();
            }
            text
        };
        Self::Network { message, code }
    }
}

pub type Result<T, E = McpError> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_appends_http_status_once() {
        let error = McpError::Http {
            code: SdkErrorCode::ClientHttpNotImplemented,
            message: "Error POSTing to endpoint: nope".into(),
            status: 404,
            status_text: "Not Found".into(),
            text: Some("nope".into()),
        };
        assert_eq!(error.describe(), "Error POSTing to endpoint: nope (HTTP 404)");
        let error = McpError::Http {
            code: SdkErrorCode::ClientHttpNotImplemented,
            message: "already HTTP 500 here".into(),
            status: 500,
            status_text: String::new(),
            text: None,
        };
        assert_eq!(error.describe(), "already HTTP 500 here");
    }

    #[test]
    fn transient_classification() {
        let http = |status| McpError::Http {
            code: SdkErrorCode::ClientHttpNotImplemented,
            message: String::new(),
            status,
            status_text: String::new(),
            text: None,
        };
        assert!(http(408).is_transient());
        assert!(http(429).is_transient());
        assert!(http(500).is_transient());
        assert!(!http(501).is_transient());
        assert!(http(503).is_transient());
        assert!(!http(404).is_transient());
        assert!(McpError::Network { message: "x".into(), code: Some("ECONNRESET".into()) }.is_transient());
        assert!(!McpError::other("x").is_transient());
    }
}
