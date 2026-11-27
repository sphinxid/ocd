//! Rendering: text primitives, markdown, highlighting, diffs, transcript and screens.

pub mod choice;
pub mod dialog;
#[cfg(test)]
mod dialog_nd_tests;
pub mod diff;
#[cfg(test)]
pub mod fixture;
pub mod form;
pub mod go_art;
pub mod go_upsell;
pub mod highlight;
pub mod images;
pub mod markdown;
pub mod mdoc;
pub mod prompt;
pub mod tab_pulse;
pub mod text;
pub mod transcript;
pub mod view_helpers;
