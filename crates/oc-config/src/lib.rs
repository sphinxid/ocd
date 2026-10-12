//! opencode location configuration (spec 01 §6): discovery, JSONC + variables, v1→v2
//! normalization, per-key folds, Markdown agents/commands/skills, managed policy and the
//! `cli.json` client configuration. See README.md for the public API.

pub mod cli;
pub mod decode;
pub mod discovery;
pub mod fold;
pub mod jsonc_edit;
pub mod legacy;
pub mod load;
pub mod markdown;
pub mod normalize;
pub mod policy;
pub mod redact;
pub mod schema;
pub mod sources;
pub mod template;

pub use normalize::{Diagnostic, DiagnosticKind, NormalizeResult, normalize};
pub use schema::{Entry, Info};

#[cfg(test)]
mod normalize_tests;
