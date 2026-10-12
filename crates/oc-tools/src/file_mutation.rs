//! Mutation serialization and BOM-aware writes (`core/src/file-mutation.ts`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use tokio::sync::{Mutex, OwnedMutexGuard};

use crate::util::bom;

type LockMap = parking_lot::Mutex<HashMap<String, Arc<Mutex<()>>>>;

static TRANSACTION_LOCKS: LazyLock<LockMap> = LazyLock::new(Default::default);
static TARGET_LOCKS: LazyLock<LockMap> = LazyLock::new(Default::default);

fn lock_for(map: &LockMap, key: &str) -> Arc<Mutex<()>> {
    map.lock().entry(key.to_string()).or_insert_with(|| Arc::new(Mutex::new(()))).clone()
}

/// `FSUtil.resolve`: realpath when it exists, else the lexical path.
pub fn fs_resolve(path: &Path) -> String {
    let resolved = crate::util::normalize(path);
    std::fs::canonicalize(&resolved).unwrap_or(resolved).to_string_lossy().into_owned()
}

/// Guard for a mutation transaction.
pub struct TransactionGuard {
    _guards: Vec<OwnedMutexGuard<()>>,
}

/// `FileMutation.withLock(paths)`: process-wide keyed mutex over sorted, de-duplicated resolved paths.
pub async fn with_lock(paths: &[PathBuf]) -> TransactionGuard {
    let mut keys: Vec<String> = paths.iter().map(|p| fs_resolve(p)).collect();
    keys.sort();
    keys.dedup();
    let mut guards = Vec::with_capacity(keys.len());
    for key in keys {
        guards.push(lock_for(&TRANSACTION_LOCKS, &key).lock_owned().await);
    }
    TransactionGuard { _guards: guards }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteResult {
    pub target: String,
    pub resource: String,
    pub existed: bool,
}

impl WriteResult {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({"operation": "write", "target": self.target, "resource": self.resource, "existed": self.existed})
    }
}

/// Errors from the environment's file operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileError {
    NotFound,
    /// Path exists but is not the expected kind (`directory`, `symlink`, `other`, ...).
    WrongKind(String),
    Failed(String),
}

impl FileError {
    pub fn from_io(error: &std::io::Error, path: &Path) -> Self {
        match error.kind() {
            std::io::ErrorKind::NotFound => FileError::NotFound,
            std::io::ErrorKind::IsADirectory => FileError::WrongKind("directory".into()),
            _ => {
                if path.is_dir() {
                    FileError::WrongKind("directory".into())
                } else {
                    FileError::Failed(error.to_string())
                }
            }
        }
    }

    /// `patch.ts errorMessage`.
    pub fn patch_message(&self) -> String {
        match self {
            FileError::NotFound => "file does not exist".into(),
            FileError::WrongKind(kind) if kind == "directory" => "path is a directory".into(),
            FileError::WrongKind(kind) => format!("path is {kind}"),
            FileError::Failed(message) => message.clone(),
        }
    }
}

/// Read a regular file's bytes (`Environment.files.read`).
pub fn read_bytes(path: &Path) -> Result<Vec<u8>, FileError> {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_dir() => return Err(FileError::WrongKind("directory".into())),
        Ok(meta) if !meta.is_file() => return Err(FileError::WrongKind("other".into())),
        Ok(_) => {}
        Err(e) => return Err(FileError::from_io(&e, path)),
    }
    std::fs::read(path).map_err(|e| FileError::from_io(&e, path))
}

/// `FileMutation.readText`: decode (keeping BOM) then split.
pub fn read_text(path: &Path) -> Result<bom::Split, FileError> {
    Ok(bom::decode_bytes(&read_bytes(path)?))
}

/// Write bytes creating parent directories.
pub fn write_bytes(path: &Path, content: &[u8]) -> Result<(), FileError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| FileError::Failed(e.to_string()))?;
    }
    std::fs::write(path, content).map_err(|e| FileError::from_io(&e, path))
}

pub fn remove(path: &Path) -> Result<(), FileError> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => Err(FileError::WrongKind("directory".into())),
        Ok(_) => std::fs::remove_file(path).map_err(|e| FileError::from_io(&e, path)),
        Err(e) => Err(FileError::from_io(&e, path)),
    }
}

/// `FileMutation.syncTextBom`.
pub fn sync_text_bom(path: &Path, bom_wanted: bool) -> Result<String, FileError> {
    let (text, bytes) = bom::sync_bytes(&read_bytes(path)?, bom_wanted);
    if let Some(bytes) = bytes {
        write_bytes(path, &bytes)?;
    }
    Ok(text)
}

/// `FileMutation.write`.
pub async fn write(absolute: &Path, resource: &str, content: &[u8]) -> Result<WriteResult, FileError> {
    let key = absolute.to_string_lossy().into_owned();
    let _guard = lock_for(&TARGET_LOCKS, &key).lock_owned().await;
    let existed = std::fs::metadata(absolute).is_ok();
    write_bytes(absolute, content)?;
    Ok(WriteResult { target: key, resource: resource.to_string(), existed })
}

/// `FileMutation.writeTextPreservingBom`.
pub async fn write_text_preserving_bom(absolute: &Path, resource: &str, content: &str) -> Result<WriteResult, FileError> {
    let key = absolute.to_string_lossy().into_owned();
    let _guard = lock_for(&TARGET_LOCKS, &key).lock_owned().await;
    let next = bom::split(content);
    let current: Option<Vec<u8>> = match std::fs::File::open(absolute) {
        Ok(mut file) => {
            use std::io::Read;
            if std::fs::metadata(absolute).map(|m| m.is_dir()).unwrap_or(false) {
                return Err(FileError::WrongKind("directory".into()));
            }
            let mut buffer = [0u8; 3];
            let mut read = 0;
            while read < 3 {
                match file.read(&mut buffer[read..]) {
                    Ok(0) => break,
                    Ok(n) => read += n,
                    Err(e) => return Err(FileError::from_io(&e, absolute)),
                }
            }
            Some(buffer[..read].to_vec())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(FileError::from_io(&e, absolute)),
    };
    let keep = current.as_deref().is_some_and(bom::has) || next.bom;
    write_bytes(absolute, bom::join(&next.text, keep).as_bytes())?;
    Ok(WriteResult { target: key, resource: resource.to_string(), existed: current.is_some() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn preserves_existing_bom() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a/b.txt");
        let r = write_text_preserving_bom(&file, "a/b.txt", "hello").await.unwrap();
        assert!(!r.existed);
        std::fs::write(&file, "\u{FEFF}old").unwrap();
        let r = write_text_preserving_bom(&file, "a/b.txt", "new").await.unwrap();
        assert!(r.existed);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "\u{FEFF}new");
        let text = read_text(&file).unwrap();
        assert!(text.bom);
        assert_eq!(text.text, "new");
    }

    #[tokio::test]
    async fn locks_are_reentrant_free() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("x");
        let g = with_lock(&[a.clone(), a.clone()]).await;
        drop(g);
        let _g = with_lock(&[a]).await;
    }
}
