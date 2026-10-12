//! `read` (`core/src/tool/plugin/read.ts`, `core/src/tool/read-filesystem.ts`).

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use base64::Engine;
use oc_schema::ToolContent;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::file_access::{self, Target};
use crate::tool::{DefinitionContext, ErrorCause, Tool, ToolContext, ToolDefinition, ToolError, ToolOptions, ToolResult, decode, meta};
use crate::util::{self, js};

pub const MAX_READ_LINES: usize = 2_000;
pub const MAX_READ_BYTES: usize = 50 * 1024;
pub const MAX_MEDIA_INGEST_BYTES: u64 = 20 * 1024 * 1024;
const FIRST_CHUNK: usize = 256 * 1024;
const MAX_LINE_LENGTH: usize = 2_000;
const MAX_LINE_SUFFIX: &str = "... (line truncated to 2000 chars)";
pub const MEDIA_MIMES: [&str; 5] = ["image/png", "image/jpeg", "image/gif", "image/webp", "application/pdf"];

pub const DESCRIPTION: &str = "Read the contents of a file or directory. Supports text files, images, and PDFs. Images and PDFs are presented directly to the model. Each text line is prefixed by its 1-based line number as <line>: <content>. The prefix is for reference and is not part of the file content. Directory entries are returned one per line. Use offset and limit to read large files or directories in sections. Prefer one larger read over many small slices, and use grep to find specific content in large files.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListType {
    File,
    Directory,
    Symlink,
}

impl ListType {
    fn as_str(&self) -> &'static str {
        match self {
            ListType::File => "file",
            ListType::Directory => "directory",
            ListType::Symlink => "symlink",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListEntry {
    pub path: String,
    pub kind: ListType,
}

/// Structured read output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadOutput {
    File { uri: String, name: String, content: String, base64: bool, mime: String },
    TextPage { content: String, mime: String, offset: usize, truncated: bool, next: Option<usize> },
    ListPage { entries: Vec<ListEntry>, truncated: bool, next: Option<usize> },
}

impl ReadOutput {
    pub fn to_json(&self) -> Value {
        match self {
            ReadOutput::File { uri, name, content, base64, mime } => json!({
                "type": "file", "uri": uri, "name": name, "content": content,
                "encoding": if *base64 { "base64" } else { "utf8" }, "mime": mime,
            }),
            ReadOutput::TextPage { content, mime, offset, truncated, next } => {
                let mut v = json!({"type": "text-page", "content": content, "mime": mime, "offset": offset, "truncated": truncated});
                if let Some(next) = next {
                    v["next"] = json!(next);
                }
                v
            }
            ReadOutput::ListPage { entries, truncated, next } => {
                let mut v = json!({
                    "type": "list-page",
                    "entries": entries.iter().map(|e| json!({"path": e.path, "type": e.kind.as_str()})).collect::<Vec<_>>(),
                    "truncated": truncated,
                });
                if let Some(next) = next {
                    v["next"] = json!(next);
                }
                v
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    NotFound,
    Failed(String),
    Binary { resource: String },
    MediaLimit { resource: String },
    OffsetOutOfRange { offset: usize, lines: usize },
    PathKind { resource: String, expected: &'static str },
}

impl ReadError {
    pub fn message(&self) -> String {
        match self {
            ReadError::NotFound => "File not found".into(),
            ReadError::Failed(m) => m.clone(),
            ReadError::Binary { resource } => format!("Cannot read binary file: {resource}"),
            ReadError::MediaLimit { resource } => {
                format!("Media exceeds {MAX_MEDIA_INGEST_BYTES} byte ingestion limit: {resource}")
            }
            ReadError::OffsetOutOfRange { offset, lines } => format!(
                "Offset {offset} is out of range for this file ({lines} {})",
                if *lines == 1 { "line" } else { "lines" }
            ),
            ReadError::PathKind { resource, expected } => format!("Path is not {expected}: {resource}"),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Page {
    pub offset: Option<usize>,
    pub limit: Option<usize>,
}

fn clamp_line(line: &str) -> String {
    if js::utf16_len(line) > MAX_LINE_LENGTH {
        format!("{}{MAX_LINE_SUFFIX}", js::utf16_prefix(line, MAX_LINE_LENGTH))
    } else {
        line.to_string()
    }
}

/// `new TextDecoder().decode(bytes)`: lossy, strips one leading BOM.
fn text_decode(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    text.strip_prefix('\u{FEFF}').map(String::from).unwrap_or_else(|| text.into_owned())
}

fn nth_newline(bytes: &[u8], count: usize) -> Option<usize> {
    let mut found = 0;
    for (index, &byte) in bytes.iter().enumerate() {
        if byte != b'\n' {
            continue;
        }
        found += 1;
        if found == count {
            return Some(index + 1);
        }
    }
    None
}

fn line_count(newlines: usize, last: Option<u8>) -> usize {
    newlines + usize::from(!(last.is_none() || last == Some(b'\n')))
}

struct PageResult {
    entries: Vec<String>,
    offset: usize,
    next: Option<usize>,
    consumed: usize,
}

fn text_page(bytes: &[u8], eof: bool, page: Page) -> Option<PageResult> {
    let offset = page.offset.filter(|o| *o != 0).unwrap_or(1);
    let limit = page.limit.filter(|l| *l != 0).unwrap_or(MAX_READ_LINES).min(MAX_READ_LINES);
    let decoded = text_decode(bytes);
    let split: Vec<&str> = decoded.split('\n').collect();
    let complete: &[&str] = if eof {
        if split.last() == Some(&"") { &split[..split.len() - 1] } else { &split[..] }
    } else {
        &split[..split.len() - 1]
    };
    let available: Vec<&str> = complete.iter().map(|l| l.strip_suffix('\r').unwrap_or(l)).collect();

    let mut entries: Vec<String> = Vec::new();
    let mut size = 0usize;
    let mut next: Option<usize> = None;
    for (index, value) in available.iter().enumerate().skip(offset - 1) {
        let line = index + 1;
        if entries.len() >= limit || size >= MAX_READ_BYTES {
            next = Some(line);
            break;
        }
        let text = clamp_line(value);
        let line_size = text.len() + usize::from(!entries.is_empty());
        if size + line_size > MAX_READ_BYTES {
            next = Some(line);
            break;
        }
        entries.push(text);
        size += line_size;
    }
    if next.is_none() && entries.len() >= limit && (!eof || offset - 1 + entries.len() < available.len()) {
        next = Some(offset + entries.len());
    }
    if !eof && next.is_none() {
        return None;
    }
    let consumed_lines = match next {
        None => available.len(),
        Some(n) => n - 1,
    };
    let consumed = if consumed_lines == 0 { 0 } else { nth_newline(bytes, consumed_lines).unwrap_or(bytes.len()) };
    Some(PageResult { entries, offset, next, consumed })
}

fn make_text_page(path: &Path, resource: &str, result: PageResult, binary: bool, lines: usize) -> Result<ReadOutput, ReadError> {
    if binary {
        return Err(ReadError::Binary { resource: resource.to_string() });
    }
    if result.entries.is_empty() && result.offset != 1 {
        return Err(ReadError::OffsetOutOfRange { offset: result.offset, lines });
    }
    Ok(ReadOutput::TextPage {
        content: result.entries.join("\n"),
        mime: crate::mime::lookup(path),
        offset: result.offset,
        truncated: result.next.is_some(),
        next: result.next,
    })
}

fn io_error(error: std::io::Error) -> ReadError {
    if error.kind() == std::io::ErrorKind::NotFound { ReadError::NotFound } else { ReadError::Failed(error.to_string()) }
}

/// Directory listing entries (lstat types, `other` dropped).
pub fn list_dir(path: &Path) -> Result<Vec<(String, ListType)>, ReadError> {
    let entries = std::fs::read_dir(path).map_err(io_error)?;
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else { continue };
        let kind = if kind.is_symlink() {
            ListType::Symlink
        } else if kind.is_dir() {
            ListType::Directory
        } else if kind.is_file() {
            ListType::File
        } else {
            continue;
        };
        out.push((entry.file_name().to_string_lossy().into_owned(), kind));
    }
    Ok(out)
}

fn list(items: Vec<(String, ListType)>, page: Page) -> ReadOutput {
    let offset = page.offset.filter(|o| *o != 0).unwrap_or(1);
    let limit = page.limit.filter(|l| *l != 0).unwrap_or(MAX_READ_LINES).min(MAX_READ_LINES);
    let mut visible: Vec<ListEntry> = items
        .into_iter()
        .map(|(name, kind)| ListEntry {
            path: if kind == ListType::Directory { format!("{name}{}", std::path::MAIN_SEPARATOR) } else { name },
            kind,
        })
        .collect();
    visible.sort_by(|a, b| match (a.kind == ListType::Directory, b.kind == ListType::Directory) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => js::locale_compare(&a.path, &b.path),
    });
    let start = (offset - 1).min(visible.len());
    let end = (offset - 1 + limit).min(visible.len());
    let selected: Vec<ListEntry> = visible[start..end].to_vec();
    let truncated = offset - 1 + selected.len() < visible.len();
    let next = truncated.then_some(offset + selected.len());
    ReadOutput::ListPage { entries: selected, truncated, next }
}

fn file_uri(path: &Path) -> String {
    url::Url::from_file_path(path).map(|u| u.to_string()).unwrap_or_else(|_| format!("file://{}", path.to_string_lossy()))
}

fn read_range(file: &mut std::fs::File, length: usize) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut buffer = Vec::with_capacity(length);
    file.take(length as u64).read_to_end(&mut buffer)?;
    Ok(buffer)
}

/// `ReadToolFileSystem.read`.
pub fn read(path: &Path, resource: &str, page: Page) -> Result<ReadOutput, ReadError> {
    let metadata = std::fs::metadata(path).map_err(io_error)?;
    if metadata.is_dir() {
        return Ok(list(list_dir(path)?, page));
    }
    if !metadata.is_file() {
        return Err(ReadError::PathKind { resource: resource.to_string(), expected: "a file or directory" });
    }
    let size = metadata.len();
    let mut file = std::fs::File::open(path).map_err(io_error)?;
    let first = read_range(&mut file, FIRST_CHUNK).map_err(io_error)?;
    let media = crate::mime::detect(&first);
    if MEDIA_MIMES.contains(&media) {
        if size > MAX_MEDIA_INGEST_BYTES {
            return Err(ReadError::MediaLimit { resource: resource.to_string() });
        }
        let whole = std::fs::read(path).map_err(io_error)?;
        return Ok(ReadOutput::File {
            uri: file_uri(path),
            name: util::basename(&path.to_string_lossy()),
            content: base64::engine::general_purpose::STANDARD.encode(whole),
            base64: true,
            mime: media.to_string(),
        });
    }
    let paged = size as usize > MAX_READ_BYTES || page.offset.is_some() || page.limit.is_some();
    if !paged {
        if first.contains(&0) {
            return Err(ReadError::Binary { resource: resource.to_string() });
        }
        let content = text_decode(&first).split('\n').map(clamp_line).collect::<Vec<_>>().join("\n");
        return Ok(ReadOutput::File {
            uri: file_uri(path),
            name: util::basename(&path.to_string_lossy()),
            content,
            base64: false,
            mime: crate::mime::lookup(path),
        });
    }

    let offset = page.offset.filter(|o| *o != 0).unwrap_or(1);
    let limit = page.limit.filter(|l| *l != 0).unwrap_or(MAX_READ_LINES).min(MAX_READ_LINES);
    let mut bytes = first;
    let mut newlines = bytes.iter().filter(|&&b| b == b'\n').count();
    let mut ended = false;
    loop {
        let eof = ended || bytes.len() as u64 >= size;
        if newlines >= offset - 1 || eof {
            let start = if offset == 1 { 0 } else { nth_newline(&bytes, offset - 1).unwrap_or(bytes.len()) };
            if let Some(result) = text_page(&bytes[start..], eof, Page { offset: None, limit: Some(limit) }) {
                let translated = PageResult {
                    next: result.next.map(|n| offset + n - 1),
                    offset,
                    consumed: start + result.consumed,
                    entries: result.entries,
                };
                let binary = bytes[..translated.consumed.min(bytes.len())].contains(&0);
                return make_text_page(path, resource, translated, binary, line_count(newlines, bytes.last().copied()));
            }
        }
        let next = read_range(&mut file, FIRST_CHUNK).map_err(io_error)?;
        if next.is_empty() {
            ended = true;
            continue;
        }
        newlines += next.iter().filter(|&&b| b == b'\n').count();
        bytes.extend_from_slice(&next);
    }
}

/// `toModelContent(path, offset, output)`.
pub fn to_model_content(path: &str, offset: Option<usize>, output: &ReadOutput) -> Vec<ToolContent> {
    match output {
        ReadOutput::File { content, base64: true, mime, .. } => vec![
            ToolContent::Text {
                text: if mime == "application/pdf" { "PDF read successfully".into() } else { "Image read successfully".into() },
            },
            ToolContent::File { uri: format!("data:{mime};base64,{content}"), mime: mime.clone(), name: Some(path.to_string()) },
        ],
        ReadOutput::ListPage { entries, truncated, next } => {
            let start = offset.filter(|o| *o != 0).unwrap_or(1);
            let mut lines = vec![if entries.is_empty() {
                format!("Read directory {path}, 0 entries")
            } else {
                format!("Read directory {path}, entries {start}-{}", start + entries.len() - 1)
            }];
            lines.extend(entries.iter().map(|e| e.path.clone()));
            if *truncated {
                if let Some(next) = next {
                    lines.push(format!("[Output truncated. Continue reading with offset: {next}]"));
                }
            }
            vec![ToolContent::Text { text: lines.join("\n") }]
        }
        ReadOutput::File { content, .. } | ReadOutput::TextPage { content, .. } => {
            let (start, text) = match output {
                ReadOutput::TextPage { offset, .. } => (*offset, content.as_str()),
                _ => (1, content.strip_suffix('\n').unwrap_or(content)),
            };
            let items: Vec<&str> = if content.is_empty() { Vec::new() } else { text.split('\n').collect() };
            let mut lines = vec![if items.is_empty() {
                format!("Read file {path}, 0 lines")
            } else {
                format!("Read file {path}, lines {start}-{}", start + items.len() - 1)
            }];
            lines.extend(items.iter().enumerate().map(|(i, l)| format!("{}: {l}", start + i)));
            if let ReadOutput::TextPage { truncated: true, next: Some(next), .. } = output {
                lines.push(format!("[Output truncated. Continue reading with offset: {next}]"));
            }
            vec![ToolContent::Text { text: lines.join("\n") }]
        }
    }
}

#[derive(Deserialize)]
struct Input {
    path: String,
    offset: Option<usize>,
    limit: Option<usize>,
}

pub struct ReadTool;

fn canonical(name: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    name.nfc()
        .map(|c| match c {
            '\u{00a0}' | '\u{202f}' => ' ',
            '\u{2018}' | '\u{2019}' => '\'',
            other => other,
        })
        .collect()
}

fn alternate_path(absolute: &Path) -> Option<PathBuf> {
    let base = canonical(&absolute.file_name()?.to_string_lossy());
    let dir = util::dirname(absolute);
    let matches: Vec<String> = list_dir(&dir)
        .ok()?
        .into_iter()
        .filter(|(name, kind)| *kind == ListType::File && canonical(name) == base)
        .map(|(name, _)| name)
        .collect();
    if matches.len() != 1 {
        return None;
    }
    Some(dir.join(&matches[0]))
}

fn missing(input: &str, absolute: &Path) -> ToolError {
    let base = util::basename(input).to_lowercase();
    let input_dir = util::dirname_str(input);
    let suggestions: Vec<String> = std::fs::read_dir(util::dirname(absolute))
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|entry| {
                    let candidate = entry.to_lowercase();
                    candidate.contains(&base) || base.contains(&candidate)
                })
                .map(|entry| util::join_str(&input_dir, &entry))
                .take(3)
                .collect()
        })
        .unwrap_or_default();
    if suggestions.is_empty() {
        ToolError::new(format!("File not found: {input}"))
    } else {
        ToolError::new(format!("File not found: {input}\n\nDid you mean one of these?\n{}", suggestions.join("\n")))
    }
}

fn map_read_error(error: ReadError, input_path: &str) -> ToolError {
    match &error {
        ReadError::Binary { .. } | ReadError::MediaLimit { .. } | ReadError::OffsetOutOfRange { .. } | ReadError::PathKind { .. } => {
            let message = error.message();
            ToolError::with_cause(message.clone(), ErrorCause::Error(message))
        }
        other => ToolError::with_cause(format!("Unable to read {input_path}"), ErrorCause::Error(other.message())),
    }
}

/// Discover nested AGENTS.md files between the read target and the Location root.
fn discover_agents(target: &Path, directory: &Path, listing: bool) -> Vec<PathBuf> {
    let resolved = PathBuf::from(crate::file_mutation::fs_resolve(target));
    let root = PathBuf::from(crate::file_mutation::fs_resolve(directory));
    let mut current = if listing { resolved.clone() } else { util::dirname(&resolved) };
    let mut found = Vec::new();
    loop {
        let candidate = current.join("AGENTS.md");
        if candidate.is_file() {
            found.push(candidate);
        }
        if current == root {
            break;
        }
        match current.parent() {
            Some(parent) if parent != current => current = parent.to_path_buf(),
            _ => break,
        }
    }
    found
        .into_iter()
        .map(|f| PathBuf::from(crate::file_mutation::fs_resolve(&f)))
        .filter(|file| !util::contains(&util::dirname(file), &root) && *file != resolved)
        .collect()
}

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn options(&self) -> ToolOptions {
        ToolOptions::direct()
    }

    fn definition(&self, _ctx: &DefinitionContext) -> ToolDefinition {
        super::definition("read", DESCRIPTION, include_str!("../../schemas/read.json"))
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolResult, ToolError> {
        let input: Input = decode(input)?;
        let leaf = format!("Unable to read {}", input.path);
        let page = Page { offset: input.offset, limit: input.limit };
        let requested = file_access::authorize_read(&ctx, &input.path, None).await.map_err(super::perm(leaf.clone()))?;
        let (output, target, shown): (ReadOutput, Target, String) = match read(&requested.absolute, &requested.resource, page) {
            Ok(output) => (output, requested.clone(), input.path.clone()),
            Err(ReadError::NotFound) => {
                let Some(alternate) = alternate_path(&requested.absolute) else {
                    return Err(missing(&input.path, &requested.absolute));
                };
                let target = file_access::authorize_read(&ctx, &alternate.to_string_lossy(), Some(&requested))
                    .await
                    .map_err(super::perm(leaf.clone()))?;
                match read(&target.absolute, &target.resource, page) {
                    Ok(ReadOutput::ListPage { .. }) | Err(ReadError::NotFound) => return Err(missing(&input.path, &requested.absolute)),
                    Ok(output) => {
                        let shown = util::join_str(&util::dirname_str(&input.path), &util::basename(&alternate.to_string_lossy()));
                        (output, target, shown)
                    }
                    Err(error) => return Err(map_read_error(error, &input.path)),
                }
            }
            Err(error) => return Err(map_read_error(error, &input.path)),
        };
        if target.external_directory.is_none() {
            let candidates = discover_agents(&target.absolute, &ctx.directory, matches!(output, ReadOutput::ListPage { .. }));
            if !candidates.is_empty() {
                let _ = ctx.host.load_instructions(&ctx.session_id, candidates).await;
            }
        }
        if let ReadOutput::File { base64: true, mime, .. } = &output {
            if !MEDIA_MIMES.contains(&mime.as_str()) {
                return Err(map_read_error(ReadError::Binary { resource: target.resource.clone() }, &input.path));
            }
        }
        let truncated = match &output {
            ReadOutput::File { .. } => false,
            ReadOutput::TextPage { truncated, .. } | ReadOutput::ListPage { truncated, .. } => *truncated,
        };
        Ok(ToolResult {
            content: to_model_content(&shown, input.offset, &output),
            output: Some(output.to_json()),
            metadata: Some(meta(vec![("truncated", Value::Bool(truncated))])),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(output: &ReadOutput, path: &str, offset: Option<usize>) -> String {
        match &to_model_content(path, offset, output)[0] {
            ToolContent::Text { text } => text.clone(),
            _ => panic!(),
        }
    }

    #[test]
    fn small_file_unpaged() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "one\ntwo\r\nthree\n").unwrap();
        let out = read(&file, "a.txt", Page::default()).unwrap();
        assert_eq!(text(&out, "a.txt", None), "Read file a.txt, lines 1-3\n1: one\n2: two\r\n3: three");
        std::fs::write(&file, "").unwrap();
        let out = read(&file, "a.txt", Page::default()).unwrap();
        assert_eq!(text(&out, "a.txt", None), "Read file a.txt, 0 lines");
    }

    #[test]
    fn paged_reading() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        let body: String = (1..=10).map(|i| format!("line{i}\r\n")).collect();
        std::fs::write(&file, &body).unwrap();
        let out = read(&file, "a.txt", Page { offset: Some(3), limit: Some(2) }).unwrap();
        assert_eq!(
            text(&out, "a.txt", Some(3)),
            "Read file a.txt, lines 3-4\n3: line3\n4: line4\n[Output truncated. Continue reading with offset: 5]"
        );
        let out = read(&file, "a.txt", Page { offset: Some(9), limit: None }).unwrap();
        assert_eq!(text(&out, "a.txt", Some(9)), "Read file a.txt, lines 9-10\n9: line9\n10: line10");
        let err = read(&file, "a.txt", Page { offset: Some(20), limit: None }).unwrap_err();
        assert_eq!(err.message(), "Offset 20 is out of range for this file (10 lines)");
        let out = read(&file, "a.txt", Page { offset: Some(10), limit: Some(1) }).unwrap();
        assert!(matches!(out, ReadOutput::TextPage { truncated: false, next: None, .. }));
    }

    #[test]
    fn large_file_paging_across_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("big.txt");
        let body: String = (1..=60_000).map(|i| format!("row {i}\n")).collect();
        std::fs::write(&file, &body).unwrap();
        let out = read(&file, "big.txt", Page::default()).unwrap();
        let ReadOutput::TextPage { next, truncated, offset, .. } = &out else { panic!() };
        assert_eq!((*offset, *truncated, *next), (1, true, Some(2001)));
        let out = read(&file, "big.txt", Page { offset: Some(59_999), limit: None }).unwrap();
        assert_eq!(text(&out, "big.txt", Some(59_999)), "Read file big.txt, lines 59999-60000\n59999: row 59999\n60000: row 60000");
        let err = read(&file, "big.txt", Page { offset: Some(70_000), limit: None }).unwrap_err();
        assert_eq!(err.message(), "Offset 70000 is out of range for this file (60000 lines)");
    }

    #[test]
    fn byte_cap_and_line_clamp() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("wide.txt");
        let long = "x".repeat(2500);
        let body: String = (0..40).map(|_| format!("{long}\n")).collect();
        std::fs::write(&file, &body).unwrap();
        let out = read(&file, "wide.txt", Page::default()).unwrap();
        let ReadOutput::TextPage { content, next, .. } = &out else { panic!() };
        let lines: Vec<&str> = content.split('\n').collect();
        assert_eq!(lines[0], format!("{}{MAX_LINE_SUFFIX}", "x".repeat(2000)));
        // each clamped line is 2034 bytes (+1 newline): 25 lines fit in 51200
        assert_eq!(lines.len(), 25);
        assert_eq!(*next, Some(26));
    }

    #[test]
    fn binary_and_media() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("bin");
        std::fs::write(&file, b"ab\0cd").unwrap();
        assert_eq!(read(&file, "bin", Page::default()).unwrap_err().message(), "Cannot read binary file: bin");
        let png = dir.path().join("i.png");
        std::fs::write(&png, [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 1, 2]).unwrap();
        let out = read(&png, "i.png", Page::default()).unwrap();
        let content = to_model_content("i.png", None, &out);
        assert_eq!(content[0], ToolContent::Text { text: "Image read successfully".into() });
        assert!(matches!(&content[1], ToolContent::File { mime, name: Some(n), .. } if mime == "image/png" && n == "i.png"));
    }

    #[test]
    fn directory_listing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("zdir")).unwrap();
        std::fs::create_dir(dir.path().join("adir")).unwrap();
        std::fs::write(dir.path().join("b.txt"), "").unwrap();
        std::fs::write(dir.path().join("A.txt"), "").unwrap();
        let out = read(dir.path(), "x", Page::default()).unwrap();
        assert_eq!(text(&out, "x", None), "Read directory x, entries 1-4\nadir/\nzdir/\nA.txt\nb.txt");
        let out = read(dir.path(), "x", Page { offset: Some(2), limit: Some(2) }).unwrap();
        assert_eq!(text(&out, "x", Some(2)), "Read directory x, entries 2-3\nzdir/\nA.txt\n[Output truncated. Continue reading with offset: 4]");
        let out = read(dir.path(), "x", Page { offset: Some(9), limit: None }).unwrap();
        assert_eq!(text(&out, "x", Some(9)), "Read directory x, 0 entries");
    }

    #[test]
    fn missing_suggestions() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.json"), "").unwrap();
        std::fs::write(dir.path().join("other.md"), "").unwrap();
        let err = missing("src/config", &dir.path().join("config"));
        assert_eq!(err.message, "File not found: src/config\n\nDid you mean one of these?\nsrc/config.json");
        let err = missing("zzz", &dir.path().join("zzz"));
        assert_eq!(err.message, "File not found: zzz");
        std::fs::write(dir.path().join("it’s.txt"), "x").unwrap();
        assert_eq!(alternate_path(&dir.path().join("it's.txt")).unwrap(), dir.path().join("it’s.txt"));
    }

    #[test]
    fn agents_discovery() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::write(root.join("AGENTS.md"), "root").unwrap();
        std::fs::write(root.join("a/AGENTS.md"), "a").unwrap();
        std::fs::write(root.join("a/b/x.txt"), "x").unwrap();
        let found = discover_agents(&root.join("a/b/x.txt"), root, false);
        assert_eq!(found, vec![PathBuf::from(crate::file_mutation::fs_resolve(&root.join("a/AGENTS.md")))]);
        assert!(discover_agents(&root.join("a/AGENTS.md"), root, false).is_empty());
    }
}
