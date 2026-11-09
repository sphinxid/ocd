//! Stored media references (`experimental.media_blob`, #60116): `util/media.ts` (`createMediaCache`,
//! `isMissingMedia`), `prompt/codec.ts` (`restoredPromptInput`) and `prompt/restore.ts` (`usePromptRestore`).
//!
//! The transcript store reads messages and the inbox with `X-OpenCode-Capabilities: experimental-media-uri`, so a
//! server that keeps media in blobs sends `media:<hash>` refs instead of inline bytes. A ref's bytes are read once
//! (`GET /api/experimental/media/<hash>`) and cached by hash; restoring a sent prompt reads them back into `data:`
//! URIs first.

use std::collections::HashMap;
use std::sync::Arc;

use base64::Engine as _;
use parking_lot::Mutex;
use serde_json::Value;

use super::{App, Toast};
use crate::api::Client;
use crate::api::types::Message;
use crate::state::PromptInfo;

/// The blob hash of a `media:<64 lowercase hex>` ref (`mediaHashFromUri`).
pub fn media_hash(uri: &str) -> Option<&str> {
    let hash = uri.strip_prefix("media:")?;
    (hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))).then_some(hash)
}

/// `createMediaCache`: bytes by hash; a hash names its bytes, so entries stay valid across reconnects. Concurrent
/// reads of one hash share a request; a failed read is not cached.
#[derive(Default)]
pub struct MediaCache {
    cells: Mutex<HashMap<String, Arc<tokio::sync::OnceCell<Arc<Vec<u8>>>>>>,
}

impl MediaCache {
    /// The cached bytes of a ref, if read already.
    pub fn peek(&self, uri: &str) -> Option<Arc<Vec<u8>>> {
        let hash = media_hash(uri)?;
        self.cells.lock().get(hash).and_then(|c| c.get().cloned())
    }

    pub async fn read(&self, client: &Client, uri: &str) -> anyhow::Result<Arc<Vec<u8>>> {
        let Some(hash) = media_hash(uri) else { anyhow::bail!("Not a media ref: {uri}") };
        let cell = self.cells.lock().entry(hash.to_string()).or_default().clone();
        let bytes = cell.get_or_try_init(|| async { client.media_get(hash).await.map(Arc::new) }).await?;
        Ok(bytes.clone())
    }
}

/// `isMissingMedia`: the server answered that the blob is gone (404 `FileNotFoundError`), as opposed to a failure
/// where it may still exist.
pub fn is_missing_media(error: &anyhow::Error) -> bool {
    error.downcast_ref::<crate::api::ApiError>().is_some_and(|e| e.tag == "FileNotFoundError")
}

/// What a restore read gave for one file.
enum Read {
    Bytes(Arc<Vec<u8>>),
    Missing,
}

impl App {
    /// `usePromptRestore`: a sent prompt for the composer, with its `media:` images read back as `data:` URIs.
    /// Reading first means a failed read changes nothing, and the draft owns its bytes before the server releases
    /// the blob. An image the server no longer has is left out with a warning; `allow_missing: false` (a fork)
    /// fails instead. Any other failed read fails, since that image may still exist. Without refs `then` runs now.
    pub fn restore_prompt(
        &mut self,
        message: &Message,
        allow_missing: bool,
        then: impl FnOnce(&mut App, anyhow::Result<PromptInfo>) + Send + 'static,
    ) {
        let prompt = super::restore::restored_prompt(message);
        let refs: Vec<(usize, String)> = prompt
            .files
            .iter()
            .enumerate()
            .filter_map(|(i, f)| f.get("uri").and_then(Value::as_str).filter(|u| media_hash(u).is_some()).map(|u| (i, u.to_string())))
            .collect();
        if refs.is_empty() {
            then(self, Ok(prompt));
            return;
        }
        let mimes: Vec<Option<String>> = message.files.iter().flatten().map(|f| f.mime.clone()).collect();
        let (client, media) = (self.client.clone(), self.media.clone());
        self.spawn(
            async move {
                let reads = refs.iter().map(|(_, uri)| {
                    let (client, media) = (client.clone(), media.clone());
                    async move {
                        match media.read(&client, uri).await {
                            Ok(bytes) => Ok(Read::Bytes(bytes)),
                            Err(e) if allow_missing && is_missing_media(&e) => Ok(Read::Missing),
                            Err(e) => Err(e),
                        }
                    }
                });
                let results = futures::future::try_join_all(reads).await?;
                Ok(refs.into_iter().map(|(i, _)| i).zip(results).collect::<Vec<(usize, Read)>>())
            },
            move |app, r| {
                let reads = match r {
                    Ok(reads) => reads,
                    Err(e) => return then(app, Err(e)),
                };
                let mut prompt = prompt;
                let mut missing = 0;
                let mut drop = Vec::new();
                for (i, read) in reads {
                    match read {
                        Read::Bytes(bytes) => {
                            let mime = mimes.get(i).cloned().flatten().unwrap_or_else(|| "application/octet-stream".into());
                            let data = base64::engine::general_purpose::STANDARD.encode(bytes.as_slice());
                            prompt.files[i]["uri"] = Value::String(format!("data:{mime};base64,{data}"));
                        }
                        Read::Missing => {
                            missing += 1;
                            drop.push(i);
                        }
                    }
                }
                for i in drop.into_iter().rev() {
                    prompt.files.remove(i);
                }
                if missing > 0 {
                    let message = if missing == 1 {
                        "An image in this message is no longer stored, so it can't be restored".to_string()
                    } else {
                        format!("{missing} images in this message are no longer stored, so they can't be restored")
                    };
                    app.toast(Toast::new("warning", message));
                }
                then(app, Ok(prompt));
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_refs() {
        let hash = "e".repeat(64);
        assert_eq!(media_hash(&format!("media:{hash}")), Some(hash.as_str()));
        assert_eq!(media_hash(&format!("media:{}", "E".repeat(64))), None);
        assert_eq!(media_hash("media:abc"), None);
        assert_eq!(media_hash("data:image/png;base64,AA"), None);
    }
}
