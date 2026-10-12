//! Key-value store (`core/src/kv.ts`).

use rusqlite::{params, OptionalExtension};
use serde_json::Value;

use crate::db::{now_ms, Database};

impl Database {
    pub fn kv_get(&self, key: &str) -> anyhow::Result<Option<Value>> {
        let raw: Option<String> =
            self.with(|c| c.query_row("SELECT value FROM kv WHERE key = ?", [key], |r| r.get(0)).optional())?;
        Ok(raw.and_then(|s| serde_json::from_str(&s).ok()))
    }

    pub fn kv_set(&self, key: &str, value: &Value) -> anyhow::Result<()> {
        let now = now_ms();
        self.with(|c| {
            c.execute(
                "INSERT INTO kv (key, value, time_created, time_updated) VALUES (?, ?, ?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value, time_updated = ?",
                params![key, value.to_string(), now, now, now],
            )
        })?;
        Ok(())
    }

    pub fn kv_remove(&self, key: &str) -> anyhow::Result<()> {
        self.with(|c| c.execute("DELETE FROM kv WHERE key = ?", [key]))?;
        Ok(())
    }

    /// `scan({prefix, after?, limit?})` → (entries, next)
    pub fn kv_scan(
        &self,
        prefix: &str,
        after: Option<&str>,
        limit: Option<usize>,
    ) -> anyhow::Result<(Vec<(String, Value)>, Option<String>)> {
        let limit = limit.unwrap_or(100).clamp(1, 1000);
        let end = prefix_end(prefix);
        let rows: Vec<(String, String)> = self.with(|c| {
            let mut s = c.prepare(
                "SELECT key, value FROM kv WHERE key >= ?1 AND (?2 IS NULL OR key < ?2) AND (?3 IS NULL OR key > ?3) ORDER BY key LIMIT ?4",
            )?;
            let r = s
                .query_map(params![prefix, end, after, (limit + 1) as i64], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect();
            r
        })?;
        let more = rows.len() > limit;
        let entries: Vec<(String, Value)> = rows
            .into_iter()
            .take(limit)
            .map(|(k, v)| (k, serde_json::from_str(&v).unwrap_or(Value::Null)))
            .collect();
        let next = if more { entries.last().map(|(k, _)| k.clone()) } else { None };
        Ok((entries, next))
    }
}

/// Increment the last code point below 0x10FFFF.
fn prefix_end(prefix: &str) -> Option<String> {
    let mut chars: Vec<char> = prefix.chars().collect();
    while let Some(last) = chars.pop() {
        let mut n = last as u32 + 1;
        if (0xD800..0xE000).contains(&n) {
            n = 0xE000;
        }
        if let Some(c) = char::from_u32(n).filter(|_| (last as u32) < 0x10FFFF) {
            chars.push(c);
            return Some(chars.into_iter().collect());
        }
    }
    None
}
