//! Shared utilities for the opencode Rust port (mirrors `packages/util`).

pub mod flock;
pub mod fs;
pub mod global;
pub mod hash;
pub mod http;
pub mod jsonc;
pub mod log;
pub mod panic;
pub mod trace;
pub mod variable;
pub mod version;

pub use global::Global;
