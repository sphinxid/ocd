//! Node `path` semantics used by the upstream modules (lexical, POSIX-style on Unix).

use std::path::{Component, Path, PathBuf};

/// `path.normalize` for absolute paths / `path.resolve(base, input)`.
pub fn normalize(input: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in input.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(Component::RootDir.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                let at_root = out.parent().is_none();
                if !at_root {
                    out.pop();
                } else if !out.has_root() {
                    out.push("..");
                }
            }
            Component::Normal(part) => out.push(part),
        }
    }
    if out.as_os_str().is_empty() { PathBuf::from(".") } else { out }
}

/// `path.resolve(base, input)`.
pub fn resolve(base: &Path, input: impl AsRef<Path>) -> PathBuf {
    let input = input.as_ref();
    if input.is_absolute() { normalize(input) } else { normalize(&base.join(input)) }
}

/// `path.relative(from, to)` for absolute paths, using `/` separators on Unix.
pub fn relative(from: &Path, to: &Path) -> String {
    let from = normalize(from);
    let to = normalize(to);
    let a: Vec<_> = from.components().collect();
    let b: Vec<_> = to.components().collect();
    let common = a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count();
    let mut parts: Vec<String> = Vec::new();
    for _ in common..a.len() {
        parts.push("..".to_string());
    }
    for component in &b[common..] {
        parts.push(component.as_os_str().to_string_lossy().into_owned());
    }
    parts.join(std::path::MAIN_SEPARATOR_STR)
}

/// `FSUtil.contains(parent, child)`.
pub fn contains(parent: &Path, child: &Path) -> bool {
    let result = relative(parent, child);
    let sep = std::path::MAIN_SEPARATOR;
    result.is_empty()
        || (!Path::new(&result).is_absolute() && result != ".." && !result.starts_with(&format!("..{sep}")))
}

/// `FSUtil.resolve`: absolute path, realpath'd when it exists.
pub fn real_or_resolved(input: &Path) -> PathBuf {
    let resolved = resolve(&std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")), input);
    std::fs::canonicalize(&resolved).unwrap_or(resolved)
}

/// `resolvePath` in `git.ts`: resolve a path printed by Git against `cwd`.
pub fn resolve_git_path(cwd: &Path, value: &str) -> PathBuf {
    let trimmed = value.trim_end_matches(['\r', '\n']);
    if trimmed.is_empty() {
        return cwd.to_path_buf();
    }
    resolve(cwd, trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_and_contains() {
        assert_eq!(relative(Path::new("/a/b"), Path::new("/a/b")), "");
        assert_eq!(relative(Path::new("/a/b"), Path::new("/a/b/c/d")), "c/d");
        assert_eq!(relative(Path::new("/a/b"), Path::new("/a/x")), "../x");
        assert!(contains(Path::new("/a/b"), Path::new("/a/b/..c")));
        assert!(!contains(Path::new("/a/b"), Path::new("/a/bc")));
        assert!(!contains(Path::new("/a/b"), Path::new("/a")));
        assert_eq!(resolve(Path::new("/a/b"), "../c/./d"), PathBuf::from("/a/c/d"));
        assert_eq!(resolve(Path::new("/a"), "/x/../y"), PathBuf::from("/y"));
        assert_eq!(resolve_git_path(Path::new("/r"), ".git\n"), PathBuf::from("/r/.git"));
        assert_eq!(resolve_git_path(Path::new("/r"), ""), PathBuf::from("/r"));
    }
}
