//! Config variable substitution (`core/src/config/variable.ts`).
//! Applied to raw text before JSONC parsing.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct ConfigInvalidError {
    pub path: String,
    pub message: String,
}

pub struct Input<'a> {
    pub text: &'a str,
    /// Base directory for relative `{file:}` paths.
    pub dir: &'a Path,
    /// Source label (config file path or virtual label) used in errors.
    pub source: &'a str,
    pub env: Option<&'a HashMap<String, String>>,
}

pub fn substitute(input: Input) -> Result<String, ConfigInvalidError> {
    let env_re = regex::Regex::new(r"\{env:([^}]+)\}").unwrap();
    let text = env_re
        .replace_all(input.text, |c: &regex::Captures| {
            let name = &c[1];
            input
                .env
                .and_then(|e| e.get(name).cloned())
                .or_else(|| std::env::var(name).ok())
                .unwrap_or_default()
        })
        .into_owned();
    if !text.contains("{file:") {
        return Ok(text);
    }
    let file_re = regex::Regex::new(r"\{file:[^}]+\}").unwrap();
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for m in file_re.find_iter(&text) {
        out.push_str(&text[last..m.start()]);
        last = m.end();
        let token = m.as_str();
        let line_start = text[..m.start()].rfind('\n').map(|i| i + 1).unwrap_or(0);
        if text[line_start..m.start()].trim_start().starts_with("//") {
            out.push_str(token);
            continue;
        }
        let raw = &token["{file:".len()..token.len() - 1];
        let resolved: PathBuf = if let Some(rest) = raw.strip_prefix("~/") {
            dirs::home_dir().unwrap_or_default().join(rest)
        } else if Path::new(raw).is_absolute() {
            PathBuf::from(raw)
        } else {
            normalize(&input.dir.join(raw))
        };
        match std::fs::read_to_string(&resolved) {
            Ok(content) => {
                let json = serde_json::to_string(content.trim()).unwrap();
                out.push_str(&json[1..json.len() - 1]);
            }
            Err(e) => {
                let message = if e.kind() == std::io::ErrorKind::NotFound {
                    format!("bad file reference: \"{}\" {} does not exist", token, resolved.display())
                } else {
                    format!("bad file reference: \"{}\"", token)
                };
                return Err(ConfigInvalidError { path: input.source.to_string(), message });
            }
        }
    }
    out.push_str(&text[last..]);
    Ok(out)
}

/// Lexical normalization like `path.resolve` (no symlink resolution).
pub fn normalize(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}
