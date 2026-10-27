//! Release highlights after an update (`src/util/highlights.ts`, the startup effect in `app.tsx`).
//!
//! Upstream embeds a release's highlights only in stable release builds; ocd embeds none, so it
//! behaves like an upstream build without highlights: the dialog and `/highlights` never appear,
//! but the startup check still records the version as seen in the keyed store (`highlights`), so a
//! later build that has highlights shows them once.

use super::{App, Route};
use serde_json::{Value, json};

const STORAGE: &str = "highlights";

/// The highlights embedded in this build.
pub const ITEMS: &[&str] = &[];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Open the dialog and record the version.
    Show,
    /// Mark the version seen silently.
    Record,
    /// Leave the state untouched.
    None,
}

/// `decide`: only stable versions count; a first run with no state and no prompt history is
/// recorded silently.
pub fn decide(version: &str, count: usize, history: bool, seen: Option<&str>) -> Decision {
    let Some(current) = stable(version) else { return Decision::None };
    let seen = seen.and_then(stable);
    match seen {
        None if !history => return Decision::Record,
        Some(seen) if !newer(&current, &seen) => return Decision::None,
        _ => {}
    }
    if count > 0 { Decision::Show } else { Decision::Record }
}

fn stable(version: &str) -> Option<[u64; 3]> {
    let mut parts = version.split('.');
    let mut out = [0u64; 3];
    for slot in &mut out {
        let p = parts.next()?;
        if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        *slot = p.parse().ok()?;
    }
    parts.next().is_none().then_some(out)
}

fn newer(current: &[u64; 3], seen: &[u64; 3]) -> bool {
    current.iter().zip(seen).find(|(a, b)| a != b).is_some_and(|(a, b)| a > b)
}

impl App {
    /// The startup check: only into an idle home screen, never with `--prompt`, `--continue` or
    /// `--session`, and not when the TUI config sets `highlights: false`. Returns whether the
    /// dialog should open (never, while [`ITEMS`] is empty).
    pub fn claim_highlights(&mut self) -> bool {
        if self.config.bool("highlights") == Some(false) {
            return false;
        }
        if !matches!(self.route, Route::Home) || !self.dialogs.is_empty() || !self.editor.is_empty() {
            return false;
        }
        if self.opts.prompt.is_some() || self.opts.continue_last || self.opts.session.is_some() {
            return false;
        }
        if ["OPENCODE_DRIVE", "OPENCODE_STORY", "OPENCODE_ROUTE"].iter().any(|k| std::env::var_os(k).is_some()) {
            return false;
        }
        let history = self.history.path.as_ref().is_some_and(|p| p.exists());
        let version = self.opts.version.clone();
        let seen = |v: &Value| v.get("seen").and_then(|s| s.as_str()).map(str::to_string);
        if decide(&version, ITEMS.len(), history, seen(&self.keyed.get(STORAGE).unwrap_or(Value::Null)).as_deref()) == Decision::None {
            return false;
        }
        let mut action = Decision::None;
        self.keyed.update(STORAGE, |v| {
            if !v.is_object() {
                *v = json!({});
            }
            action = decide(&version, ITEMS.len(), history, seen(v).as_deref());
            if action != Decision::None {
                v["seen"] = json!(version);
            }
        });
        action == Decision::Show
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // `test/highlights.test.ts`.
    fn d(seen: Option<&str>) -> Decision {
        decide("2.0.27", 5, true, seen)
    }

    #[test]
    fn first_run_shows_highlights_to_existing_users_and_records_new_installs_silently() {
        assert_eq!(d(None), Decision::Show);
        assert_eq!(decide("2.0.27", 5, false, None), Decision::Record);
        assert_eq!(decide("2.0.27", 0, true, None), Decision::Record);
    }

    #[test]
    fn newer_versions_show_highlights_or_record_silently_when_there_are_none() {
        assert_eq!(d(Some("2.0.26")), Decision::Show);
        assert_eq!(decide("2.0.27", 5, false, Some("1.99.99")), Decision::Show);
        assert_eq!(decide("2.0.27", 0, true, Some("2.0.26")), Decision::Record);
    }

    #[test]
    fn the_same_version_downgrades_and_unstable_versions_do_nothing() {
        assert_eq!(d(Some("2.0.27")), Decision::None);
        assert_eq!(d(Some("2.0.28")), Decision::None);
        assert_eq!(decide("2.0.30", 5, true, Some("2.1.0")), Decision::None);
        assert_eq!(decide("0.0.0-dev-123", 5, true, None), Decision::None);
        assert_eq!(decide("local", 5, true, None), Decision::None);
    }

    #[test]
    fn startup_records_the_version_without_a_dialog() {
        let mut a = crate::app::tests::app();
        a.opts.version = "2.1.4".into();
        a.history.path = None;
        assert!(!a.claim_highlights());
        assert_eq!(a.keyed.get(STORAGE), Some(json!({ "seen": "2.1.4" })));
        assert!(a.dialogs.is_empty());
        a.opts.version = "2.1.5".into();
        a.config = crate::config::Config::from_value(json!({ "highlights": false }));
        assert!(!a.claim_highlights());
        assert_eq!(a.keyed.get(STORAGE), Some(json!({ "seen": "2.1.4" })));
    }
}
