//! Port of `packages/core/src/file-retention.ts`.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use futures::StreamExt;

/// Retention used for `<data>/tool-output/tool_*` and `<data>/shell/<projectID>/sh_*.out`.
pub const DEFAULT_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Delete each listed file whose mtime is older than `now - retention` (8 at a time). Files that
/// cannot be inspected or removed are skipped.
pub async fn cleanup(files: impl IntoIterator<Item = PathBuf>, retention: Duration) {
    let cutoff = SystemTime::now().checked_sub(retention).unwrap_or(SystemTime::UNIX_EPOCH);
    futures::stream::iter(files)
        .for_each_concurrent(8, |file| async move {
            let Ok(info) = tokio::fs::metadata(&file).await else { return };
            let Ok(mtime) = info.modified() else { return };
            if mtime >= cutoff {
                return;
            }
            let _ = tokio::fs::remove_file(&file).await;
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn removes_only_expired_files() {
        let root = tempfile::tempdir().unwrap();
        let old = root.path().join("tool_old");
        let fresh = root.path().join("tool_fresh");
        std::fs::write(&old, "x").unwrap();
        std::fs::write(&fresh, "x").unwrap();
        let past = SystemTime::now() - Duration::from_secs(8 * 24 * 60 * 60);
        std::fs::File::options().write(true).open(&old).unwrap().set_modified(past).unwrap();
        cleanup(vec![old.clone(), fresh.clone(), root.path().join("missing")], DEFAULT_RETENTION).await;
        assert!(!old.exists());
        assert!(fresh.exists());
    }
}
