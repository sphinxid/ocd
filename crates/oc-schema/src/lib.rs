//! Domain schema for the opencode Rust port (mirrors `packages/schema`).

pub mod collate;
pub mod event;
pub mod id;
pub mod message;
pub mod types;

pub use message::Message;
pub use types::*;
