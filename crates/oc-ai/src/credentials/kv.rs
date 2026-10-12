//! Key/value storage used by well-known discovery and plugin storage (`packages/core/src/kv.ts`).

use async_trait::async_trait;
use indexmap::IndexMap;
use parking_lot::Mutex;
use serde_json::Value;

/// JSON key/value store. Core backs it with its KV table.
#[async_trait]
pub trait KvStore: Send + Sync {
    async fn get(&self, key: &str) -> anyhow::Result<Option<Value>>;
    async fn set(&self, key: &str, value: Value) -> anyhow::Result<()>;
    async fn remove(&self, key: &str) -> anyhow::Result<()>;
}

/// In-memory [`KvStore`].
#[derive(Default)]
pub struct MemoryKv {
    entries: Mutex<IndexMap<String, Value>>,
}

impl MemoryKv {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> IndexMap<String, Value> {
        self.entries.lock().clone()
    }
}

#[async_trait]
impl KvStore for MemoryKv {
    async fn get(&self, key: &str) -> anyhow::Result<Option<Value>> {
        Ok(self.entries.lock().get(key).cloned())
    }

    async fn set(&self, key: &str, value: Value) -> anyhow::Result<()> {
        self.entries.lock().insert(key.to_string(), value);
        Ok(())
    }

    async fn remove(&self, key: &str) -> anyhow::Result<()> {
        self.entries.lock().shift_remove(key);
        Ok(())
    }
}
