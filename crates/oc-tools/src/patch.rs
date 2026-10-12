//! Patch grammar, parser and application (`util/src/patch.ts`).

use crate::util::{bom, js};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    Boundary { first: bool },
    InvalidHunk { line: String, line_number: usize, reason: Option<String> },
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::Boundary { first: true } => f.write_str("The first line of the patch must be '*** Begin Patch'"),
            ParseError::Boundary { first: false } => f.write_str("The last line of the patch must be '*** End Patch'"),
            ParseError::InvalidHunk { line_number, reason: Some(reason), .. } => {
                write!(f, "Invalid hunk at line {line_number}: {reason}")
            }
            ParseError::InvalidHunk { line, line_number, reason: None } => write!(
                f,
                "Invalid hunk at line {line_number}: '{line}' is not a valid hunk header. Valid hunk headers: '*** Add File: {{path}}', '*** Delete File: {{path}}', '*** Update File: {{path}}'"
            ),
        }
    }
}

impl std::error::Error for ParseError {}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UpdateFileChunk {
    pub old_lines: Vec<String>,
    pub new_lines: Vec<String>,
    pub change_context: Option<String>,
    pub end_of_file: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hunk {
    Add { path: String, contents: String },
    Delete { path: String },
    Update { path: String, move_path: Option<String>, chunks: Vec<UpdateFileChunk> },
}

impl Hunk {
    pub fn path(&self) -> &str {
        match self {
            Hunk::Add { path, .. } | Hunk::Delete { path } | Hunk::Update { path, .. } => path,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileUpdate {
    pub content: String,
    pub bom: bool,
}

fn strip_heredoc(input: &str) -> String {
    // /^(?:cat\s+)?<<(['"]?)(\w+)\1\s*\n([\s\S]*?)\n\2\s*$/
    let re = regex::Regex::new(r#"^(?:cat\s+)?<<(['"]?)(\w+)(['"]?)\s*\n"#).unwrap();
    let Some(caps) = re.captures(input) else { return input.to_string() };
    let open = caps.get(1).map(|m| m.as_str()).unwrap_or("");
    let close = caps.get(3).map(|m| m.as_str()).unwrap_or("");
    if open != close {
        return input.to_string();
    }
    let tag = caps.get(2).unwrap().as_str();
    let body_start = caps.get(0).unwrap().end();
    let rest = &input[body_start..];
    // Lazy body: the first "\n<tag><ws>*$" that ends the input.
    let mut search = 0;
    while let Some(pos) = rest[search..].find(&format!("\n{tag}")) {
        let at = search + pos;
        let after = &rest[at + 1 + tag.len()..];
        if after.chars().all(|c| js::is_space(c)) {
            return rest[..at].to_string();
        }
        search = at + 1;
    }
    // Body may be empty: "<<EOF\nEOF"
    if let Some(after) = rest.strip_prefix(tag) {
        if after.chars().all(js::is_space) {
            // `\n([\s\S]*?)\n\2` requires a newline before the tag; an empty body needs "\n\nEOF".
            return input.to_string();
        }
    }
    input.to_string()
}

fn is_boundary(line: &str) -> bool {
    line == "*** End Patch"
        || line.starts_with("*** Add File: ")
        || line.starts_with("*** Delete File: ")
        || line.starts_with("*** Update File: ")
}

/// `Patch.parse`.
pub fn parse(patch_text: &str) -> Result<Vec<Hunk>, ParseError> {
    let stripped = strip_heredoc(js::trim(patch_text));
    let lines: Vec<String> =
        stripped.split('\n').map(|line| line.strip_suffix('\r').unwrap_or(line).to_string()).collect();
    let begin = if lines.first().map(|l| js::trim(l)) == Some("*** Begin Patch") { 0isize } else { -1 };
    let end = if lines.last().map(|l| js::trim(l)) == Some("*** End Patch") { lines.len() as isize - 1 } else { -1 };
    if begin == -1 {
        return Err(ParseError::Boundary { first: true });
    }
    if end == -1 || begin >= end {
        return Err(ParseError::Boundary { first: false });
    }
    let begin = begin as usize;
    let end = end as usize;

    let mut hunks = Vec::new();
    let mut index = begin + 1;
    while index < end {
        let line = &lines[index];
        let header = js::trim(line);
        if index == begin + 1
            && header.starts_with("*** Environment ID:")
            && !js::trim(&header["*** Environment ID:".len()..]).is_empty()
        {
            index += 1;
            continue;
        }
        if let Some(rest) = header.strip_prefix("*** Add File: ") {
            let path = js::trim(rest).to_string();
            let (content, next) = parse_add(&lines, index + 1, end, &path)?;
            hunks.push(Hunk::Add { path, contents: content });
            index = next;
            continue;
        }
        if let Some(rest) = header.strip_prefix("*** Delete File: ") {
            let path = js::trim(rest).to_string();
            let next = lines.get(index + 1).map(|l| js::trim(l).to_string());
            if index + 1 < end {
                if let Some(next) = next {
                    if !is_boundary(&next) {
                        if next.starts_with("*** ") {
                            return Err(ParseError::InvalidHunk { line: next, line_number: index + 2, reason: None });
                        }
                        let reason = format!(
                            "Unexpected line after Delete File '{path}': '{next}'. Delete hunks do not contain body lines"
                        );
                        return Err(ParseError::InvalidHunk { line: next, line_number: index + 2, reason: Some(reason) });
                    }
                }
            }
            hunks.push(Hunk::Delete { path });
            index += 1;
            continue;
        }
        if let Some(rest) = header.strip_prefix("*** Update File: ") {
            let path = js::trim(rest).to_string();
            let mut next = index + 1;
            let mut move_path = None;
            while lines.get(next).map(|l| js::trim_end(l)) == Some("*** End of File") {
                next += 1;
            }
            if let Some(mv) = lines.get(next).map(|l| js::trim_end(l)) {
                if mv == "*** Move to:" || mv.starts_with("*** Move to: ") {
                    let dest = js::trim(mv.get("*** Move to: ".len()..).unwrap_or("")).to_string();
                    if dest.is_empty() {
                        return Err(ParseError::InvalidHunk {
                            line: js::trim(&lines[next]).to_string(),
                            line_number: next + 1,
                            reason: Some(format!("Move destination for '{path}' must not be empty")),
                        });
                    }
                    move_path = Some(dest);
                    next += 1;
                }
            }
            let (chunks, next) = parse_update(&lines, next, end, &path, index)?;
            hunks.push(Hunk::Update { path, move_path, chunks });
            index = next;
            continue;
        }
        return Err(ParseError::InvalidHunk { line: header.to_string(), line_number: index + 1, reason: None });
    }
    Ok(hunks)
}

fn parse_add(lines: &[String], start: usize, end: usize, path: &str) -> Result<(String, usize), ParseError> {
    let mut content = Vec::new();
    let mut index = start;
    while index < end && !is_boundary(js::trim(&lines[index])) {
        let Some(rest) = lines[index].strip_prefix('+') else {
            let line = js::trim(&lines[index]).to_string();
            let reason = format!("Invalid Add File line for '{path}': expected a line starting with '+', got '{line}'");
            return Err(ParseError::InvalidHunk { line, line_number: index + 1, reason: Some(reason) });
        };
        content.push(rest.to_string());
        index += 1;
    }
    Ok((content.join("\n"), index))
}

fn unexpected(line: &str) -> String {
    format!(
        "Unexpected line found in update hunk: '{line}'. Every line should start with ' ' (context line), '+' (added line), or '-' (removed line)"
    )
}

fn parse_update(
    lines: &[String],
    start: usize,
    end: usize,
    path: &str,
    hunk: usize,
) -> Result<(Vec<UpdateFileChunk>, usize), ParseError> {
    let mut chunks: Vec<UpdateFileChunk> = Vec::new();
    let mut index = start;
    let mut after_end_of_file = false;
    while index < end {
        let line = &lines[index];
        let update_line = js::trim_end(line);
        if after_end_of_file {
            if update_line.is_empty() {
                index += 1;
                continue;
            }
            if update_line == "@@" || update_line.starts_with("@@ ") {
                after_end_of_file = false;
            } else if is_boundary(update_line) {
                break;
            } else {
                return Err(ParseError::InvalidHunk {
                    line: line.clone(),
                    line_number: index + 1,
                    reason: Some(format!("Expected update hunk to start with a @@ context marker, got: '{line}'")),
                });
            }
        }
        if update_line == "*** End of File" {
            if let Some(chunk) = chunks.last_mut() {
                if chunk.old_lines.is_empty() && chunk.new_lines.is_empty() {
                    return Err(ParseError::InvalidHunk {
                        line: update_line.to_string(),
                        line_number: index + 1,
                        reason: Some("Update hunk does not contain any lines".into()),
                    });
                }
                chunk.end_of_file = true;
                after_end_of_file = true;
            }
            index += 1;
            continue;
        }
        if is_boundary(update_line) {
            break;
        }
        if update_line == "@@" || update_line.starts_with("@@ ") {
            if let Some(previous) = chunks.last() {
                if previous.old_lines.is_empty() && previous.new_lines.is_empty() {
                    return Err(ParseError::InvalidHunk {
                        line: line.clone(),
                        line_number: index + 1,
                        reason: Some(unexpected(line)),
                    });
                }
            }
            chunks.push(UpdateFileChunk {
                change_context: if update_line == "@@" { None } else { Some(update_line["@@ ".len()..].to_string()) },
                ..Default::default()
            });
            index += 1;
            continue;
        }
        if chunks.is_empty() {
            chunks.push(UpdateFileChunk::default());
        }
        let chunk = chunks.last_mut().unwrap();
        if line.is_empty() {
            chunk.old_lines.push(String::new());
            chunk.new_lines.push(String::new());
            index += 1;
            continue;
        }
        if let Some(rest) = line.strip_prefix(' ') {
            chunk.old_lines.push(rest.to_string());
            chunk.new_lines.push(rest.to_string());
            index += 1;
            continue;
        }
        if let Some(rest) = line.strip_prefix('-') {
            chunk.old_lines.push(rest.to_string());
            index += 1;
            continue;
        }
        if let Some(rest) = line.strip_prefix('+') {
            chunk.new_lines.push(rest.to_string());
            index += 1;
            continue;
        }
        let populated = !chunk.old_lines.is_empty() || !chunk.new_lines.is_empty();
        return Err(ParseError::InvalidHunk {
            line: line.clone(),
            line_number: index + 1,
            reason: Some(if populated {
                format!("Expected update hunk to start with a @@ context marker, got: '{line}'")
            } else {
                unexpected(line)
            }),
        });
    }
    if chunks.is_empty() {
        return Err(ParseError::InvalidHunk {
            line: js::trim(&lines[hunk]).to_string(),
            line_number: hunk + 1,
            reason: Some(format!("Update file hunk for path '{path}' is empty")),
        });
    }
    let last = chunks.last().unwrap();
    if last.old_lines.is_empty() && last.new_lines.is_empty() {
        let line = js::trim(lines.get(index).map(String::as_str).unwrap_or("")).to_string();
        let reason = if line == "*** End Patch" { "Update hunk does not contain any lines".to_string() } else { unexpected(&line) };
        return Err(ParseError::InvalidHunk { line, line_number: index + 1, reason: Some(reason) });
    }
    Ok((chunks, index))
}

/// `Patch.derive(path, chunks, original)`.
pub fn derive(path: &str, chunks: &[UpdateFileChunk], original: &str) -> Result<FileUpdate, String> {
    let source = bom::split(original);
    let mut lines: Vec<String> = source.text.split('\n').map(String::from).collect();
    if lines.last().map(String::as_str) == Some("") {
        lines.pop();
    }
    let replacements = compute_replacements(&lines, path, chunks)?;
    for (start, remove, insert) in replacements.into_iter().rev() {
        let end = (start + remove).min(lines.len());
        let start = start.min(lines.len());
        lines.splice(start..end, insert);
    }
    if lines.last().map(String::as_str) != Some("") {
        lines.push(String::new());
    }
    let next = bom::split(&lines.join("\n"));
    Ok(FileUpdate { content: next.text, bom: source.bom || next.bom })
}

pub fn join_bom(text: &str, has_bom: bool) -> String {
    bom::join(text, has_bom)
}

type Replacement = (usize, usize, Vec<String>);

fn compute_replacements(lines: &[String], path: &str, chunks: &[UpdateFileChunk]) -> Result<Vec<Replacement>, String> {
    let mut replacements: Vec<Replacement> = Vec::new();
    let mut line_index = 0usize;
    for chunk in chunks {
        if let Some(context) = chunk.change_context.as_deref().filter(|c| !c.is_empty()) {
            let found = seek(lines, &[context.to_string()], line_index, false);
            let Some(found) = found else {
                return Err(format!("Failed to find context '{context}' in {path}"));
            };
            line_index = found + 1;
        }
        if chunk.old_lines.is_empty() {
            replacements.push((lines.len(), 0, chunk.new_lines.clone()));
            continue;
        }
        let mut old_lines: &[String] = &chunk.old_lines;
        let mut new_lines: &[String] = &chunk.new_lines;
        let mut found = seek(lines, old_lines, line_index, chunk.end_of_file);
        if found.is_none() && old_lines.last().map(String::as_str) == Some("") {
            old_lines = &old_lines[..old_lines.len() - 1];
            if new_lines.last().map(String::as_str) == Some("") {
                new_lines = &new_lines[..new_lines.len() - 1];
            }
            found = seek(lines, old_lines, line_index, chunk.end_of_file);
        }
        if found.is_none() && chunk.old_lines.iter().all(String::is_empty) {
            let expected = if chunk.old_lines.len() == 1 {
                "an expected blank line".to_string()
            } else {
                format!("{} consecutive blank lines", chunk.old_lines.len())
            };
            return Err(format!("Failed to find {expected} in {path}"));
        }
        let Some(found) = found else {
            return Err(format!("Failed to find expected lines in {path}:\n{}", chunk.old_lines.join("\n")));
        };
        replacements.push((found, old_lines.len(), new_lines.to_vec()));
        line_index = found + old_lines.len();
    }
    replacements.sort_by_key(|r| r.0);
    Ok(replacements)
}

/// The unicode punctuation map shared with `edit`.
pub fn normalize_unicode(value: &str) -> String {
    value
        .chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}' | '\u{2212}' => '-',
            '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
            other => other,
        })
        .collect()
}

fn compare(kind: u8, left: &str, right: &str) -> bool {
    match kind {
        0 => left == right,
        1 => js::trim_end(left) == js::trim_end(right),
        2 => js::trim(left) == js::trim(right),
        _ => normalize_unicode(js::trim(left)) == normalize_unicode(js::trim(right)),
    }
}

fn matches_at(lines: &[String], pattern: &[String], offset: usize, kind: u8) -> bool {
    pattern.iter().enumerate().all(|(i, p)| lines.get(offset + i).is_some_and(|l| compare(kind, l, p)))
}

fn seek(lines: &[String], pattern: &[String], start: usize, eof: bool) -> Option<usize> {
    if pattern.is_empty() {
        return None;
    }
    if pattern.len() > lines.len() {
        return None;
    }
    let last = lines.len() - pattern.len();
    if eof {
        if last < start {
            return None;
        }
        for kind in 0..4 {
            if matches_at(lines, pattern, last, kind) {
                return Some(last);
            }
        }
        return None;
    }
    for kind in 0..4 {
        let mut offset = start;
        while offset <= last {
            if matches_at(lines, pattern, offset, kind) {
                return Some(offset);
            }
            offset += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn parses_all_hunk_kinds() {
        let text = "*** Begin Patch\n*** Add File: hello.txt\n+Hello world\n*** Update File: src/app.py\n*** Move to: src/main.py\n@@ def greet():\n-print(\"Hi\")\n+print(\"Hello, world!\")\n*** Delete File: obsolete.txt\n*** End Patch";
        let hunks = parse(text).unwrap();
        assert_eq!(
            hunks,
            vec![
                Hunk::Add { path: "hello.txt".into(), contents: "Hello world".into() },
                Hunk::Update {
                    path: "src/app.py".into(),
                    move_path: Some("src/main.py".into()),
                    chunks: vec![UpdateFileChunk {
                        old_lines: s(&["print(\"Hi\")"]),
                        new_lines: s(&["print(\"Hello, world!\")"]),
                        change_context: Some("def greet():".into()),
                        end_of_file: false,
                    }],
                },
                Hunk::Delete { path: "obsolete.txt".into() },
            ]
        );
    }

    #[test]
    fn boundaries() {
        assert_eq!(parse("bad").unwrap_err().to_string(), "The first line of the patch must be '*** Begin Patch'");
        assert_eq!(parse("*** Begin Patch\nx").unwrap_err().to_string(), "The last line of the patch must be '*** End Patch'");
        assert_eq!(parse("*** Begin Patch").unwrap_err().to_string(), "The last line of the patch must be '*** End Patch'");
        assert_eq!(parse("  *** Begin Patch\r\n*** End Patch  ").unwrap(), vec![]);
    }

    #[test]
    fn heredoc_and_environment() {
        let text = "cat <<'EOF'\n*** Begin Patch\n*** Environment ID: abc\n*** Delete File: a\n*** End Patch\nEOF";
        assert_eq!(parse(text).unwrap(), vec![Hunk::Delete { path: "a".into() }]);
        let text = "<<EOF\n*** Begin Patch\n*** Delete File: a\n*** End Patch\nEOF\n";
        assert_eq!(parse(text).unwrap(), vec![Hunk::Delete { path: "a".into() }]);
    }

    #[test]
    fn errors() {
        let err = parse("*** Begin Patch\n*** Frobnicate: x\n*** End Patch").unwrap_err();
        assert_eq!(err.to_string(), "Invalid hunk at line 2: '*** Frobnicate: x' is not a valid hunk header. Valid hunk headers: '*** Add File: {path}', '*** Delete File: {path}', '*** Update File: {path}'");
        let err = parse("*** Begin Patch\n*** Add File: a\nnope\n*** End Patch").unwrap_err();
        assert_eq!(err.to_string(), "Invalid hunk at line 3: Invalid Add File line for 'a': expected a line starting with '+', got 'nope'");
        let err = parse("*** Begin Patch\n*** Delete File: a\nbody\n*** End Patch").unwrap_err();
        assert_eq!(err.to_string(), "Invalid hunk at line 3: Unexpected line after Delete File 'a': 'body'. Delete hunks do not contain body lines");
        let err = parse("*** Begin Patch\n*** Delete File: a\n*** Bogus\n*** End Patch").unwrap_err();
        assert!(err.to_string().starts_with("Invalid hunk at line 3: '*** Bogus' is not a valid hunk header"));
        let err = parse("*** Begin Patch\n*** Update File: a\n*** End Patch").unwrap_err();
        assert_eq!(err.to_string(), "Invalid hunk at line 2: Update file hunk for path 'a' is empty");
        let err = parse("*** Begin Patch\n*** Update File: a\n@@\n*** End Patch").unwrap_err();
        assert_eq!(err.to_string(), "Invalid hunk at line 4: Update hunk does not contain any lines");
        let err = parse("*** Begin Patch\n*** Update File: a\n@@\n@@\n-x\n*** End Patch").unwrap_err();
        assert!(err.to_string().starts_with("Invalid hunk at line 4: Unexpected line found in update hunk: '@@'."));
        let err = parse("*** Begin Patch\n*** Update File: a\n-x\nbad\n*** End Patch").unwrap_err();
        assert_eq!(err.to_string(), "Invalid hunk at line 4: Expected update hunk to start with a @@ context marker, got: 'bad'");
        let err = parse("*** Begin Patch\n*** Update File: a\nbad\n*** End Patch").unwrap_err();
        assert!(err.to_string().starts_with("Invalid hunk at line 3: Unexpected line found in update hunk: 'bad'"));
        let err = parse("*** Begin Patch\n*** Update File: a\n*** Move to: \n-x\n*** End Patch").unwrap_err();
        assert_eq!(err.to_string(), "Invalid hunk at line 3: Move destination for 'a' must not be empty");
        let err = parse("*** Begin Patch\n*** Update File: a\n-x\n*** End of File\nzzz\n*** End Patch").unwrap_err();
        assert_eq!(err.to_string(), "Invalid hunk at line 5: Expected update hunk to start with a @@ context marker, got: 'zzz'");
        let err = parse("*** Begin Patch\n*** Update File: a\n@@\n*** End of File\n*** End Patch").unwrap_err();
        assert_eq!(err.to_string(), "Invalid hunk at line 4: Update hunk does not contain any lines");
    }

    #[test]
    fn end_of_file_and_blank_lines() {
        let hunks = parse("*** Begin Patch\n*** Update File: a\n@@\n-x\n+y\n*** End of File\n\n@@ ctx\n+z\n*** End Patch").unwrap();
        let Hunk::Update { chunks, .. } = &hunks[0] else { panic!() };
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].end_of_file);
        assert_eq!(chunks[1].change_context.as_deref(), Some("ctx"));
        let hunks = parse("*** Begin Patch\n*** Update File: a\n a\n\n-b\n*** End Patch").unwrap();
        let Hunk::Update { chunks, .. } = &hunks[0] else { panic!() };
        assert_eq!(chunks[0].old_lines, s(&["a", "", "b"]));
        assert_eq!(chunks[0].new_lines, s(&["a", ""]));
    }

    #[test]
    fn derive_applies_chunks() {
        let chunk = UpdateFileChunk { old_lines: s(&["b"]), new_lines: s(&["B"]), change_context: None, end_of_file: false };
        assert_eq!(derive("f", &[chunk], "a\nb\nc\n").unwrap(), FileUpdate { content: "a\nB\nc\n".into(), bom: false });
        // context seek
        let chunk = UpdateFileChunk { old_lines: s(&["x"]), new_lines: s(&["y"]), change_context: Some("fn two".into()), end_of_file: false };
        assert_eq!(derive("f", &[chunk], "fn one\nx\nfn two\nx\n").unwrap().content, "fn one\nx\nfn two\ny\n");
        // pure insertion at end
        let chunk = UpdateFileChunk { old_lines: vec![], new_lines: s(&["z"]), change_context: None, end_of_file: false };
        assert_eq!(derive("f", &[chunk], "a").unwrap().content, "a\nz\n");
        // whitespace-tolerant matching (rstrip, trim, unicode)
        let chunk = UpdateFileChunk { old_lines: s(&["say “hi” — now"]), new_lines: s(&["ok"]), change_context: None, end_of_file: false };
        assert_eq!(derive("f", &[chunk], "  say \"hi\" - now  \n").unwrap().content, "ok\n");
        // eof anchoring
        let chunk = UpdateFileChunk { old_lines: s(&["x"]), new_lines: s(&["y"]), change_context: None, end_of_file: true };
        assert_eq!(derive("f", &[chunk], "x\nx\n").unwrap().content, "x\ny\n");
        // trailing blank retry
        let chunk = UpdateFileChunk { old_lines: s(&["a", ""]), new_lines: s(&["b", ""]), change_context: None, end_of_file: false };
        assert_eq!(derive("f", &[chunk], "a").unwrap().content, "b\n");
        // bom preserved
        let chunk = UpdateFileChunk { old_lines: s(&["a"]), new_lines: s(&["b"]), change_context: None, end_of_file: false };
        assert_eq!(derive("f", &[chunk], "\u{FEFF}a\n").unwrap(), FileUpdate { content: "b\n".into(), bom: true });
        // CRLF keeps \r on untouched lines
        let chunk = UpdateFileChunk { old_lines: s(&["b"]), new_lines: s(&["B"]), change_context: None, end_of_file: false };
        assert_eq!(derive("f", &[chunk], "a\r\nb\r\nc\r\n").unwrap().content, "a\r\nB\nc\r\n");
    }

    #[test]
    fn derive_errors() {
        let chunk = UpdateFileChunk { old_lines: s(&["q"]), new_lines: s(&["r"]), change_context: Some("nope".into()), end_of_file: false };
        assert_eq!(derive("f.txt", &[chunk], "a\n").unwrap_err(), "Failed to find context 'nope' in f.txt");
        let chunk = UpdateFileChunk { old_lines: s(&["q", "w"]), new_lines: s(&["r"]), change_context: None, end_of_file: false };
        assert_eq!(derive("f.txt", &[chunk], "a\n").unwrap_err(), "Failed to find expected lines in f.txt:\nq\nw");
        let chunk = UpdateFileChunk { old_lines: s(&[""]), new_lines: s(&["r"]), change_context: None, end_of_file: false };
        assert_eq!(derive("f.txt", &[chunk], "a\n").unwrap_err(), "Failed to find an expected blank line in f.txt");
        let chunk = UpdateFileChunk { old_lines: s(&["", "", ""]), new_lines: s(&["r"]), change_context: None, end_of_file: false };
        assert_eq!(derive("f.txt", &[chunk], "a\n").unwrap_err(), "Failed to find 3 consecutive blank lines in f.txt");
    }
}
