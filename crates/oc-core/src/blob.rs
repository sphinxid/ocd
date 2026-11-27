//! Blob store (`core/src/blob-store.ts`, 2.4.1): files under `<data>/blobs/` named by their ID (`blob_<descending>`),
//! with no extension; their type is read from their first bytes. A blob is deleted a week after it was written,
//! whatever stored it: the server sweeps the folder at startup and hourly.

use std::path::{Path, PathBuf};
use std::time::Duration;

use futures::{Stream, StreamExt};
use tokio::io::AsyncWriteExt;

/// `BlobStore.DIRECTORY`, under the data folder.
pub const DIRECTORY: &str = "blobs";
/// `BlobStore.RETENTION`: how long a blob is kept after it was written.
pub const RETENTION: Duration = Duration::from_secs(7 * 24 * 3600);

/// `Blob.Info`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Info {
    pub id: String,
    pub mime: String,
    pub bytes: u64,
}

/// `BlobStore.Content`: a stored blob and where its file is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Content {
    pub id: String,
    pub path: PathBuf,
    pub mime: String,
    pub bytes: u64,
}

/// `Blob.ID`: strict, because the server names files after it.
pub fn valid_id(id: &str) -> bool {
    let Some(rest) = id.strip_prefix("blob_") else { return false };
    rest.len() == 26
        && rest[..12].bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && rest[12..].bytes().all(|b| b.is_ascii_alphanumeric())
}

/// `BlobID.create()`.
pub fn create_id() -> String {
    oc_schema::id::descending("blob")
}

pub fn directory(data: &Path) -> PathBuf {
    data.join(DIRECTORY)
}

/// `BlobStore.read`: the blob's file, size and sniffed type; `None` when there is no such blob.
pub async fn read(data: &Path, id: &str) -> Option<Content> {
    if !valid_id(id) {
        return None;
    }
    let path = directory(data).join(id);
    let meta = tokio::fs::metadata(&path).await.ok()?;
    if !meta.is_file() {
        return None;
    }
    let head = {
        use tokio::io::AsyncReadExt;
        let mut file = tokio::fs::File::open(&path).await.ok()?;
        let mut buf = Vec::with_capacity(64);
        (&mut file).take(64).read_to_end(&mut buf).await.ok()?;
        buf
    };
    let mime = oc_ai::media::detect_media_type(&head).unwrap_or("application/octet-stream").to_string();
    Some(Content { id: id.to_string(), path, mime, bytes: meta.len() })
}

/// `BlobStore.remove`.
pub async fn remove(data: &Path, id: &str) {
    if valid_id(id) {
        let _ = tokio::fs::remove_file(directory(data).join(id)).await;
    }
}

/// A failed blob write: the stream's own error, or the store's (`BlobStore.UnavailableError`).
#[derive(Debug, Clone, PartialEq)]
pub enum WriteError<E> {
    Stream(E),
    Unavailable(String),
}

/// `BlobStore.write`: stores the stream as a new blob, flushed to disk before it returns. A failed write leaves no
/// blob.
pub async fn write<E>(data: &Path, mut stream: impl Stream<Item = Result<bytes::Bytes, E>> + Unpin) -> Result<Info, WriteError<E>> {
    let id = create_id();
    let unavailable = |cause: std::io::Error| WriteError::Unavailable(format!("Could not store blob {id}: {cause}"));
    let dir = directory(data);
    tokio::fs::create_dir_all(&dir).await.map_err(unavailable)?;
    let path = dir.join(&id);
    let written = async {
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&path).await.map_err(unavailable)?;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(WriteError::Stream)?;
            file.write_all(&chunk).await.map_err(unavailable)?;
        }
        file.sync_all().await.map_err(unavailable)?;
        Ok(())
    }
    .await;
    if let Err(e) = written {
        let _ = tokio::fs::remove_file(&path).await;
        return Err(e);
    }
    let stored = read(data, &id).await.ok_or_else(|| WriteError::Unavailable(format!("Could not store blob {id}: it is gone")))?;
    Ok(Info { id, mime: stored.mime, bytes: stored.bytes })
}

/// One pass of the cleanup: blobs (`blob_*`) older than the retention are deleted.
pub async fn cleanup(data: &Path) {
    let mut files = Vec::new();
    if let Ok(mut entries) = tokio::fs::read_dir(directory(data)).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            if entry.file_name().to_string_lossy().starts_with("blob_") {
                files.push(entry.path());
            }
        }
    }
    oc_git::file_retention::cleanup(files, RETENTION).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunks(parts: &[&[u8]]) -> impl Stream<Item = Result<bytes::Bytes, String>> + Unpin {
        futures::stream::iter(parts.iter().map(|p| Ok(bytes::Bytes::copy_from_slice(p))).collect::<Vec<_>>())
    }

    #[test]
    fn ids_are_strict() {
        let id = create_id();
        assert!(valid_id(&id), "{id}");
        assert!(!valid_id("video_0123456789abABCDEFGHIJKLMN_0"));
        assert!(!valid_id("blob_../../etc/passwd"));
        assert!(!valid_id(&format!("{id}x")));
    }

    // blob-store.test.ts: "deletes blobs a week after they were written".
    #[tokio::test]
    async fn deletes_blobs_a_week_after_they_were_written() {
        let dir = tempfile::tempdir().unwrap();
        let old = write(dir.path(), chunks(&[&[1, 2, 3]])).await.unwrap();
        let fresh = write(dir.path(), chunks(&[&[4, 5], &[6]])).await.unwrap();
        assert_ne!(old.id, fresh.id);
        assert_eq!(fresh.bytes, 3);
        let week = std::time::SystemTime::now() - RETENTION - Duration::from_secs(1);
        std::fs::File::options().write(true).open(directory(dir.path()).join(&old.id)).unwrap().set_modified(week).unwrap();
        // The next server to start runs the cleanup.
        cleanup(dir.path()).await;
        assert!(read(dir.path(), &old.id).await.is_none());
        let kept = read(dir.path(), &fresh.id).await.unwrap();
        assert_eq!((kept.id.as_str(), kept.bytes), (fresh.id.as_str(), 3));
    }

    #[tokio::test]
    async fn a_failed_write_leaves_no_blob() {
        let dir = tempfile::tempdir().unwrap();
        let stream = futures::stream::iter(vec![Ok(bytes::Bytes::from_static(b"abc")), Err("lost".to_string())]);
        assert_eq!(write(dir.path(), stream).await, Err(WriteError::Stream("lost".to_string())));
        let mut entries = std::fs::read_dir(directory(dir.path())).unwrap();
        assert!(entries.next().is_none());
    }

    #[tokio::test]
    async fn sniffs_the_type() {
        let dir = tempfile::tempdir().unwrap();
        let mp4 = b"\x00\x00\x00\x18ftypisom\x00\x00\x02\x00isomiso2";
        let info = write(dir.path(), chunks(&[mp4])).await.unwrap();
        assert_eq!(info.mime, "video/mp4");
    }
}
