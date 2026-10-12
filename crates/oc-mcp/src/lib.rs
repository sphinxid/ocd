//! MCP (Model Context Protocol) client for the opencode Rust port.
//!
//! Port of `packages/core/src/mcp/**` plus the parts of `@modelcontextprotocol/client@2.0.0`
//! that upstream relies on. See `README.md` for the public API.

pub mod cli;
pub mod config;
pub mod connection;
pub mod error;
pub mod jsonrpc;
pub mod manager;
pub mod naming;
pub mod oauth;
pub mod protocol;
pub mod transport;
pub mod types;

pub use config::{
    LocalConfig, McpSection, OAuthConfig, OAuthSetting, Protocol, RemoteConfig, ServerConfig, TimeoutConfig,
    apply_codemode_defaults, merge_documents,
};
pub use connection::{ConnectError, Connection};
pub use error::McpError;
pub use manager::{ManagerError, ManagerOptions, McpManager};
pub use oauth::{AuthorizeHandle, McpCredentialStore, MemoryCredentialStore, StoredCredential};
pub use protocol::ElicitationHandler;
pub use types::*;
