//! Shared helpers.

pub mod bom;
pub mod js;
pub mod wildcard;

use std::path::{Component, Path, PathBuf};

/// `which(name, undefined, bin)`: search `PATH` followed by `<bin>`.
pub fn which(name: &str, bin: Option<&Path>) -> Option<PathBuf> {
    let mut paths = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect::<Vec<_>>()).unwrap_or_default();
    if let Some(bin) = bin {
        paths.push(bin.to_path_buf());
    }
    let joined = std::env::join_paths(paths).ok()?;
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    which::which_in(name, Some(joined), cwd).ok()
}

/// Node's `path.resolve(base, input)`: lexical, no symlink resolution.
pub fn resolve(base: &Path, input: &str) -> PathBuf {
    let joined = if Path::new(input).is_absolute() { PathBuf::from(input) } else { base.join(input) };
    normalize(&joined)
}

/// Lexically normalize `.`/`..` components (like `path.normalize` on an absolute path),
/// dropping a trailing separator.
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(p) => out.push(p.as_os_str()),
            Component::RootDir => out.push(Component::RootDir.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(part) => out.push(part),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// Node's `path.relative(from, to)` for absolute normalized paths.
pub fn relative(from: &Path, to: &Path) -> String {
    let from = normalize(from);
    let to = normalize(to);
    let a: Vec<_> = from.components().collect();
    let b: Vec<_> = to.components().collect();
    let mut common = 0;
    while common < a.len() && common < b.len() && a[common] == b[common] {
        common += 1;
    }
    let mut parts: Vec<String> = Vec::new();
    for _ in common..a.len() {
        parts.push("..".into());
    }
    for c in &b[common..] {
        parts.push(c.as_os_str().to_string_lossy().into_owned());
    }
    parts.join(std::path::MAIN_SEPARATOR_STR)
}

/// `FSUtil.contains(parent, child)`.
pub fn contains(parent: &Path, child: &Path) -> bool {
    let rel = relative(parent, child);
    rel.is_empty() || (!Path::new(&rel).is_absolute() && rel != ".." && !rel.starts_with(&format!("..{}", std::path::MAIN_SEPARATOR)))
}

/// `path.dirname`.
pub fn dirname(path: &Path) -> PathBuf {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        Some(_) => PathBuf::from("."),
        None => path.to_path_buf(),
    }
}

/// `path.dirname` on a display string (keeps `.` for bare names).
pub fn dirname_str(path: &str) -> String {
    let p = Path::new(path);
    match p.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_string_lossy().into_owned(),
        Some(_) => ".".into(),
        None => path.to_string(),
    }
}

/// `path.join(a, b)` on strings (normalizes; `join(".", x)` → `x`).
pub fn join_str(a: &str, b: &str) -> String {
    let joined = if a.is_empty() { b.to_string() } else { format!("{a}/{b}") };
    let normalized = normalize_relative(&joined);
    normalized
}

/// `path.normalize` for possibly-relative strings.
pub fn normalize_relative(input: &str) -> String {
    let absolute = input.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in input.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|p| *p != "..") {
                    parts.pop();
                } else if !absolute {
                    parts.push("..");
                }
            }
            other => parts.push(other),
        }
    }
    let body = parts.join("/");
    if absolute {
        format!("/{body}")
    } else if body.is_empty() {
        ".".into()
    } else {
        body
    }
}

/// `path.basename`.
pub fn basename(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    trimmed.rsplit('/').next().unwrap_or("").to_string()
}

pub fn slash(value: &str) -> String {
    value.replace('\\', "/")
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        assert_eq!(resolve(Path::new("/a/b"), "../c"), PathBuf::from("/a/c"));
        assert_eq!(resolve(Path::new("/a/b"), "/x/./y/"), PathBuf::from("/x/y"));
        assert_eq!(relative(Path::new("/a/b"), Path::new("/a/b/c/d")), "c/d");
        assert_eq!(relative(Path::new("/a/b"), Path::new("/a/x")), "../x");
        assert_eq!(relative(Path::new("/a/b"), Path::new("/a/b")), "");
        assert!(contains(Path::new("/a/b"), Path::new("/a/b/c")));
        assert!(contains(Path::new("/a/b"), Path::new("/a/b")));
        assert!(!contains(Path::new("/a/b"), Path::new("/a/bc")));
        assert_eq!(join_str(".", "x.txt"), "x.txt");
        assert_eq!(join_str("src", "a.ts"), "src/a.ts");
        assert_eq!(dirname_str("a.txt"), ".");
        assert_eq!(dirname_str("src/a.txt"), "src");
        assert_eq!(basename("src/a.txt"), "a.txt");
    }
}
