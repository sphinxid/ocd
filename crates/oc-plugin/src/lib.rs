//! ocd plugin support: upstream `@opencode/plugin` server plugins (Promise and Effect style) run
//! unchanged in a Bun sidecar process that talks JSON-RPC with the Rust server (phase 1), and the
//! same sidecar runs AI SDK provider packages for `aisdk:` models (phase 2, [`aisdk`]).
//! See README.md for the architecture, protocol and the core integration checklist.

pub mod aisdk;
pub mod api;
pub mod bun;
pub mod host;
pub mod npm;
pub mod process;
pub mod protocol;
pub mod storage;
pub mod supervisor;
mod zip;

pub use aisdk::{AisdkError, AisdkHttp, AisdkParts, AisdkStreamRequest};
pub use api::{ApiError, ApiRequest, PluginApi, StorageEntry, StorageScan};
pub use host::{BlockedFn, HookError, ProgressFn, HostEvent, HostOptions, LoadRequest, LoadResult, PluginHost, RpcFailure, ToolError};
pub use process::CallError;
pub use protocol::{HookSpec, Installed, LocationRef, Operation};
pub use supervisor::{Entry, InternalPlugins, Loaded, Resolution};
