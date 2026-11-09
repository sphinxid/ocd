//! Filesystem helpers shared by config, instruction and location code (`packages/util/src/fs-util.ts`).

use std::io;
use std::path::{Path, PathBuf};

/// `FSUtil.isAccessDenied`: a permission denial, including sandbox denials (`EACCES`, `EPERM`).
pub fn access_denied(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::PermissionDenied
}

/// `fs.realpath` / `fs.realpathSync.native`: the canonical absolute path with symlinks resolved.
///
/// On Windows `std::fs::canonicalize` returns a verbatim path (`\\?\C:\dir`, `\\?\UNC\server\share`), which Node
/// never hands out (libuv strips the prefix from `GetFinalPathNameByHandle`) and which breaks wherever the path is
/// shown, turned into a `file://` URL or given to another program (`git worktree add` cannot create `//?/C:/...`).
/// This returns the same path in its ordinary form (`C:\dir`, `\\server\share`); elsewhere it is
/// `std::fs::canonicalize`. Rust's own file APIs add the prefix back where a long path needs it.
pub fn canonicalize(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    std::fs::canonicalize(path).map(simplified)
}

/// `canonicalize` for async callers.
pub async fn canonicalize_async(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    tokio::fs::canonicalize(path).await.map(simplified)
}

/// A Windows verbatim path (`\\?\C:\dir`, `\\?\UNC\server\share\dir`) in its ordinary form, the way libuv returns
/// real paths; any other path unchanged.
pub fn simplified(path: PathBuf) -> PathBuf {
    if !cfg!(windows) {
        return path;
    }
    let Some(text) = path.to_str() else { return path };
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    match text.strip_prefix(r"\\?\") {
        Some(rest) if rest.len() >= 2 && rest.as_bytes()[0].is_ascii_alphabetic() && rest.as_bytes()[1] == b':' => PathBuf::from(rest),
        _ => path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denial_kinds() {
        #[cfg(unix)]
        {
            assert!(access_denied(&io::Error::from_raw_os_error(13)));
            assert!(access_denied(&io::Error::from_raw_os_error(1)));
        }
        // ERROR_ACCESS_DENIED
        #[cfg(windows)]
        assert!(access_denied(&io::Error::from_raw_os_error(5)));
        assert!(!access_denied(&io::Error::from(io::ErrorKind::NotFound)));
    }

    #[test]
    fn canonical_paths_are_not_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let real = canonicalize(dir.path()).unwrap();
        assert!(real.is_absolute());
        assert!(!real.to_string_lossy().starts_with(r"\\?\"), "{}", real.display());
        assert_eq!(canonicalize(real.join(".")).unwrap(), real);
        if cfg!(windows) {
            assert_eq!(simplified(PathBuf::from(r"\\?\C:\a\b")), PathBuf::from(r"C:\a\b"));
            assert_eq!(simplified(PathBuf::from(r"\\?\UNC\server\share\a")), PathBuf::from(r"\\server\share\a"));
            assert_eq!(simplified(PathBuf::from(r"\\?\Volume{x}\a")), PathBuf::from(r"\\?\Volume{x}\a"));
        }
        assert_eq!(simplified(PathBuf::from("/a/b")), PathBuf::from("/a/b"));
    }
}
