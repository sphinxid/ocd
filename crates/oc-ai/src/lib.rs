//! `oc-ai`: provider-neutral LLM client for the opencode Rust port (port of `packages/ai` plus the
//! provider/catalog/credential parts of `packages/core`). See `README.md`.

pub mod json;
pub mod error;
pub mod media;
pub mod types;
pub mod compile;
pub mod response;
pub mod route;
pub mod protocols;
pub mod client;

pub use client::{Client, Compiled, LLMStream, StreamDriver};
pub use error::{AIError, ErrorKind, HttpContext, ProviderConfigurationError, ProviderError};
pub use media::{MediaAsset, MediaSource};
pub use response::{LLMResponse, ResponseState};
pub use route::{Endpoint, LanguageModel, Route};
pub use types::*;
pub use tokio_util::sync::CancellationToken;
pub mod providers;
pub mod catalog;
#[doc(hidden)]
pub mod testing;
pub mod credentials;
pub mod retry;
pub mod modalities;
