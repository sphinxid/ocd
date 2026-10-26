//! Filesystem helpers shared by config, instruction and location code (`packages/util/src/fs-util.ts`).

use std::io;
use std::path::{Path, PathBuf};

/// `FSUtil.isAccessDenied`: a permission denial, including sandbox denials (`EACCES`, `EPERM`).
pub fn access_denied(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::PermissionDenied
}

/// `FSUtil.resolve`: `path.resolve` then `realpath`, falling back to the unresolved path when the
/// target is missing or a sandbox denies the lookup (`ENOENT`, `ENOTDIR`, `EPERM`, `EACCES`).
pub fn resolve(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")).join(path)
    };
    std::fs::canonicalize(&absolute).unwrap_or(absolute)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denial_kinds() {
        assert!(access_denied(&io::Error::from_raw_os_error(13)));
        #[cfg(unix)]
        assert!(access_denied(&io::Error::from_raw_os_error(1)));
        assert!(!access_denied(&io::Error::from(io::ErrorKind::NotFound)));
    }
}
