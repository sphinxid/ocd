//! opencode v2 HTTP API server — wire-compatible with `assets/openapi.json`.
//!
//! The engine is abstracted behind the generated [`Backend`] trait (one method per OpenAPI
//! operation). This crate owns transport concerns: routing, request decoding and 400 shapes,
//! envelopes, the error table, auth (Basic / `auth_token` / pairing cookie), CORS, readiness,
//! SSE framing, raw file reads, PTY WebSockets and web-UI serving. See `README.md`.

pub mod app;
pub mod auth;
pub mod backend;
pub mod context;
pub mod cors;
pub mod error;
pub mod fsread;
pub mod generated;
mod handlers;
pub mod ids;
pub mod location;
pub mod pty;
pub(crate) mod router;
pub mod server;
pub mod service;
pub mod sse;
pub mod support;
pub mod validate;
pub mod webui;

pub use app::Status;
pub use backend::*;
pub use context::{LocationRef, RequestContext, ServerMeta};
pub use error::ApiError;
pub use generated::{Backend, OPS, Op, OpMeta, Scope, Envelope};
pub use server::{RouterHandle, RunningServer, ServerOptions, router, serve};

/// The OpenAPI document served at `/openapi.json`.
pub const OPENAPI_JSON: &str = include_str!("../../../assets/openapi.json");
