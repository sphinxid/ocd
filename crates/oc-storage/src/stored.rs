//! Stored message rows as v2.2 writes them: assistant parts that a newer release moved to
//! `session_part` (`core/src/session/history.ts` `withParts`, `projector.ts` `writeParts`) and media
//! kept in `blob` behind `media:<sha256>` refs (`core/src/session/blob.ts`).
//!
//! ocd never creates part rows itself (upstream 2.2.5 does not either); it reads them and keeps them up to
//! date. Blobs are written where the session's location sets `experimental.media_blob` (the projector's
//! `Committing::media`, imports), read back inline or kept as refs for clients that opt in, and released.

use std::collections::{HashMap, HashSet};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use serde_json::{Map, Value, json};

/// `PartQueryBatchSize` / blob batches: ids per `IN (...)` query.
pub const BATCH: usize = 500;

/// A `session_message` row being read: `(id, type, data)` with `data` replaceable in place.
pub trait MessageRow {
    fn row(&mut self) -> (&str, &str, &mut String);
}

impl MessageRow for (String, String, String) {
    fn row(&mut self) -> (&str, &str, &mut String) {
        (&self.0, &self.1, &mut self.2)
    }
}

impl MessageRow for (i64, String, String, String) {
    fn row(&mut self) -> (&str, &str, &mut String) {
        (&self.1, &self.2, &mut self.3)
    }
}

impl MessageRow for (i64, String, String, String, String) {
    fn row(&mut self) -> (&str, &str, &mut String) {
        (&self.1, &self.2, &mut self.3)
    }
}

fn placeholders(n: usize) -> String {
    vec!["?"; n].join(", ")
}

fn any_rows(c: &Connection, table: &str) -> rusqlite::Result<bool> {
    Ok(c.query_row(&format!("SELECT 1 FROM {table} LIMIT 1"), [], |_| Ok(())).optional()?.is_some())
}

// ---- session_part ---------------------------------------------------------------------------

/// `SessionHistory.withParts`: each assistant row whose message has `session_part` rows reads
/// them (in ordinal order) as its `data.content`. Rows without parts keep their inline content.
pub fn with_parts<R: MessageRow>(c: &Connection, rows: &mut [R]) -> rusqlite::Result<()> {
    let ids: Vec<String> = rows
        .iter_mut()
        .filter_map(|r| {
            let (id, kind, _) = r.row();
            (kind == "assistant").then(|| id.to_string())
        })
        .collect();
    if ids.is_empty() || !any_rows(c, "session_part")? {
        return Ok(());
    }
    let mut by_message: HashMap<String, Vec<Value>> = HashMap::new();
    for chunk in ids.chunks(BATCH) {
        let mut s = c.prepare(&format!(
            "SELECT message_id, data FROM session_part WHERE message_id IN ({}) ORDER BY message_id ASC, ordinal ASC",
            placeholders(chunk.len())
        ))?;
        let found = s.query_map(params_from_iter(chunk.iter()), |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in found {
            let (message, data) = row?;
            by_message.entry(message).or_default().push(serde_json::from_str(&data).unwrap_or(Value::Null));
        }
    }
    if by_message.is_empty() {
        return Ok(());
    }
    for r in rows.iter_mut() {
        let (id, _, data) = r.row();
        let Some(parts) = by_message.get(id) else { continue };
        let Ok(Value::Object(mut obj)) = serde_json::from_str::<Value>(data) else { continue };
        if !obj.contains_key("content") {
            continue;
        }
        obj.insert("content".into(), Value::Array(parts.clone()));
        *data = Value::Object(obj).to_string();
    }
    Ok(())
}

/// A message's `session_part` rows `(ordinal, data)` in ordinal order.
pub fn part_rows(c: &Connection, message: &str) -> rusqlite::Result<Vec<(i64, String)>> {
    let mut s = c.prepare("SELECT ordinal, data FROM session_part WHERE message_id = ? ORDER BY ordinal ASC")?;
    let rows = s.query_map([message], |r| Ok((r.get(0)?, r.get(1)?)))?.collect();
    rows
}

/// `writeParts`: writes `content` over a message's existing part rows, touching only the rows that
/// changed: changed ordinals are updated, new parts appended after the last ordinal, stale ones deleted.
pub fn write_parts(
    c: &Connection,
    session: &str,
    message: &str,
    created: i64,
    existing: &[(i64, String)],
    content: &[Value],
) -> rusqlite::Result<()> {
    let mut next = existing.last().map(|p| p.0).unwrap_or(-1) + 1;
    for (index, part) in content.iter().enumerate() {
        let kind = part.get("type").and_then(Value::as_str).unwrap_or_default();
        let call_id = if kind == "tool" { part.get("id").and_then(Value::as_str) } else { None };
        let encoded = part.to_string();
        match existing.get(index) {
            None => {
                c.execute(
                    "INSERT INTO session_part (message_id, session_id, ordinal, type, call_id, time_created, time_updated, data) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                    params![message, session, next, kind, call_id, created, created, encoded],
                )?;
                next += 1;
            }
            Some((ordinal, current)) => {
                let same = serde_json::from_str::<Value>(current).map(|v| v.to_string() == encoded).unwrap_or(false);
                if !same {
                    c.execute(
                        "UPDATE session_part SET type = ?, call_id = ?, data = ?, time_updated = ? WHERE message_id = ? AND ordinal = ?",
                        params![kind, call_id, encoded, created, message, ordinal],
                    )?;
                }
            }
        }
    }
    let stale: Vec<i64> = existing.iter().skip(content.len()).map(|p| p.0).collect();
    if !stale.is_empty() {
        let mut args: Vec<rusqlite::types::Value> = vec![message.to_string().into()];
        args.extend(stale.iter().map(|o| rusqlite::types::Value::Integer(*o)));
        c.execute(
            &format!("DELETE FROM session_part WHERE message_id = ? AND ordinal IN ({})", placeholders(stale.len())),
            params_from_iter(args),
        )?;
    }
    Ok(())
}

// ---- media blobs ----------------------------------------------------------------------------

/// `Prompt.MediaRef`: `media:` and 64 lowercase hex digits.
pub fn is_media_ref(uri: &str) -> bool {
    uri.strip_prefix("media:").is_some_and(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
}

fn ref_hash(uri: &str) -> &str {
    &uri["media:".len()..]
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// `SessionStored.FileRef`: a user file whose bytes live in `blob`.
fn is_file_ref(file: &Value) -> bool {
    str_field(file, "uri").is_some_and(is_media_ref) && str_field(file, "mime").is_some()
}

/// `SessionStored.ToolFileRef`: a tool result file whose bytes live in `blob`.
fn is_tool_file_ref(item: &Value) -> bool {
    str_field(item, "type") == Some("file") && str_field(item, "uri").is_some_and(is_media_ref) && str_field(item, "mime").is_some()
}

/// Content items of a settled tool part (`completed` / `error`).
fn tool_items(part: &Value) -> Option<&Vec<Value>> {
    if str_field(part, "type") != Some("tool") {
        return None;
    }
    let state = part.get("state")?;
    match str_field(state, "status") {
        Some("completed") | Some("error") => state.get("content").and_then(Value::as_array),
        _ => None,
    }
}

fn tool_items_mut(part: &mut Value) -> Option<&mut Vec<Value>> {
    if str_field(part, "type") != Some("tool") {
        return None;
    }
    let state = part.get_mut("state")?;
    match str_field(state, "status") {
        Some("completed") | Some("error") => state.get_mut("content").and_then(Value::as_array_mut),
        _ => None,
    }
}

/// `collect`: every blob hash a stored message row or inbox payload names.
pub fn hashes_of(data: &Value) -> HashSet<String> {
    let mut out = HashSet::new();
    if let Some(files) = data.get("files").and_then(Value::as_array) {
        for f in files.iter().filter(|f| is_file_ref(f)) {
            out.insert(ref_hash(str_field(f, "uri").unwrap_or_default()).to_string());
        }
    }
    if let Some(content) = data.get("content").and_then(Value::as_array) {
        for part in content {
            for item in tool_items(part).into_iter().flatten().filter(|i| is_tool_file_ref(i)) {
                out.insert(ref_hash(str_field(item, "uri").unwrap_or_default()).to_string());
            }
        }
    }
    out
}

/// A ref whose blob is gone reads as this text, so clients and the model never see a bare ref.
fn unavailable(mime: &str, name: Option<&str>) -> String {
    match name {
        None => format!("[media unavailable: {mime}]"),
        Some(name) => format!("[media unavailable: {name} ({mime})]"),
    }
}

fn inline_file(file: &mut Value, blobs: &HashMap<String, String>) {
    if !is_file_ref(file) {
        return;
    }
    let Some(obj) = file.as_object_mut() else { return };
    let uri = obj.shift_remove("uri").and_then(|u| u.as_str().map(str::to_string)).unwrap_or_default();
    match blobs.get(ref_hash(&uri)) {
        Some(data) => {
            obj.insert("data".into(), Value::String(data.clone()));
        }
        None => {
            let mime = obj.get("mime").and_then(Value::as_str).unwrap_or_default().to_string();
            let name = obj.get("name").and_then(Value::as_str).map(str::to_string);
            obj.insert("data".into(), Value::String(STANDARD.encode(unavailable(&mime, name.as_deref()))));
            obj.insert("mime".into(), Value::String("text/plain".into()));
        }
    }
}

fn inline_tool_item(item: &mut Value, blobs: &HashMap<String, String>) {
    if !is_tool_file_ref(item) {
        return;
    }
    let uri = str_field(item, "uri").unwrap_or_default().to_string();
    let mime = str_field(item, "mime").unwrap_or_default().to_string();
    match blobs.get(ref_hash(&uri)) {
        Some(data) => item["uri"] = Value::String(format!("data:{mime};base64,{data}")),
        None => *item = json!({ "type": "text", "text": unavailable(&mime, str_field(item, "name")) }),
    }
}

/// Base64 bytes of the given blobs, 500 hashes per query.
fn load_blobs(c: &Connection, hashes: &HashSet<String>) -> rusqlite::Result<HashMap<String, String>> {
    let all: Vec<&String> = hashes.iter().collect();
    let mut out = HashMap::new();
    for chunk in all.chunks(BATCH) {
        let mut s = c.prepare(&format!("SELECT hash, data FROM blob WHERE hash IN ({})", placeholders(chunk.len())))?;
        let rows = s.query_map(params_from_iter(chunk.iter()), |r| Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?)))?;
        for row in rows {
            let (hash, data) = row?;
            out.insert(hash, STANDARD.encode(data));
        }
    }
    Ok(out)
}

/// `rewriteMessage` with `inlineFile` / `inlineToolItem`.
fn inline_message(data: &mut Value, blobs: &HashMap<String, String>) {
    if let Some(files) = data.get_mut("files").and_then(Value::as_array_mut)
        && files.iter().any(is_file_ref)
    {
        for f in files.iter_mut() {
            inline_file(f, blobs);
        }
        return;
    }
    if let Some(content) = data.get_mut("content").and_then(Value::as_array_mut) {
        for part in content.iter_mut() {
            if let Some(items) = tool_items_mut(part) {
                for item in items.iter_mut() {
                    inline_tool_item(item, blobs);
                }
            }
        }
    }
}

/// `SessionBlob.hydrate`: restores inline media for the refs in `session_message` rows. Rows
/// without refs are left untouched.
pub fn hydrate<R: MessageRow>(c: &Connection, rows: &mut [R]) -> rusqlite::Result<()> {
    let mut parsed: Vec<Option<Value>> = Vec::with_capacity(rows.len());
    let mut hashes = HashSet::new();
    for r in rows.iter_mut() {
        let (_, _, data) = r.row();
        // Cheap pre-check: a row naming no ref is never parsed.
        if !data.contains("\"media:") {
            parsed.push(None);
            continue;
        }
        let value: Option<Value> = serde_json::from_str(data).ok();
        let found = value.as_ref().map(hashes_of).unwrap_or_default();
        if found.is_empty() {
            parsed.push(None);
            continue;
        }
        hashes.extend(found);
        parsed.push(value);
    }
    if hashes.is_empty() {
        return Ok(());
    }
    let blobs = load_blobs(c, &hashes)?;
    for (r, value) in rows.iter_mut().zip(parsed) {
        let Some(mut value) = value else { continue };
        inline_message(&mut value, &blobs);
        let (_, _, data) = r.row();
        *data = value.to_string();
    }
    Ok(())
}

/// `withParts` then `hydrate`: what every read that returns messages sees.
pub fn restore<R: MessageRow>(c: &Connection, rows: &mut [R]) -> rusqlite::Result<()> {
    with_parts(c, rows)?;
    hydrate(c, rows)
}

/// `SessionBlob.hydrateInbox`: restores inline media for the refs in `session_inbox` payloads.
pub fn hydrate_inbox(c: &Connection, payloads: &mut [&mut Value]) -> rusqlite::Result<()> {
    let mut hashes = HashSet::new();
    for p in payloads.iter() {
        if let Some(files) = p.get("files").and_then(Value::as_array) {
            for f in files.iter().filter(|f| is_file_ref(f)) {
                hashes.insert(ref_hash(str_field(f, "uri").unwrap_or_default()).to_string());
            }
        }
    }
    if hashes.is_empty() {
        return Ok(());
    }
    let blobs = load_blobs(c, &hashes)?;
    for p in payloads.iter_mut() {
        if let Some(files) = p.get_mut("files").and_then(Value::as_array_mut)
            && files.iter().any(is_file_ref)
        {
            for f in files.iter_mut() {
                inline_file(f, &blobs);
            }
        }
    }
    Ok(())
}

/// `SessionBlob.read`: a blob's mime and bytes, only while some message or inbox item references it.
pub fn read_blob(c: &Connection, hash: &str) -> rusqlite::Result<Option<(String, Vec<u8>)>> {
    c.query_row(
        "SELECT mime, data FROM blob WHERE hash = ? AND EXISTS (SELECT 1 FROM blob_ref WHERE blob_ref.hash = blob.hash)",
        [hash],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()
}

/// `recordRefs`: records that a message (or pending inbox row) at `seq` references each blob its
/// stored data names. A ref whose blob is already gone keeps nothing alive.
pub fn record_refs(c: &Connection, session: &str, message: &str, seq: i64, data: &Value) -> rusqlite::Result<()> {
    record_hashes(c, session, message, seq, &hashes_of(data))
}

fn record_hashes(c: &Connection, session: &str, message: &str, seq: i64, hashes: &HashSet<String>) -> rusqlite::Result<()> {
    if hashes.is_empty() {
        return Ok(());
    }
    let list: Vec<&String> = hashes.iter().collect();
    let found: Vec<String> = {
        let mut s = c.prepare(&format!("SELECT hash FROM blob WHERE hash IN ({})", placeholders(list.len())))?;
        let rows = s.query_map(params_from_iter(list.iter()), |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        rows
    };
    for hash in found {
        c.execute(
            "INSERT INTO blob_ref (session_id, message_id, hash, seq) VALUES (?, ?, ?, ?) ON CONFLICT (session_id, message_id, hash) DO UPDATE SET seq = ?",
            params![session, message, hash, seq, seq],
        )?;
    }
    Ok(())
}

/// `syncRefs`: records the refs a rewritten message names and releases the ones it no longer does.
pub fn sync_refs(c: &Connection, session: &str, message: &str, seq: i64, data: &Value) -> rusqlite::Result<()> {
    let present = hashes_of(data);
    record_hashes(c, session, message, seq, &present)?;
    let mut args: Vec<rusqlite::types::Value> = vec![session.to_string().into(), message.to_string().into()];
    let mut sql = "session_id = ? AND message_id = ?".to_string();
    if !present.is_empty() {
        sql.push_str(&format!(" AND hash NOT IN ({})", placeholders(present.len())));
        args.extend(present.into_iter().map(rusqlite::types::Value::Text));
    }
    release(c, &sql, args)
}

/// Deletes the matching refs, then every blob among theirs that no ref names anymore.
fn release(c: &Connection, filter: &str, args: Vec<rusqlite::types::Value>) -> rusqlite::Result<()> {
    if !any_rows(c, "blob_ref")? {
        return Ok(());
    }
    let released: Vec<String> = {
        let mut s = c.prepare(&format!("DELETE FROM blob_ref WHERE {filter} RETURNING hash"))?;
        let rows = s.query_map(params_from_iter(args), |r| r.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?;
        rows
    };
    let mut seen = HashSet::new();
    let hashes: Vec<String> = released.into_iter().filter(|h| seen.insert(h.clone())).collect();
    for chunk in hashes.chunks(BATCH) {
        c.execute(
            &format!(
                "DELETE FROM blob WHERE hash IN ({}) AND NOT EXISTS (SELECT 1 FROM blob_ref WHERE blob_ref.hash = blob.hash)",
                placeholders(chunk.len())
            ),
            params_from_iter(chunk.iter()),
        )?;
    }
    Ok(())
}

/// `releaseMessage`: the blobs a removed message or cancelled inbox row referenced.
pub fn release_message(c: &Connection, session: &str, message: &str) -> rusqlite::Result<()> {
    release(c, "session_id = ? AND message_id = ?", vec![session.to_string().into(), message.to_string().into()])
}

/// `releaseFrom`: the blobs referenced at or after `seq`, for a revert.
pub fn release_from(c: &Connection, session: &str, seq: i64) -> rusqlite::Result<()> {
    release(c, "session_id = ? AND seq >= ?", vec![session.to_string().into(), seq.into()])
}

/// `releaseSession`: every blob a deleted session referenced.
pub fn release_session(c: &Connection, session: &str) -> rusqlite::Result<()> {
    release(c, "session_id = ?", vec![session.to_string().into()])
}

// ---- media blobs: writes (`experimental.media_blob`) ----------------------------------------

/// Node `Buffer.from(s, "base64")`: both alphabets, whitespace and other characters skipped, stops at `=`.
fn node_base64_decode(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits = 0;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            _ => continue,
        };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
}

/// A blob taken out of a row, not yet saved.
#[derive(Debug, Clone, PartialEq)]
pub struct Pending {
    pub hash: String,
    pub mime: String,
    pub data: Vec<u8>,
}

/// Blobs taken out of one row, by hash, in the order they were first named.
#[derive(Debug, Default)]
pub struct Taken(Vec<Pending>);

impl Taken {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn hashes(&self) -> HashSet<String> {
        self.0.iter().map(|p| p.hash.clone()).collect()
    }

    pub fn blobs(&self) -> &[Pending] {
        &self.0
    }

    /// `take`: the ref of `raw`, recording its blob the first time.
    fn take(&mut self, raw: Vec<u8>, mime: &str) -> String {
        let hash = oc_util::hash::sha256(&raw);
        if !self.0.iter().any(|p| p.hash == hash) {
            self.0.push(Pending { hash: hash.clone(), mime: mime.to_string(), data: raw });
        }
        format!("media:{hash}")
    }
}

/// `DATA_URI` (`^data:[^,]*;base64,(.*)$`, dot-all): the payload of a base64 `data:` URI.
fn data_uri_payload(uri: &str) -> Option<&str> {
    let rest = uri.strip_prefix("data:")?;
    let comma = rest.find(',')?;
    rest[..comma].ends_with(";base64").then(|| &rest[comma + 1..])
}

/// `fileRef`: an inline user file becomes a ref (`data` dropped, `uri` appended); refs pass through.
fn file_ref(taken: &mut Taken, file: &mut Value) {
    if is_file_ref(file) {
        return;
    }
    let Some(obj) = file.as_object_mut() else { return };
    let data = obj.shift_remove("data").and_then(|d| d.as_str().map(str::to_string)).unwrap_or_default();
    let mime = obj.get("mime").and_then(Value::as_str).unwrap_or_default().to_string();
    let uri = taken.take(node_base64_decode(&data), &mime);
    obj.insert("uri".into(), Value::String(uri));
}

/// `toolRef`: a tool result file with a base64 `data:` URI becomes a ref.
fn tool_ref(taken: &mut Taken, item: &mut Value) {
    if str_field(item, "type") != Some("file") {
        return;
    }
    let Some(payload) = str_field(item, "uri").and_then(data_uri_payload).map(str::to_string) else { return };
    let mime = str_field(item, "mime").unwrap_or_default().to_string();
    let uri = taken.take(node_base64_decode(&payload), &mime);
    item["uri"] = Value::String(uri);
}

/// `externalizePayload` / `externalize` with the flag on: a user message's (or inbox user payload's)
/// inline files, or an assistant message's tool `data:` files, become refs. `kind` is the row type.
pub fn externalize(kind: &str, data: &mut Value) -> Taken {
    let mut taken = Taken::default();
    match kind {
        "user" => {
            if let Some(files) = data.get_mut("files").and_then(Value::as_array_mut) {
                for f in files.iter_mut() {
                    file_ref(&mut taken, f);
                }
            }
        }
        "assistant" => {
            if let Some(content) = data.get_mut("content").and_then(Value::as_array_mut) {
                for part in content.iter_mut() {
                    if let Some(items) = tool_items_mut(part) {
                        for item in items.iter_mut() {
                            tool_ref(&mut taken, item);
                        }
                    }
                }
            }
        }
        _ => {}
    }
    taken
}

/// `split`: moves a stored row's inline media into blobs whatever the flag says (forks and imports):
/// files when some file is inline, else tool `data:` files.
pub fn split(data: &mut Value) -> Taken {
    let mut taken = Taken::default();
    let inline_files = data.get("files").and_then(Value::as_array).is_some_and(|f| f.iter().any(|f| !is_file_ref(f)));
    if inline_files {
        for f in data.get_mut("files").and_then(Value::as_array_mut).into_iter().flatten() {
            file_ref(&mut taken, f);
        }
    } else if let Some(content) = data.get_mut("content").and_then(Value::as_array_mut) {
        for part in content.iter_mut() {
            if let Some(items) = tool_items_mut(part) {
                for item in items.iter_mut() {
                    tool_ref(&mut taken, item);
                }
            }
        }
    }
    taken
}

/// `save`: inserts blobs, keeping any that already exist.
pub fn save(c: &Connection, taken: &Taken) -> rusqlite::Result<()> {
    let now = crate::db::now_ms();
    for blob in taken.blobs() {
        c.execute(
            "INSERT INTO blob (hash, mime, bytes, width, height, data, time_created) VALUES (?, ?, ?, NULL, NULL, ?, ?) ON CONFLICT DO NOTHING",
            params![blob.hash, blob.mime, blob.data.len() as i64, blob.data, now],
        )?;
    }
    Ok(())
}

/// `recordHashes`: refs for the given hashes only (what an import took out of its own data).
pub fn record_taken(c: &Connection, session: &str, message: &str, seq: i64, hashes: &HashSet<String>) -> rusqlite::Result<()> {
    record_hashes(c, session, message, seq, hashes)
}

/// What a client that fetches bytes itself learns about a blob (`SessionBlob.Meta`).
#[derive(Debug, Clone, PartialEq)]
pub struct Meta {
    pub bytes: i64,
    pub width: Option<i64>,
    pub height: Option<i64>,
}

/// `loadMeta`: size and dimensions of the referenced blobs among `hashes`.
fn load_meta(c: &Connection, hashes: &HashSet<String>) -> rusqlite::Result<HashMap<String, Meta>> {
    let all: Vec<&String> = hashes.iter().collect();
    let mut out = HashMap::new();
    for chunk in all.chunks(BATCH) {
        let mut s = c.prepare(&format!(
            "SELECT hash, bytes, width, height FROM blob WHERE hash IN ({}) AND EXISTS (SELECT 1 FROM blob_ref WHERE blob_ref.hash = blob.hash)",
            placeholders(chunk.len())
        ))?;
        let rows = s.query_map(params_from_iter(chunk.iter()), |r| {
            Ok((r.get::<_, String>(0)?, Meta { bytes: r.get(1)?, width: r.get(2)?, height: r.get(3)? }))
        })?;
        for row in rows {
            let (hash, meta) = row?;
            out.insert(hash, meta);
        }
    }
    Ok(out)
}

/// `keepFile`: a ref whose blob is gone or no longer referenced reads as the unavailable placeholder.
fn keep_file(file: &mut Value, meta: &HashMap<String, Meta>) {
    if is_file_ref(file) && !meta.contains_key(ref_hash(str_field(file, "uri").unwrap_or_default())) {
        inline_file(file, &HashMap::new());
    }
}

fn keep_tool_item(item: &mut Value, meta: &HashMap<String, Meta>) {
    if is_tool_file_ref(item) && !meta.contains_key(ref_hash(str_field(item, "uri").unwrap_or_default())) {
        inline_tool_item(item, &HashMap::new());
    }
}

/// `describe`: a kept user file ref as `Prompt.MediaFileAttachment` encodes it (`data` removed; then
/// `uri`, `bytes`, `width`, `height` after the attachment fields).
fn describe_file(file: &mut Value, meta: &HashMap<String, Meta>) {
    let Some(obj) = file.as_object_mut() else { return };
    let Some(uri) = obj.get("uri").and_then(Value::as_str).map(str::to_string) else { return };
    obj.shift_remove("data");
    obj.shift_remove("uri");
    obj.insert("uri".into(), Value::String(uri.clone()));
    if let Some(m) = meta.get(ref_hash(&uri)) {
        obj.insert("bytes".into(), json!(m.bytes));
        if let Some(w) = m.width {
            obj.insert("width".into(), json!(w));
        }
        if let Some(h) = m.height {
            obj.insert("height".into(), json!(h));
        }
    }
}

/// `keepRefs` on stored message rows: refs stay, except those whose blob is gone or unreferenced. Returns the
/// metadata of every blob still named.
pub fn keep_refs<R: MessageRow>(c: &Connection, rows: &mut [R]) -> rusqlite::Result<HashMap<String, Meta>> {
    with_parts(c, rows)?;
    let mut parsed: Vec<Option<Value>> = Vec::with_capacity(rows.len());
    let mut hashes = HashSet::new();
    for r in rows.iter_mut() {
        let (_, _, data) = r.row();
        if !data.contains("\"media:") {
            parsed.push(None);
            continue;
        }
        let value: Option<Value> = serde_json::from_str(data).ok();
        let found = value.as_ref().map(hashes_of).unwrap_or_default();
        if found.is_empty() {
            parsed.push(None);
            continue;
        }
        hashes.extend(found);
        parsed.push(value);
    }
    let meta = load_meta(c, &hashes)?;
    for (r, value) in rows.iter_mut().zip(parsed) {
        let Some(mut value) = value else { continue };
        if let Some(files) = value.get_mut("files").and_then(Value::as_array_mut)
            && files.iter().any(is_file_ref)
        {
            for f in files.iter_mut() {
                keep_file(f, &meta);
            }
        } else if let Some(content) = value.get_mut("content").and_then(Value::as_array_mut) {
            for part in content.iter_mut() {
                if let Some(items) = tool_items_mut(part) {
                    for item in items.iter_mut() {
                        keep_tool_item(item, &meta);
                    }
                }
            }
        }
        let (_, _, data) = r.row();
        *data = value.to_string();
    }
    Ok(meta)
}

/// `keepInboxRefs` on inbox payloads.
pub fn keep_inbox_refs(c: &Connection, payloads: &mut [&mut Value]) -> rusqlite::Result<HashMap<String, Meta>> {
    let mut hashes = HashSet::new();
    for p in payloads.iter() {
        hashes.extend(hashes_of(p));
    }
    let meta = load_meta(c, &hashes)?;
    for p in payloads.iter_mut() {
        if let Some(files) = p.get_mut("files").and_then(Value::as_array_mut)
            && files.iter().any(is_file_ref)
        {
            for f in files.iter_mut() {
                keep_file(f, &meta);
            }
        }
    }
    Ok(meta)
}

/// `describe` on a decoded message or payload's `files`: each file that is a ref is encoded as a media
/// attachment with its blob's size and dimensions.
pub fn describe(files: &mut [Value], meta: &HashMap<String, Meta>) {
    for f in files.iter_mut() {
        if f.get("uri").is_some() {
            describe_file(f, meta);
        }
    }
}

/// A user message (or payload) whose `files` keep refs, made decodable by the typed schema: each ref file gets
/// an empty `data` in place of its `uri`. Returns the value and `(index, uri)` of each such file.
pub fn without_refs(mut value: Value) -> (Value, Vec<(usize, String)>) {
    let mut refs = Vec::new();
    if let Some(files) = value.get_mut("files").and_then(Value::as_array_mut) {
        for (i, f) in files.iter_mut().enumerate() {
            if !is_file_ref(f) {
                continue;
            }
            let Some(obj) = f.as_object_mut() else { continue };
            let uri = obj.shift_remove("uri").and_then(|u| u.as_str().map(str::to_string)).unwrap_or_default();
            obj.insert("data".into(), Value::String(String::new()));
            refs.push((i, uri));
        }
    }
    (value, refs)
}

/// Undoes [`without_refs`] on the encoded value: `data` dropped and `uri` appended, as `fileRef` stores them.
pub fn restore_refs(value: &mut Value, refs: &[(usize, String)]) {
    if refs.is_empty() {
        return;
    }
    let Some(files) = value.get_mut("files").and_then(Value::as_array_mut) else { return };
    for (i, uri) in refs {
        if let Some(obj) = files.get_mut(*i).and_then(Value::as_object_mut) {
            obj.shift_remove("data");
            obj.insert("uri".into(), Value::String(uri.clone()));
        }
    }
}

/// Parse a stored JSON object, `{}` when it is not one.
pub fn parse_object(data: &str) -> Map<String, Value> {
    match serde_json::from_str(data) {
        Ok(Value::Object(m)) => m,
        _ => Map::new(),
    }
}
