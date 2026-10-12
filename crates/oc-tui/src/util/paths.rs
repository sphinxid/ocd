//! XDG roots (`util/src/global-roots.ts`) used by the TUI for its local state.

use std::path::PathBuf;

fn env_dir(var: &str) -> Option<PathBuf> {
    std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from)
}

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

/// `$XDG_CONFIG_HOME/opencode` or `~/.config/opencode` (overridable by `OPENCODE_CONFIG_DIR`).
pub fn config_dir() -> PathBuf {
    if let Some(p) = env_dir("OPENCODE_CONFIG_DIR") {
        return p;
    }
    env_dir("XDG_CONFIG_HOME").unwrap_or_else(|| home().join(".config")).join("opencode")
}

pub fn state_dir() -> PathBuf {
    env_dir("XDG_STATE_HOME").unwrap_or_else(|| home().join(".local").join("state")).join("opencode")
}

pub fn data_dir() -> PathBuf {
    env_dir("XDG_DATA_HOME").unwrap_or_else(|| home().join(".local").join("share")).join("opencode")
}

pub fn cache_dir() -> PathBuf {
    env_dir("XDG_CACHE_HOME").unwrap_or_else(|| home().join(".cache")).join("opencode")
}

/// Atomically write a file (tmp + rename).
pub fn write_atomic(path: &std::path::Path, content: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!(
        "{}.{}.tmp",
        std::process::id(),
        crate::util::id_body(false)
    ));
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path)
}
