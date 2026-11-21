//! Decoded images by source URI (`createMediaSource` plus the `<image>` load). Inline `data:` URIs and local files
//! decode on first use; `media:` refs and `http(s):` URLs are fetched by the app ([`Store::take_wanted`]) and then
//! handed back with [`Store::resolve`]. A source that can't be read or decoded is `Failed` ("No preview") until the
//! store forgets it.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;

use super::decode::{self, Loaded, Source};

fn key(uri: &str) -> (u64, usize) {
    let mut h = DefaultHasher::new();
    uri.hash(&mut h);
    (h.finish(), uri.len())
}

/// How many decoded images are kept.
const CAPACITY: usize = 64;

#[derive(Default)]
pub struct Store {
    entries: HashMap<(u64, usize), (Loaded, u64)>,
    wanted: Vec<String>,
    clock: u64,
}

impl Store {
    /// The image for `uri`, decoding it or asking for its bytes on first use.
    pub fn get(&mut self, uri: &str) -> Loaded {
        self.clock += 1;
        let k = key(uri);
        if let Some((loaded, used)) = self.entries.get_mut(&k) {
            *used = self.clock;
            return loaded.clone();
        }
        let loaded = match decode::source_of(uri) {
            Source::Bytes(bytes) => decoded(&bytes),
            Source::File(path) => std::fs::read(&path).map(|b| decoded(&b)).unwrap_or(Loaded::Failed),
            Source::Media(_) | Source::Remote(_) => {
                self.wanted.push(uri.to_string());
                Loaded::Pending
            }
            Source::Invalid => Loaded::Failed,
        };
        self.insert(k, loaded.clone());
        loaded
    }

    fn insert(&mut self, k: (u64, usize), loaded: Loaded) {
        if self.entries.len() >= CAPACITY
            && let Some(oldest) = self.entries.iter().filter(|(_, (l, _))| !matches!(l, Loaded::Pending)).min_by_key(|(_, (_, u))| *u).map(|(k, _)| *k)
        {
            self.entries.remove(&oldest);
        }
        self.entries.insert(k, (loaded, self.clock));
    }

    /// Sources waiting for bytes, each returned once.
    pub fn take_wanted(&mut self) -> Vec<String> {
        std::mem::take(&mut self.wanted)
    }

    /// The bytes of a wanted source arrived (`None`: the read failed).
    pub fn resolve(&mut self, uri: &str, bytes: Option<&[u8]>) {
        let loaded = bytes.map(decoded).unwrap_or(Loaded::Failed);
        self.clock += 1;
        self.entries.insert(key(uri), (loaded, self.clock));
    }

    /// Whether `uri` failed to load or decode (`media.failed()`).
    pub fn failed(&mut self, uri: &str) -> bool {
        matches!(self.get(uri), Loaded::Failed)
    }

    /// The decoded image, if ready.
    pub fn ready(&mut self, uri: &str) -> Option<Arc<decode::Picture>> {
        match self.get(uri) {
            Loaded::Ready(p) => Some(p),
            _ => None,
        }
    }
}

fn decoded(bytes: &[u8]) -> Loaded {
    match decode::decode(bytes) {
        Ok(p) => Loaded::Ready(Arc::new(p)),
        Err(_) => Loaded::Failed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    #[test]
    fn decodes_inline_and_waits_for_media() {
        let png = super::super::tests::png(2, 2, |_, _| [1, 2, 3, 255]);
        let uri = format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(&png));
        let mut s = Store::default();
        let a = s.ready(&uri).unwrap();
        assert!(Arc::ptr_eq(&a, &s.ready(&uri).unwrap()));
        assert!(s.failed("data:image/png;base64,AQID"));
        let media = format!("media:{}", "b".repeat(64));
        assert!(matches!(s.get(&media), Loaded::Pending));
        assert!(matches!(s.get(&media), Loaded::Pending));
        assert_eq!(s.take_wanted(), vec![media.clone()]);
        s.resolve(&media, Some(&png));
        assert!(s.ready(&media).is_some());
        s.resolve(&media, None);
        assert!(s.failed(&media));
    }
}
