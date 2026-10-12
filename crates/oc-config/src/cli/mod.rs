//! CLI / TUI client configuration (`cli.json`, spec 01 §6.11).

pub mod config;
pub mod keybind;
pub mod migrate;
pub mod schema;
pub mod v1;

pub use config::CliConfig;
