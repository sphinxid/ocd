//! Built-in tools for the opencode Rust port (`packages/core/src/tool/**`).
//!
//! See `README.md` for the integration API (`Tool`, `ToolContext`, `ToolHost`, `Registry`).

pub mod file_access;
pub mod file_diff;
pub mod file_mutation;
pub mod formatter;
pub mod host;
pub mod html;
pub mod image;
pub mod jsdiff;
pub mod mime;
mod mime_db;
pub mod patch;
pub mod registry;
pub mod ripgrep;
pub mod shell;
pub mod tool;
pub mod tool_output;
pub mod tools;
pub mod util;
pub mod validate;
pub mod websearch;

pub use host::ToolHost;
pub use registry::{Registry, Snapshot};
pub use tool::{DefinitionContext, Tool, ToolContext, ToolDefinition, ToolError, ToolOptions, ToolResult};

#[cfg(test)]
mod tests;
