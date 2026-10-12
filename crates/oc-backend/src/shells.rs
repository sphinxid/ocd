//! `ShellSelect.list` (`core/src/shell/select.ts`) for `GET /api/config/shell` (unix).

use std::path::Path;

use indexmap::IndexSet;
use serde_json::{Value, json};

fn compatible(file: &str) -> bool {
    !matches!(oc_tools::shell::select::name(file).as_str(), "fish" | "nu")
}

/// `findExecutable(name, bin)`: `PATH` lookup, then the opencode bin directory.
fn find_executable(name: &str, bin: &Path) -> Option<String> {
    if let Ok(p) = which::which(name) {
        return Some(p.display().to_string());
    }
    let candidate = bin.join(name);
    candidate.is_file().then(|| candidate.display().to_string())
}

fn executable(file: &str, bin: &Path) -> Option<String> {
    if Path::new(file).is_absolute() {
        return Path::new(file).is_file().then(|| file.to_string());
    }
    find_executable(file, bin)
}

fn unix_shells() -> Vec<String> {
    match std::fs::read_to_string("/etc/shells") {
        Ok(text) if !text.is_empty() => {
            let set: IndexSet<String> =
                text.split('\n').filter(|l| !l.trim().is_empty() && !l.starts_with('#')).map(str::to_string).collect();
            set.into_iter().collect()
        }
        _ => vec!["/bin/bash".into(), "/bin/zsh".into(), "/bin/sh".into()],
    }
}

/// Available shells: `{path, name, acceptable}`.
pub fn list(bin: &Path) -> Vec<Value> {
    unix_shells()
        .into_iter()
        .filter(|s| executable(s, bin).is_some())
        .map(|s| {
            let n = oc_tools::shell::select::name(&s);
            let name = if executable(&n, bin).is_some() { n } else { s.clone() };
            json!({"path": s, "name": name, "acceptable": compatible(&s)})
        })
        .collect()
}
