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
/// Lines an offset past the end returns from the file's end.
const TAIL_LINES: usize = 100;
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
    PathKind { resource: String, expected: &'static str },
    DirectoryOffset { resource: String },
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
            ReadError::PathKind { resource, expected } => format!("Path is not {expected}: {resource}"),
            ReadError::DirectoryOffset { resource } => {
                format!("A negative offset only works for files, and {resource} is a directory")
            }
        }
    }
}

/// `PageInput`: a negative `offset` counts back from the file's last line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Page {
    pub offset: Option<i64>,
    pub limit: Option<usize>,
}

impl Page {
    /// `page.offset || 1`.
    fn start(&self) -> i64 {
        self.offset.filter(|o| *o != 0).unwrap_or(1)
    }

    /// `Math.min(page.limit || MAX_READ_LINES, MAX_READ_LINES)`.
    fn lines(&self) -> usize {
        self.limit.filter(|l| *l != 0).unwrap_or(MAX_READ_LINES).min(MAX_READ_LINES)
    }

    /// `Math.min(page.limit || TAIL_LINES, TAIL_LINES)`.
    fn tail(&self) -> usize {
        self.limit.filter(|l| *l != 0).unwrap_or(TAIL_LINES).min(TAIL_LINES)
    }
}

/// `clampLine`: the cut never splits a surrogate pair (#62575), since `utf16_prefix` keeps whole characters.
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

/// `textOffset(tree, newline)`: the byte after the `newline`-th newline, or the end.
fn text_offset(bytes: &[u8], newline: usize) -> usize {
    if newline == 0 { 0 } else { nth_newline(bytes, newline).unwrap_or(bytes.len()) }
}

/// A trailing newline ends the last line instead of starting an empty one.
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
    let offset = page.start().max(1) as usize;
    let limit = page.lines();
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
    // An offset past the end returns the file's last lines that fit, ending at the last line.
    if eof && offset > available.len().max(1) {
        let from = available.len().saturating_sub(page.tail());
        for value in available[from..].iter().rev() {
            let text = clamp_line(value);
            size += text.len() + usize::from(!entries.is_empty());
            if size > MAX_READ_BYTES {
                break;
            }
            entries.insert(0, text);
        }
        let offset = available.len() - entries.len() + 1;
        return Some(PageResult { entries, offset, next: None, consumed: bytes.len() });
    }
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

fn make_text_page(mime: &str, resource: &str, result: PageResult, binary: bool) -> Result<ReadOutput, ReadError> {
    if binary {
        return Err(ReadError::Binary { resource: resource.to_string() });
    }
    Ok(ReadOutput::TextPage {
        content: result.entries.join("\n"),
        mime: mime.to_string(),
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

/// `list(items, page, sep)`: directories end in `sep` (`path.sep`, or `/` for `skill://` files).
fn list(items: Vec<(String, ListType)>, page: Page, sep: char) -> ReadOutput {
    let offset = page.start().max(1) as usize;
    let limit = page.lines();
    let mut visible: Vec<ListEntry> = items
        .into_iter()
        .map(|(name, kind)| ListEntry { path: if kind == ListType::Directory { format!("{name}{sep}") } else { name }, kind })
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

fn read_range(file: &mut impl std::io::Read, length: usize) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut buffer = Vec::with_capacity(length);
    file.take(length as u64).read_to_end(&mut buffer)?;
    Ok(buffer)
}

/// How a read file names itself in [`ReadOutput`]: its `uri`, `name` and the mime type of that name.
struct FileName {
    uri: String,
    name: String,
    mime: String,
}

/// `ReadToolFileSystem.read`.
pub fn read(path: &Path, resource: &str, page: Page) -> Result<ReadOutput, ReadError> {
    let metadata = std::fs::metadata(path).map_err(io_error)?;
    if metadata.is_dir() {
        if page.offset.is_some_and(|o| o < 0) {
            return Err(ReadError::DirectoryOffset { resource: resource.to_string() });
        }
        return Ok(list(list_dir(path)?, page, std::path::MAIN_SEPARATOR));
    }
    if !metadata.is_file() {
        return Err(ReadError::PathKind { resource: resource.to_string(), expected: "a file or directory" });
    }
    let file = std::fs::File::open(path).map_err(io_error)?;
    let name = FileName { uri: file_uri(path), name: util::basename(&path.to_string_lossy()), mime: crate::mime::lookup(path) };
    read_file(file, metadata.len(), || std::fs::read(path).map_err(io_error), name, resource, page)
}

/// The file half of `ReadToolFileSystem.read`: `reader` yields the file's bytes in order and `size` is its length.
fn read_file(
    mut reader: impl std::io::Read,
    size: u64,
    whole: impl FnOnce() -> Result<Vec<u8>, ReadError>,
    name: FileName,
    resource: &str,
    page: Page,
) -> Result<ReadOutput, ReadError> {
    let first = read_range(&mut reader, FIRST_CHUNK).map_err(io_error)?;
    let media = crate::mime::detect(&first);
    if MEDIA_MIMES.contains(&media) {
        if size > MAX_MEDIA_INGEST_BYTES {
            return Err(ReadError::MediaLimit { resource: resource.to_string() });
        }
        let whole = whole()?;
        return Ok(ReadOutput::File {
            uri: name.uri,
            name: name.name,
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
        return Ok(ReadOutput::File { uri: name.uri, name: name.name, content, base64: false, mime: name.mime });
    }
    if first.len() as u64 >= size {
        return complete_page(&name.mime, resource, &first, page);
    }

    let offset = page.start();
    let limit = page.lines();
    let mut bytes = first;
    let mut newlines = bytes.iter().filter(|&&b| b == b'\n').count();
    let mut ended = false;
    loop {
        let eof = ended || bytes.len() as u64 >= size;
        let total = line_count(newlines, bytes.last().copied());
        if eof && offset > total as i64 {
            let skipped = total.saturating_sub(page.tail());
            // Decode from the newline ending the last skipped line, so a byte order mark starting the tail stays
            // content. That empty first line is never in the tail.
            let start = text_offset(&bytes, skipped).saturating_sub(1);
            let Some(tail) = text_page(&bytes[start..], true, page) else {
                return Err(ReadError::Failed("Read page did not settle for a complete file".into()));
            };
            let tail = PageResult { offset: skipped.saturating_sub(1) + tail.offset, ..tail };
            return make_text_page(&name.mime, resource, tail, bytes.contains(&0));
        }
        if eof || (offset > 0 && newlines as i64 >= offset - 1) {
            // A negative offset only resolves at EOF, counting back from the last line.
            let from = if offset < 0 { (total as i64 + offset + 1).max(1) as usize } else { offset as usize };
            let start = text_offset(&bytes, from - 1);
            // Decode only the requested lines plus one byte, enough to tell whether more follow.
            let end = bytes.len().min(text_offset(&bytes, from - 1 + limit) + 1);
            let eof_page = eof && end == bytes.len();
            if let Some(result) = text_page(&bytes[start..end], eof_page, Page { offset: None, limit: Some(limit) }) {
                let translated = PageResult {
                    next: result.next.map(|n| from + n - 1),
                    offset: from,
                    consumed: start + result.consumed,
                    entries: result.entries,
                };
                let binary = bytes[..translated.consumed.min(bytes.len())].contains(&0);
                return make_text_page(&name.mime, resource, translated, binary);
            }
        }
        let next = read_range(&mut reader, FIRST_CHUNK).map_err(io_error)?;
        if next.is_empty() {
            ended = true;
            continue;
        }
        newlines += next.iter().filter(|&&b| b == b'\n').count();
        bytes.extend_from_slice(&next);
    }
}

/// `completePage`: a file whose first chunk holds all of it.
fn complete_page(mime: &str, resource: &str, bytes: &[u8], page: Page) -> Result<ReadOutput, ReadError> {
    let total = line_count(bytes.iter().filter(|&&b| b == b'\n').count(), bytes.last().copied());
    let page = match page.offset {
        Some(offset) if offset < 0 => Page { offset: Some((total as i64 + offset + 1).max(1)), ..page },
        _ => page,
    };
    let Some(result) = text_page(bytes, true, page) else {
        return Err(ReadError::Failed("Read page did not settle for a complete file".into()));
    };
    let binary = bytes[..result.consumed.min(bytes.len())].contains(&0);
    make_text_page(mime, resource, result, binary)
}

/// The in-memory files a `skill://` read sees (upstream writes them to a memory driver): `SKILL.md` and the
/// skill's virtual files, by path from the skill's root.
pub struct VirtualFiles {
    files: Vec<(String, Vec<u8>)>,
}

impl VirtualFiles {
    pub fn new(content: &str, extra: &[(String, String)]) -> Self {
        let mut files = vec![("SKILL.md".to_string(), content.as_bytes().to_vec())];
        for (rel, text) in extra {
            match files.iter_mut().find(|(p, _)| p == rel) {
                Some(slot) => slot.1 = text.as_bytes().to_vec(),
                None => files.push((rel.clone(), text.as_bytes().to_vec())),
            }
        }
        VirtualFiles { files }
    }

    /// The directory `dir` ("" for the root): its children, or `None` when no file lies under it.
    fn list(&self, dir: &str) -> Option<Vec<(String, ListType)>> {
        let prefix = if dir.is_empty() { String::new() } else { format!("{dir}/") };
        let mut out: Vec<(String, ListType)> = Vec::new();
        let mut found = dir.is_empty();
        for (path, _) in &self.files {
            let Some(rest) = path.strip_prefix(&prefix) else { continue };
            found = true;
            let (name, kind) = match rest.split_once('/') {
                Some((name, _)) => (name, ListType::Directory),
                None => (rest, ListType::File),
            };
            if !out.iter().any(|(n, _)| n == name) {
                out.push((name.to_string(), kind));
            }
        }
        found.then_some(out)
    }

    /// `ReadToolFileSystem.read` over the memory driver; `target` is `/` or `/<path>`.
    pub fn read(&self, target: &str, resource: &str, page: Page) -> Result<ReadOutput, ReadError> {
        let rel = target.trim_start_matches('/');
        if let Some((_, bytes)) = self.files.iter().find(|(p, _)| p == rel) {
            let base = rel.rsplit('/').next().unwrap_or(rel);
            let name = FileName { uri: resource.to_string(), name: base.to_string(), mime: crate::mime::lookup(Path::new(base)) };
            let whole = bytes.clone();
            let reader = std::io::Cursor::new(bytes.as_slice());
            return read_file(reader, bytes.len() as u64, move || Ok(whole), name, resource, page);
        }
        let Some(items) = self.list(rel) else { return Err(ReadError::NotFound) };
        if page.offset.is_some_and(|o| o < 0) {
            return Err(ReadError::DirectoryOffset { resource: resource.to_string() });
        }
        Ok(list(items, page, '/'))
    }
}

/// `toModelContent(path, offset, output)`.
pub fn to_model_content(path: &str, offset: Option<i64>, output: &ReadOutput) -> Vec<ToolContent> {
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
                format!("Read directory {path}, entries {start}-{}", start + entries.len() as i64 - 1)
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
            // A page that starts before the requested offset is the file's tail, returned for an offset past the end.
            let total = (start + items.len()) as i64 - 1;
            let past_end = matches!(output, ReadOutput::TextPage { .. }) && offset.is_some_and(|o| (start as i64) < o);
            let mut header = if items.is_empty() {
                format!("Read file {path}, 0 lines")
            } else {
                format!("Read file {path}, lines {start}-{total}")
            };
            if let Some(offset) = offset.filter(|_| past_end) {
                let unit = if total == 1 { "line" } else { "lines" };
                header.push_str(&format!(" (offset {offset} is past the end; the file has {total} {unit})"));
            }
            let mut lines = vec![header];
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
    offset: Option<i64>,
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

/// The environment's directory listing, classified like upstream's `listing`: `ENOENT`/`ENOTDIR` mean the folder
/// is missing, anything else (a file, a permission error) is unknown.
enum Listing {
    Found(Vec<String>),
    Missing,
    Unknown,
}

fn listing(path: &Path) -> Listing {
    let missing = |e: &std::io::Error| e.kind() == std::io::ErrorKind::NotFound || e.raw_os_error() == Some(20);
    match std::fs::metadata(path) {
        Err(e) if missing(&e) => return Listing::Missing,
        Err(_) => return Listing::Unknown,
        Ok(m) if !m.is_dir() => return Listing::Unknown,
        Ok(_) => {}
    }
    match std::fs::read_dir(path) {
        Ok(entries) => Listing::Found(entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()),
        Err(e) if missing(&e) => Listing::Missing,
        Err(_) => Listing::Unknown,
    }
}

/// Node's `path.extname` of a file name (its `preDotState` scan): from the last `.`, except for names like
/// `.bashrc` and `..`.
fn extname(name: &str) -> &str {
    let bytes = name.as_bytes();
    let mut start_dot: Option<usize> = None;
    let mut pre_dot_state = 0i8;
    for index in (0..bytes.len()).rev() {
        if bytes[index] == b'.' {
            if start_dot.is_none() {
                start_dot = Some(index);
            } else if pre_dot_state != 1 {
                pre_dot_state = 1;
            }
        } else if start_dot.is_some() {
            pre_dot_state = -1;
        }
    }
    match start_dot {
        Some(dot) if pre_dot_state != 0 && !(pre_dot_state == 1 && dot + 1 == bytes.len() && dot == 1) => &name[dot..],
        _ => "",
    }
}

/// `stem(name)`: the name without its extension, or the whole name when that would leave nothing.
fn stem(name: &str) -> &str {
    let base = &name[..name.len() - extname(name).len()];
    if base.is_empty() { name } else { base }
}

/// Levenshtein distance over UTF-16 code units (`editDistance`).
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<u16> = a.encode_utf16().collect();
    let b: Vec<u16> = b.encode_utf16().collect();
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut current = vec![i; b.len() + 1];
        for j in 1..=b.len() {
            current[j] = (previous[j] + 1).min(current[j - 1] + 1).min(previous[j - 1] + usize::from(a[i - 1] != b[j - 1]));
        }
        previous = current;
    }
    previous[b.len()]
}

/// Ranks a folder's entries against a missing name: same name ignoring case, then same stem with another
/// extension, then one containing the other, then a typo in the stem (`similar`).
fn similar(names: &[String], requested: &str) -> Vec<String> {
    let name = requested.to_lowercase();
    let wanted = stem(&name);
    let wanted_len = js::utf16_len(wanted);
    let typos = if wanted_len < 3 {
        0
    } else if wanted_len < 5 {
        1
    } else {
        2
    };
    let tier = |candidate: &str| -> Option<u8> {
        if candidate == name {
            return Some(1);
        }
        // Names that only differ in look-alike characters are what alternatePath recovers or deliberately skips.
        if canonical(candidate) == canonical(&name) {
            return None;
        }
        let found = stem(candidate);
        if found == wanted {
            return Some(2);
        }
        let found_len = js::utf16_len(found);
        if found_len.min(wanted_len) >= 3
            && (candidate.contains(name.as_str()) || name.contains(candidate) || found.contains(wanted) || wanted.contains(found))
        {
            return Some(3);
        }
        // Folders can be huge, so skip the distance when the lengths alone rule out a typo.
        if typos > 0 && found_len.abs_diff(wanted_len) <= typos && edit_distance(found, wanted) <= typos {
            return Some(4);
        }
        None
    };
    let mut ranked: Vec<(&String, u8, usize)> = names
        .iter()
        .filter(|entry| entry.as_str() != requested)
        .filter_map(|entry| {
            let lower = entry.to_lowercase();
            tier(&lower).map(|rank| (entry, rank, edit_distance(&lower, &name)))
        })
        .collect();
    ranked.sort_by(|a, b| a.1.cmp(&b.1).then(a.2.cmp(&b.2)).then_with(|| js::locale_compare(a.0, b.0)));
    ranked.into_iter().take(3).map(|(entry, _, _)| entry.clone()).collect()
}

/// Node's `path.isAbsolute` for the platform.
fn is_absolute(input: &str) -> bool {
    Path::new(input).is_absolute() || (cfg!(windows) && (input.starts_with('/') || input.starts_with('\\')))
}

/// A path written from the wrong root, such as /Users/me/src/a.ts for a location at /Users/me/repo, usually means
/// <location>/src/a.ts, so check that one path and nothing else (`reroot`).
async fn reroot(ctx: &ToolContext, input: &str, requested: &Target) -> Vec<String> {
    let root = &ctx.directory;
    let parent = util::dirname(root);
    if util::contains(root, &requested.absolute) || !util::contains(&parent, &requested.absolute) {
        return Vec::new();
    }
    let candidate = root.join(util::relative(&parent, &requested.absolute));
    let target = file_access::resolve(ctx, &candidate.to_string_lossy(), None);
    if std::fs::metadata(&target.absolute).is_err() {
        return Vec::new();
    }
    let request = ctx.permission("read", vec![target.resource.clone()], Some(vec!["*".into()]), None);
    if !ctx.host.permission_allows(request).await {
        return Vec::new();
    }
    vec![if is_absolute(input) || input.starts_with("~/") { target.absolute.to_string_lossy().into_owned() } else { target.resource }]
}

fn did_you_mean(suggestions: &[String]) -> Option<String> {
    (!suggestions.is_empty()).then(|| format!("Did you mean one of these?\n{}", suggestions.iter().take(3).cloned().collect::<Vec<_>>().join("\n")))
}

/// A missing folder usually means a removed worktree, a moved directory, or a relative path resolved from the
/// wrong place, so name the first missing folder and its nearest existing parent.
fn missing_folder(input: &str, folder: &Path, moved: Vec<String>) -> ToolError {
    let mut child = folder.to_path_buf();
    let mut parent = util::dirname(folder);
    while parent != child {
        match listing(&parent) {
            Listing::Unknown => break,
            Listing::Found(names) => {
                let child_name = child.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let mut suggestions = moved;
                suggestions.extend(similar(&names, &child_name).into_iter().map(|entry| parent.join(entry).to_string_lossy().into_owned()));
                let mut parts = vec![
                    format!("File not found: {input}"),
                    format!("The folder {} does not exist. The nearest folder that does is {}.", child.display(), parent.display()),
                ];
                parts.extend(did_you_mean(&suggestions));
                return ToolError::new(parts.join("\n\n"));
            }
            Listing::Missing => {
                child = parent.clone();
                parent = util::dirname(&parent);
            }
        }
    }
    let mut parts = vec![format!("File not found: {input}")];
    parts.extend(did_you_mean(&moved));
    ToolError::new(parts.join("\n\n"))
}

/// `missing`: the not-found failure with the suggestions that might be meant (the rerooted path first).
fn missing_with(input: &str, absolute: &Path, moved: Vec<String>) -> ToolError {
    let folder = util::dirname(absolute);
    let mut suggestions = moved;
    match listing(&folder) {
        Listing::Missing => return missing_folder(input, &folder, suggestions),
        Listing::Found(names) => {
            let input_dir = util::dirname_str(input);
            suggestions.extend(similar(&names, &util::basename(input)).into_iter().map(|entry| util::join_str(&input_dir, &entry)));
        }
        Listing::Unknown => {}
    }
    let mut parts = vec![format!("File not found: {input}")];
    parts.extend(did_you_mean(&suggestions));
    ToolError::new(parts.join("\n\n"))
}

async fn missing(ctx: &ToolContext, input: &str, requested: &Target) -> ToolError {
    let moved = reroot(ctx, input, requested).await;
    missing_with(input, &requested.absolute, moved)
}

fn map_read_error(error: ReadError, input_path: &str) -> ToolError {
    match &error {
        ReadError::Binary { .. } | ReadError::MediaLimit { .. } | ReadError::PathKind { .. } | ReadError::DirectoryOffset { .. } => {
            let message = error.message();
            ToolError::with_cause(message.clone(), ErrorCause::Error(message))
        }
        other => ToolError::with_cause(format!("Unable to read {input_path}"), ErrorCause::Error(other.message())),
    }
}

/// `SkillDiscovery.isSafeSegment`.
fn safe_segment(value: &str) -> bool {
    !value.is_empty() && value != "." && value != ".." && !value.contains('/') && !value.contains('\\') && !value.contains('\0')
}

/// A parsed `skill://<id>[/<file>]` URI: the skill, the file's path in its memory filesystem, and the canonical URI
/// that permission rules match (so `skill://id/file/` can't slip past a rule for `skill://id/file`).
#[derive(Debug, PartialEq, Eq)]
struct SkillUri {
    id: String,
    target: String,
    uri: String,
}

fn parse_skill_uri(value: &str) -> Option<SkillUri> {
    let trimmed = value["skill://".len()..].trim_end_matches('/');
    let (id, rest) = trimmed.split_once('/').unwrap_or((trimmed, ""));
    if !safe_segment(id) || (!rest.is_empty() && !rest.split('/').all(safe_segment)) {
        return None;
    }
    Some(SkillUri {
        id: id.to_string(),
        target: format!("/{rest}"),
        uri: if rest.is_empty() { format!("skill://{id}") } else { format!("skill://{id}/{rest}") },
    })
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

fn result(shown: &str, offset: Option<i64>, output: ReadOutput, resource: &str, input_path: &str) -> Result<ToolResult, ToolError> {
    if let ReadOutput::File { base64: true, mime, .. } = &output {
        if !MEDIA_MIMES.contains(&mime.as_str()) {
            return Err(map_read_error(ReadError::Binary { resource: resource.to_string() }, input_path));
        }
    }
    let truncated = match &output {
        ReadOutput::File { .. } => false,
        ReadOutput::TextPage { truncated, .. } | ReadOutput::ListPage { truncated, .. } => *truncated,
    };
    Ok(ToolResult {
        content: to_model_content(shown, offset, &output),
        output: Some(output.to_json()),
        metadata: Some(meta(vec![("truncated", Value::Bool(truncated))])),
    })
}

impl ReadTool {
    /// A `skill://` URI reads the skill's in-memory files after the `skill` and `read` approvals; nothing is
    /// discovered around it.
    async fn read_skill(&self, input: &Input, ctx: &ToolContext, page: Page) -> Result<ToolResult, ToolError> {
        let leaf = format!("Unable to read {}", input.path);
        let not_found = || ToolError::new(format!("File not found: {}", input.path));
        let Some(parsed) = parse_skill_uri(&input.path) else { return Err(not_found()) };
        let skill = match ctx.host.get_skill(&parsed.id).await {
            Ok(Some(skill)) if skill.path.to_string_lossy().starts_with("skill://") => skill,
            Ok(_) => return Err(not_found()),
            Err(message) => return Err(ToolError::with_cause(leaf, ErrorCause::Error(message))),
        };
        super::assert(ctx, "skill", vec![skill.id.clone()], Some(vec![skill.id.clone()]), None).await.map_err(super::perm(leaf.clone()))?;
        super::assert(ctx, "read", vec![parsed.uri.clone()], Some(vec!["*".into()]), None).await.map_err(super::perm(leaf))?;
        let output = match VirtualFiles::new(&skill.content, &skill.files).read(&parsed.target, &input.path, page) {
            Ok(output) => output,
            Err(ReadError::NotFound) => return Err(not_found()),
            Err(error) => return Err(map_read_error(error, &input.path)),
        };
        result(&input.path, input.offset, output, &input.path, &input.path)
    }
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
        let page = Page { offset: input.offset, limit: input.limit };
        if input.path.starts_with("skill://") {
            return self.read_skill(&input, &ctx, page).await;
        }
        let leaf = format!("Unable to read {}", input.path);
        let requested = file_access::authorize_read(&ctx, &input.path, None).await.map_err(super::perm(leaf.clone()))?;
        let (output, target, shown): (ReadOutput, Target, String) = match read(&requested.absolute, &requested.resource, page) {
            Ok(output) => (output, requested.clone(), input.path.clone()),
            Err(ReadError::NotFound) => {
                let Some(alternate) = alternate_path(&requested.absolute) else {
                    return Err(missing(&ctx, &input.path, &requested).await);
                };
                let target = file_access::authorize_read(&ctx, &alternate.to_string_lossy(), Some(&requested))
                    .await
                    .map_err(super::perm(leaf.clone()))?;
                match read(&target.absolute, &target.resource, page) {
                    Ok(ReadOutput::ListPage { .. }) | Err(ReadError::NotFound) => return Err(missing(&ctx, &input.path, &requested).await),
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
        result(&shown, input.offset, output, &target.resource, &input.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(output: &ReadOutput, path: &str, offset: Option<i64>) -> String {
        match &to_model_content(path, offset, output)[0] {
            ToolContent::Text { text } => text.clone(),
            _ => panic!(),
        }
    }

    /// Writes `content` to `name` in `dir` and reads it like the tool would show it.
    fn lines_of(dir: &Path, name: &str, content: impl AsRef<[u8]>, page: Page) -> Vec<String> {
        let file = dir.join(name);
        std::fs::write(&file, content).unwrap();
        let out = read(&file, name, page).unwrap();
        text(&out, name, page.offset).split('\n').map(String::from).collect()
    }

    fn numbered(lines: &[String], from: usize, to: usize) -> Vec<String> {
        lines[from - 1..to].iter().enumerate().map(|(i, l)| format!("{}: {l}", from + i)).collect()
    }

    /// `${index + 1}`.padEnd(width, fill)
    fn padded(count: usize, width: usize, fill: char) -> Vec<String> {
        (1..=count).map(|i| i.to_string()).map(|s| format!("{s}{}", fill.to_string().repeat(width - s.len()))).collect()
    }

    fn page(offset: Option<i64>, limit: Option<usize>) -> Page {
        Page { offset, limit }
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
        let out = read(&file, "a.txt", page(Some(3), Some(2))).unwrap();
        assert_eq!(
            text(&out, "a.txt", Some(3)),
            "Read file a.txt, lines 3-4\n3: line3\n4: line4\n[Output truncated. Continue reading with offset: 5]"
        );
        let out = read(&file, "a.txt", page(Some(9), None)).unwrap();
        assert_eq!(text(&out, "a.txt", Some(9)), "Read file a.txt, lines 9-10\n9: line9\n10: line10");
        let out = read(&file, "a.txt", page(Some(10), Some(1))).unwrap();
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
        let out = read(&file, "big.txt", page(Some(59_999), None)).unwrap();
        assert_eq!(text(&out, "big.txt", Some(59_999)), "Read file big.txt, lines 59999-60000\n59999: row 59999\n60000: row 60000");
        let out = read(&file, "big.txt", page(Some(70_000), Some(2))).unwrap();
        assert_eq!(
            text(&out, "big.txt", Some(70_000)),
            "Read file big.txt, lines 59999-60000 (offset 70000 is past the end; the file has 60000 lines)\n59999: row 59999\n60000: row 60000"
        );
    }

    /// tool-read-filesystem.test.ts "returns the last lines when the offset is past the end" (#62147).
    #[test]
    fn past_end_returns_the_last_lines() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        assert_eq!(
            lines_of(d, "short.txt", "one\ntwo\nthree\nfour\nfive\n", page(Some(9), Some(2))),
            ["Read file short.txt, lines 4-5 (offset 9 is past the end; the file has 5 lines)", "4: four", "5: five"]
        );
        assert_eq!(
            lines_of(d, "empty.txt", "", page(Some(5), None)),
            ["Read file empty.txt, 0 lines (offset 5 is past the end; the file has 0 lines)"]
        );
        // The 50 KB cap keeps the last 51 of 100 tail lines, still ending at the last line.
        let long = padded(1_800, 1_000, 'x');
        let tail = lines_of(d, "long.txt", long.join("\n"), page(Some(2_000), Some(2_000)));
        assert_eq!(
            [tail[0].clone(), tail.last().unwrap().clone()],
            [
                "Read file long.txt, lines 1750-1800 (offset 2000 is past the end; the file has 1800 lines)".to_string(),
                format!("1800: {}", long.last().unwrap()),
            ]
        );
    }

    /// "returns the last lines of a file larger than the first chunk when the offset is past the end".
    #[test]
    fn past_end_of_a_file_larger_than_the_first_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let lines = padded(3_000, 100, 'x');
        let expected = |from: usize| numbered(&lines, from, 3_000);
        let with = |header: &str, from: usize| [vec![header.to_string()], expected(from)].concat();
        assert_eq!(
            lines_of(d, "lf.txt", lines.join("\n") + "\n", page(Some(5_000), None)),
            with("Read file lf.txt, lines 2901-3000 (offset 5000 is past the end; the file has 3000 lines)", 2_901)
        );
        assert_eq!(
            lines_of(d, "unterminated.txt", lines.join("\n"), page(Some(3_001), None)),
            with("Read file unterminated.txt, lines 2901-3000 (offset 3001 is past the end; the file has 3000 lines)", 2_901)
        );
        assert_eq!(
            lines_of(d, "crlf.txt", lines.join("\r\n") + "\r\n", page(Some(3_001), Some(5))),
            with("Read file crlf.txt, lines 2996-3000 (offset 3001 is past the end; the file has 3000 lines)", 2_996)
        );
        assert_eq!(
            lines_of(d, "crlf-unterminated.txt", lines.join("\r\n"), page(Some(4_000), Some(1))),
            with("Read file crlf-unterminated.txt, lines 3000-3000 (offset 4000 is past the end; the file has 3000 lines)", 3_000)
        );
        // A mid-file byte order mark is line content, not a decoder signature.
        let marked: Vec<String> = lines.iter().enumerate().map(|(i, l)| if i == 2_900 { format!("\u{FEFF}{l}") } else { l.clone() }).collect();
        assert_eq!(lines_of(d, "bom.txt", marked.join("\n"), page(Some(3_001), None))[1], format!("2901: \u{FEFF}{}", lines[2_900]));

        let huge = lines_of(d, "huge-last.txt", format!("{}\n{}", lines[..10].join("\n"), "z".repeat(300 * 1024)), page(Some(12), None));
        let mut want = vec!["Read file huge-last.txt, lines 1-11 (offset 12 is past the end; the file has 11 lines)".to_string()];
        want.extend(numbered(&lines, 1, 10));
        want.push(format!("11: {}... (line truncated to 2000 chars)", "z".repeat(2_000)));
        assert_eq!(huge, want);

        // Each clamped line is 2041 bytes, so 25 of them fit in the 50 KB cap.
        let wide = padded(200, 3_000, 'w');
        let capped = lines_of(d, "wide.txt", wide.join("\n") + "\n", page(Some(201), None));
        assert_eq!(capped[0], "Read file wide.txt, lines 176-200 (offset 201 is past the end; the file has 200 lines)");
        let clipped: Vec<String> =
            wide[175..].iter().enumerate().map(|(i, l)| format!("{}: {}... (line truncated to 2000 chars)", 176 + i, &l[..2_000])).collect();
        assert_eq!(capped[1..], clipped[..]);

        // Matches whole-file reads: a null byte anywhere in the file rejects a past-end read.
        let mut early = lines.join("\n").into_bytes();
        early[10] = 0;
        let file = d.join("early-nul.txt");
        std::fs::write(&file, early).unwrap();
        assert_eq!(read(&file, "early-nul.txt", page(Some(3_001), None)).unwrap_err(), ReadError::Binary { resource: "early-nul.txt".into() });
    }

    /// "starts a negative offset that many lines before the end" (#62147).
    #[test]
    fn negative_offset_counts_back_from_the_end() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let small = "one\ntwo\nthree\nfour\nfive\n";
        assert_eq!(lines_of(d, "small.txt", small, page(Some(-3), None)), ["Read file small.txt, lines 3-5", "3: three", "4: four", "5: five"]);
        assert_eq!(
            lines_of(d, "all.txt", small, page(Some(-50), None)),
            ["Read file all.txt, lines 1-5", "1: one", "2: two", "3: three", "4: four", "5: five"]
        );
        assert_eq!(
            lines_of(d, "limited.txt", small, page(Some(-4), Some(2))),
            ["Read file limited.txt, lines 2-3", "2: two", "3: three", "[Output truncated. Continue reading with offset: 4]"]
        );
        assert_eq!(lines_of(d, "crlf.txt", "one\r\ntwo\r\nthree\r\n", page(Some(-2), None)), ["Read file crlf.txt, lines 2-3", "2: two", "3: three"]);
        assert_eq!(lines_of(d, "unterminated.txt", "one\ntwo\nthree", page(Some(-1), None)), ["Read file unterminated.txt, lines 3-3", "3: three"]);
        assert_eq!(lines_of(d, "empty.txt", "", page(Some(-5), None)), ["Read file empty.txt, 0 lines"]);
    }

    /// "starts a negative offset from the end of a file larger than the first chunk".
    #[test]
    fn negative_offset_in_a_file_larger_than_the_first_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let lines = padded(3_000, 100, 'x');
        let with = |header: &str, from: usize, to: usize| [vec![header.to_string()], numbered(&lines, from, to)].concat();
        assert_eq!(lines_of(d, "lf.txt", lines.join("\n") + "\n", page(Some(-50), None)), with("Read file lf.txt, lines 2951-3000", 2_951, 3_000));
        let mut window = with("Read file window.txt, lines 2951-2960", 2_951, 2_960);
        window.push("[Output truncated. Continue reading with offset: 2961]".into());
        assert_eq!(lines_of(d, "window.txt", lines.join("\n") + "\n", page(Some(-50), Some(10))), window);
        assert_eq!(lines_of(d, "crlf.txt", lines.join("\r\n"), page(Some(-3), None)), with("Read file crlf.txt, lines 2998-3000", 2_998, 3_000));
        // 3000 lines of 101 bytes exceed the 50 KB cap, so reading from line 1 stops after 506 like any read.
        let whole = lines_of(d, "whole.txt", lines.join("\n") + "\n", page(Some(-5_000), None));
        assert_eq!(
            [&whole[0], &whole[whole.len() - 2], &whole[whole.len() - 1]],
            ["Read file whole.txt, lines 1-506", &format!("506: {}", lines[505]), "[Output truncated. Continue reading with offset: 507]"]
        );
        let wide = padded(200, 3_000, 'w');
        let clipped = lines_of(d, "wide.txt", wide.join("\n") + "\n", page(Some(-100), None));
        assert_eq!(
            [&clipped[0], clipped.last().unwrap()],
            ["Read file wide.txt, lines 101-125", "[Output truncated. Continue reading with offset: 126]"]
        );
        let huge = lines_of(d, "huge.txt", format!("{}\n{}", lines[..10].join("\n"), "z".repeat(300 * 1024)), page(Some(-20), None));
        let mut want = with("Read file huge.txt, lines 1-11", 1, 10);
        want.push(format!("11: {}... (line truncated to 2000 chars)", "z".repeat(2_000)));
        assert_eq!(huge, want);
    }

    /// "rejects a negative offset for a directory".
    #[test]
    fn negative_offset_for_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
        let error = read(dir.path(), "src", page(Some(-1), None)).unwrap_err();
        assert_eq!(error.message(), "A negative offset only works for files, and src is a directory");
        assert_eq!(map_read_error(error, "src").model_message(), "A negative offset only works for files, and src is a directory");
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

    /// "clamps long lines without splitting a surrogate pair" (#62575).
    #[test]
    fn clamp_never_cuts_an_emoji_in_half() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("long.txt");
        std::fs::write(&file, format!("{}😀tail", "a".repeat(1_999))).unwrap();
        let out = read(&file, "long.txt", page(None, Some(1))).unwrap();
        let ReadOutput::TextPage { content, truncated, .. } = out else { panic!() };
        assert_eq!(content, format!("{}... (line truncated to 2000 chars)", "a".repeat(1_999)));
        assert!(!truncated);
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
        // Directories end in the platform separator (`path.sep`).
        let sep = std::path::MAIN_SEPARATOR;
        assert_eq!(text(&out, "x", None), format!("Read directory x, entries 1-4\nadir{sep}\nzdir{sep}\nA.txt\nb.txt"));
        let out = read(dir.path(), "x", page(Some(2), Some(2))).unwrap();
        assert_eq!(text(&out, "x", Some(2)), format!("Read directory x, entries 2-3\nzdir{sep}\nA.txt\n[Output truncated. Continue reading with offset: 4]"));
        let out = read(dir.path(), "x", page(Some(9), None)).unwrap();
        assert_eq!(text(&out, "x", Some(9)), "Read directory x, 0 entries");
    }

    #[test]
    fn missing_suggestions() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.json"), "").unwrap();
        std::fs::write(dir.path().join("other.md"), "").unwrap();
        let err = missing_with("src/config", &dir.path().join("config"), vec![]);
        assert_eq!(err.message, "File not found: src/config\n\nDid you mean one of these?\nsrc/config.json");
        let err = missing_with("zzz", &dir.path().join("zzz"), vec![]);
        assert_eq!(err.message, "File not found: zzz");
        std::fs::write(dir.path().join("it’s.txt"), "x").unwrap();
        assert_eq!(alternate_path(&dir.path().join("it's.txt")).unwrap(), dir.path().join("it’s.txt"));
    }

    /// tool-read.test.ts "ranks same-folder suggestions and shows at most three" (#62147).
    #[test]
    fn ranks_suggestions() {
        let mut names: Vec<String> = ["confix.ts", "config.test.ts", "config.json", "Config.ts"].map(String::from).to_vec();
        assert_eq!(similar(&names, "config.ts"), ["Config.ts", "config.json", "config.test.ts"]);
        names.retain(|n| n != "Config.ts");
        assert_eq!(similar(&names, "config.ts"), ["config.json", "config.test.ts", "confix.ts"]);
        // "suggests close names from the same folder"
        assert_eq!(similar(&["config.ts".into(), "db.ts".into()], "conifg.ts"), ["config.ts"]);
        assert_eq!(similar(&["config.yaml".into()], "config.yml"), ["config.yaml"]);
        assert!(similar(&["db.ts", "io.ts", "api.ts", "build.ts"].map(String::from), "ui.ts").is_empty());
        // The old ordering test (#60688): the same stem first, then containment ties by locale order.
        let names = ["copy___missing_read_target__.txt", "__missing_read_target__.txt.bak", "old___missing_read_target__.txt"].map(String::from);
        assert_eq!(
            similar(&names, "__missing_read_target__.txt"),
            ["__missing_read_target__.txt.bak", "old___missing_read_target__.txt", "copy___missing_read_target__.txt"]
        );
        // Look-alike names are what the alternate path recovers or skips, never a suggestion.
        assert!(similar(&["it’s.txt".into()], "it's.txt").is_empty());
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        assert_eq!((extname("..x"), extname(".bashrc"), extname(".."), extname("a."), extname("...")), (".x", "", "", ".", "."));
        assert_eq!((stem("a.b.c"), stem(".bashrc")), ("a.b", ".bashrc"));
    }

    /// tool-read.test.ts "names the first missing folder and its nearest existing parent" (#60688) and "suggests
    /// close folder names from the nearest existing folder".
    #[test]
    fn missing_folder_names_the_nearest_existing_parent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("__missing_read_folder__-old")).unwrap();
        std::fs::write(root.join("unrelated"), "").unwrap();
        let requested = "__missing_read_folder__/src/file.ts";
        let err = missing_with(requested, &root.join(requested), vec![]);
        assert_eq!(
            err.message,
            [
                format!("File not found: {requested}"),
                format!(
                    "The folder {} does not exist. The nearest folder that does is {}.",
                    root.join("__missing_read_folder__").display(),
                    root.display()
                ),
                format!("Did you mean one of these?\n{}", root.join("__missing_read_folder__-old").display()),
            ]
            .join("\n\n")
        );
        // Without a similar entry the suggestion block is left out.
        let err = missing_with("gone/file.ts", &root.join("gone/file.ts"), vec![]);
        assert_eq!(
            err.message,
            format!("File not found: gone/file.ts\n\nThe folder {} does not exist. The nearest folder that does is {}.", root.join("gone").display(), root.display())
        );
        // A parent that is a file cannot be listed, so the walk stops with the plain message.
        let err = missing_with("unrelated/sub/file.ts", &root.join("unrelated/sub/file.ts"), vec![]);
        assert_eq!(err.message, "File not found: unrelated/sub/file.ts");
        std::fs::create_dir_all(root.join("lib")).unwrap();
        let err = missing_with("libs/x.ts", &root.join("libs/x.ts"), vec![]);
        assert_eq!(
            err.message,
            format!(
                "File not found: libs/x.ts\n\nThe folder {} does not exist. The nearest folder that does is {}.\n\nDid you mean one of these?\n{}",
                root.join("libs").display(),
                root.display(),
                root.join("lib").display()
            )
        );
    }

    #[test]
    fn skill_uris() {
        let parsed = parse_skill_uri("skill://guide/references/style.md/").unwrap();
        assert_eq!(parsed, SkillUri { id: "guide".into(), target: "/references/style.md".into(), uri: "skill://guide/references/style.md".into() });
        assert_eq!(parse_skill_uri("skill://guide/").unwrap(), SkillUri { id: "guide".into(), target: "/".into(), uri: "skill://guide".into() });
        for bad in ["skill://", "skill://guide/../SKILL.md", "skill://./x", "skill://a//b", "skill://a/b\\c"] {
            assert_eq!(parse_skill_uri(bad), None, "{bad}");
        }
        let files = VirtualFiles::new("# Guide\nRead references/style.md", &[("references/style.md".into(), "Line 1\nLine 2".into())]);
        let out = files.read("/references/style.md", "skill://guide/references/style.md", page(Some(2), Some(1))).unwrap();
        assert_eq!(text(&out, "skill://guide/references/style.md", Some(2)), "Read file skill://guide/references/style.md, lines 2-2\n2: Line 2");
        let out = files.read("/", "skill://guide/", Page::default()).unwrap();
        assert_eq!(text(&out, "skill://guide/", None), "Read directory skill://guide/, entries 1-2\nreferences/\nSKILL.md");
        let out = files.read("/SKILL.md", "skill://guide/SKILL.md", Page::default()).unwrap();
        assert_eq!(out.to_json()["uri"], "skill://guide/SKILL.md");
        assert_eq!(out.to_json()["name"], "SKILL.md");
        assert_eq!(files.read("/missing.md", "x", Page::default()), Err(ReadError::NotFound));
        assert_eq!(files.read("/SKILL.md/x", "x", Page::default()), Err(ReadError::NotFound));
        assert_eq!(files.read("/references", "r", page(Some(-1), None)), Err(ReadError::DirectoryOffset { resource: "r".into() }));
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
