//! Filesystem helpers shared by config, instruction and location code (`packages/util/src/fs-util.ts`).

use std::io;

/// `FSUtil.isAccessDenied`: a permission denial, including sandbox denials (`EACCES`, `EPERM`).
pub fn access_denied(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::PermissionDenied
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
