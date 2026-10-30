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

/// `path.resolve(p)` on POSIX: absolute against the working directory, `.` and `..` resolved lexically.
pub fn resolve(p: &str) -> String {
    let base = if p.starts_with('/') { String::new() } else { std::env::current_dir().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default() };
    let mut parts: Vec<&str> = Vec::new();
    for seg in base.split('/').chain(p.split('/')) {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    format!("/{}", parts.join("/"))
}

/// `pathToFileURL(p).href` as Bun builds it: the resolved path with every byte outside
/// `A-Za-z0-9-._/;=@:&$+,!*()'` percent-encoded.
pub fn path_to_file_url(p: &str) -> String {
    let mut out = String::from("file://");
    let resolved = resolve(p);
    let trailing = p.ends_with('/') && resolved != "/";
    for b in resolved.bytes() {
        if b.is_ascii_alphanumeric() || b"-._/;=@:&$+,!*()'".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    if trailing {
        out.push('/');
    }
    out
}

/// `promptFilesForServer`: a pasted attachment keeps its captured bytes (a local server may not have the
/// terminal's access to the file); its source path goes along as `source` only for a local server.
pub fn prompt_files_for_server(files: Vec<serde_json::Value>, local_server: bool) -> Vec<serde_json::Value> {
    files
        .into_iter()
        .map(|mut f| {
            let path = f.as_object_mut().and_then(|o| o.remove("path"));
            if let Some(p) = path.as_ref().and_then(|p| p.as_str()).filter(|p| !p.is_empty())
                && local_server
            {
                f["source"] = serde_json::json!({ "type": "uri", "uri": path_to_file_url(p) });
            }
            f
        })
        .collect()
}

#[cfg(test)]
mod url_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn file_urls_encode_like_bun() {
        assert_eq!(
            path_to_file_url("/tmp/a b[]^|%#?{}`\"<>ë;=@:&$+,!*()~'\t"),
            "file:///tmp/a%20b%5B%5D%5E%7C%25%23%3F%7B%7D%60%22%3C%3E%C3%AB;=@:&$+,!*()%7E'%09"
        );
        assert_eq!(path_to_file_url("/tmp/x\\y"), "file:///tmp/x%5Cy");
        assert_eq!(path_to_file_url("/tmp/a/../b/./c/"), "file:///tmp/b/c/");
        assert_eq!(resolve("/a/b/../../.."), "/");
    }

    #[test]
    fn keeps_pasted_bytes_with_source_paths_only_for_a_local_server() {
        let mention = json!({ "start": 0, "end": 9, "text": "[Image 1]" });
        let files = vec![
            json!({ "uri": "data:image/png;base64,AQID", "mention": mention, "name": "scrëen shot.png", "path": "/tmp/t/scrëen shot.png" }),
            json!({ "uri": "data:image/png;base64,Cg==", "name": "clipboard" }),
        ];
        assert_eq!(
            prompt_files_for_server(files.clone(), true),
            vec![
                json!({ "uri": "data:image/png;base64,AQID", "mention": mention, "name": "scrëen shot.png", "source": { "type": "uri", "uri": "file:///tmp/t/scr%C3%ABen%20shot.png" } }),
                json!({ "uri": "data:image/png;base64,Cg==", "name": "clipboard" }),
            ]
        );
        assert_eq!(
            prompt_files_for_server(files, false),
            vec![
                json!({ "uri": "data:image/png;base64,AQID", "mention": mention, "name": "scrëen shot.png" }),
                json!({ "uri": "data:image/png;base64,Cg==", "name": "clipboard" }),
            ]
        );
    }
}
