//! OpenTunnel client, wire-compatible with `@opentunnel/client` 0.6.1 and `@opentunnel/protocol` 0.6.1 (MIT,
//! anomalyco/opentunnel), which upstream opencode 2.2.2 uses for `pair --remote`. See README.md for the protocol.
//!
//! The device holds the tunnel's private key and certificate; the relay only forwards encrypted bytes. Identities
//! are stored where the SDK and the opentunnel CLI keep them, so a device shares one tunnel with both.

pub mod api;
pub mod bridge;
pub mod client;
pub mod csr;
#[cfg(any(test, feature = "fake"))]
pub mod fake;
pub mod names;
pub mod proxy;
pub mod proxy_protocol;
pub mod retry;
pub mod routes;
pub mod storage;
pub mod tunnel;
pub mod ws;

pub use api::{ApiError, DEFAULT_API, USER_AGENT};
pub use client::{Client, ClientOptions, ConnectOptions, Connection, Progress};
pub use storage::{Identity, Pending, Storage};
pub use tunnel::{AttachError, Event, Fatal, OnRouteConflict, Route, Routes};

/// What an error was caused by.
#[derive(Debug, Clone, PartialEq)]
pub enum Cause {
    /// `OpenTunnelAttachError`: the relay refused the bridge with a code that will not change on retry.
    Attach(String),
    /// A failed API call.
    Api(ApiError),
    Message(String),
}

impl std::fmt::Display for Cause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Cause::Attach(code) => write!(f, "{}", AttachError { code: code.clone() }),
            Cause::Api(e) => write!(f, "{e}"),
            Cause::Message(m) => f.write_str(m),
        }
    }
}

/// The SDK's errors.
#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// `OpenTunnelClientError`
    Client { message: String, cause: Option<Cause> },
    /// `OpenTunnelStorageError`
    Storage { message: String, cause: Option<String> },
    /// What the SDK would throw instead of failing (an unreadable identity file or certificate).
    Defect(String),
}

impl Error {
    pub(crate) fn storage(message: String, cause: String) -> Error {
        Error::Storage { message, cause: Some(cause) }
    }

    pub(crate) fn storage_message(message: String) -> Error {
        Error::Storage { message, cause: None }
    }

    pub fn message(&self) -> &str {
        match self {
            Error::Client { message, .. } | Error::Storage { message, .. } => message,
            Error::Defect(message) => message,
        }
    }

    /// The `_tag` of the SDK error.
    pub fn tag(&self) -> &'static str {
        match self {
            Error::Client { .. } => "OpenTunnelClientError",
            Error::Storage { .. } => "OpenTunnelStorageError",
            Error::Defect(_) => "Error",
        }
    }

    /// The bridge's `attach_error` code when attaching failed (`OpenTunnelClientError.code`), such as `bad_token`.
    pub fn code(&self) -> Option<&str> {
        match self {
            Error::Client { cause: Some(Cause::Attach(code)), .. } => Some(code),
            _ => None,
        }
    }

    /// Errors no retry can fix: a rejected token, a certificate the server will not issue, a proxy the bridge cannot
    /// use (such as a `socks5://` `HTTPS_PROXY`), or a defect. opencode's remote tunnel stops on these (`fatal` in
    /// `services/remote-tunnel.ts`, which matches `/unsupported proxy scheme|not a valid proxy URL/` in the message,
    /// plus defects, which skip its retry).
    pub fn is_fatal(&self) -> bool {
        match self {
            Error::Client { message, cause } => {
                matches!(cause, Some(Cause::Attach(_)))
                    || message.starts_with("Certificate issuance failed")
                    || message.contains("unsupported proxy scheme")
                    || message.contains("not a valid proxy URL")
            }
            Error::Storage { .. } => false,
            Error::Defect(_) => true,
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests;
