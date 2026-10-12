//! Byte-exact port of the parts of jsdiff 8.0.4 used by upstream `file-diff.ts`:
//! `createTwoFilesPatch` (default headers, context 4) and `diffLines` change counts.
//!
//! Ported from `diff/libesm/diff/base.js` (Myers diff with jsdiff's diagonal pruning and
//! tie-breaking), `diff/libesm/diff/line.js` (tokenizer, default options) and
//! `diff/libesm/patch/create.js` (`structuredPatch` + `formatPatch`).

/// One change object as produced by `Diff.buildValues` (values are kept as token slices).
#[derive(Debug, Clone)]
struct Change<'a> {
    added: bool,
    removed: bool,
    lines: Vec<&'a str>,
}

/// Linked-list component stored in an arena (`previousComponent` = `prev`).
#[derive(Clone, Copy)]
struct Component {
    count: usize,
    added: bool,
    removed: bool,
    prev: Option<usize>,
}

#[derive(Clone, Copy)]
struct Path {
    old_pos: i64,
    last: Option<usize>,
}

/// `tokenize` from line.js with default options: split on `\n` / `\r\n`, keeping the
/// separator attached to the preceding line; a trailing empty piece is dropped.
fn tokenize(value: &str) -> Vec<&str> {
    // Equivalent of `value.split(/(\n|\r\n)/)` followed by the merge loop.
    let bytes = value.as_bytes();
    let mut tokens: Vec<&str> = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            tokens.push(&value[start..i + 1]);
            start = i + 1;
            i += 1;
        } else if bytes[i] == b'\r' && i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
            tokens.push(&value[start..i + 2]);
            start = i + 2;
            i += 2;
        } else {
            i += 1;
        }
    }
    if start < value.len() {
        tokens.push(&value[start..]);
    }
    // `removeEmpty`: no empty tokens can be produced by the merge above.
    tokens
}

/// `Diff.diff` for LineDiff with default options (no maxEditLength, no timeout).
fn diff_tokens<'a>(old: &[&'a str], new: &[&'a str]) -> Vec<Change<'a>> {
    let new_len = new.len() as i64;
    let old_len = old.len() as i64;
    let mut arena: Vec<Component> = Vec::new();
    let max_edit_length = new_len + old_len;

    let extract_common = |arena: &mut Vec<Component>, base: &mut Path, diagonal: i64| -> i64 {
        let mut old_pos = base.old_pos;
        let mut new_pos = old_pos - diagonal;
        let mut common = 0usize;
        while new_pos + 1 < new_len && old_pos + 1 < old_len && old[(old_pos + 1) as usize] == new[(new_pos + 1) as usize] {
            new_pos += 1;
            old_pos += 1;
            common += 1;
        }
        if common > 0 {
            arena.push(Component { count: common, added: false, removed: false, prev: base.last });
            base.last = Some(arena.len() - 1);
        }
        base.old_pos = old_pos;
        new_pos
    };

    let add_to_path = |arena: &mut Vec<Component>, path: &Path, added: bool, removed: bool, inc: i64| -> Path {
        if let Some(last) = path.last {
            let l = arena[last];
            if l.added == added && l.removed == removed {
                arena.push(Component { count: l.count + 1, added, removed, prev: l.prev });
                return Path { old_pos: path.old_pos + inc, last: Some(arena.len() - 1) };
            }
        }
        arena.push(Component { count: 1, added, removed, prev: path.last });
        Path { old_pos: path.old_pos + inc, last: Some(arena.len() - 1) }
    };

    // bestPath indexed by diagonal in [-(max+1), max+1].
    let offset = max_edit_length + 1;
    let mut best: Vec<Option<Path>> = vec![None; (2 * offset + 1) as usize];
    let idx = |d: i64| (d + offset) as usize;

    let mut seed = Path { old_pos: -1, last: None };
    let new_pos = extract_common(&mut arena, &mut seed, 0);
    if seed.old_pos + 1 >= old_len && new_pos + 1 >= new_len {
        return build_values(&arena, seed.last, old, new);
    }
    best[idx(0)] = Some(seed);

    let mut min_diag = i64::MIN;
    let mut max_diag = i64::MAX;
    let mut edit_length: i64 = 1;
    while edit_length <= max_edit_length {
        let mut diagonal = min_diag.max(-edit_length);
        let upper = max_diag.min(edit_length);
        while diagonal <= upper {
            let remove_path = best[idx(diagonal - 1)];
            let add_path = best[idx(diagonal + 1)];
            if remove_path.is_some() {
                best[idx(diagonal - 1)] = None;
            }
            let mut can_add = false;
            if let Some(add) = add_path {
                let add_new_pos = add.old_pos - diagonal;
                can_add = 0 <= add_new_pos && add_new_pos < new_len;
            }
            let can_remove = remove_path.is_some_and(|r| r.old_pos + 1 < old_len);
            if !can_add && !can_remove {
                best[idx(diagonal)] = None;
                diagonal += 2;
                continue;
            }
            let mut base = if !can_remove || (can_add && remove_path.unwrap().old_pos < add_path.unwrap().old_pos) {
                add_to_path(&mut arena, &add_path.unwrap(), true, false, 0)
            } else {
                add_to_path(&mut arena, &remove_path.unwrap(), false, true, 1)
            };
            let new_pos = extract_common(&mut arena, &mut base, diagonal);
            if base.old_pos + 1 >= old_len && new_pos + 1 >= new_len {
                return build_values(&arena, base.last, old, new);
            }
            best[idx(diagonal)] = Some(base);
            if base.old_pos + 1 >= old_len {
                max_diag = max_diag.min(diagonal - 1);
            }
            if new_pos + 1 >= new_len {
                min_diag = min_diag.max(diagonal + 1);
            }
            diagonal += 2;
        }
        edit_length += 1;
    }
    // Unreachable without maxEditLength/timeout: the loop always reaches the end.
    unreachable!("jsdiff: edit graph not exhausted")
}

fn build_values<'a>(arena: &[Component], last: Option<usize>, old: &[&'a str], new: &[&'a str]) -> Vec<Change<'a>> {
    let mut components = Vec::new();
    let mut cursor = last;
    while let Some(i) = cursor {
        components.push(arena[i]);
        cursor = arena[i].prev;
    }
    components.reverse();
    let mut new_pos = 0usize;
    let mut old_pos = 0usize;
    let mut out = Vec::with_capacity(components.len());
    for c in components {
        if !c.removed {
            let lines = new[new_pos..new_pos + c.count].to_vec();
            new_pos += c.count;
            if !c.added {
                old_pos += c.count;
            }
            out.push(Change { added: c.added, removed: false, lines });
        } else {
            let lines = old[old_pos..old_pos + c.count].to_vec();
            old_pos += c.count;
            out.push(Change { added: false, removed: true, lines });
        }
    }
    out
}

fn diff_lines<'a>(old: &'a str, new: &'a str) -> Vec<Change<'a>> {
    let old_tokens = tokenize(old);
    let new_tokens = tokenize(new);
    diff_tokens(&old_tokens, &new_tokens)
}

struct Hunk {
    old_start: i64,
    old_lines: i64,
    new_start: i64,
    new_lines: i64,
    lines: Vec<String>,
}

/// `structuredPatch` (context 4) hunks.
fn structured_hunks(old: &str, new: &str) -> Vec<Hunk> {
    const CONTEXT: usize = 4;
    let mut diff = diff_lines(old, new);
    // Sentinel to simplify cleanup (`{ value: '', lines: [] }`).
    diff.push(Change { added: false, removed: false, lines: Vec::new() });
    let context_lines = |lines: &[&str]| lines.iter().map(|l| format!(" {l}")).collect::<Vec<_>>();

    let mut hunks = Vec::new();
    let mut old_range_start: i64 = 0;
    let mut new_range_start: i64 = 0;
    let mut cur_range: Vec<String> = Vec::new();
    let mut old_line: i64 = 1;
    let mut new_line: i64 = 1;
    let len = diff.len();
    for i in 0..len {
        let current = &diff[i];
        let lines = &current.lines;
        if current.added || current.removed {
            if old_range_start == 0 {
                old_range_start = old_line;
                new_range_start = new_line;
                if i > 0 {
                    let prev = &diff[i - 1].lines;
                    let from = prev.len().saturating_sub(CONTEXT);
                    cur_range = context_lines(&prev[from..]);
                    old_range_start -= cur_range.len() as i64;
                    new_range_start -= cur_range.len() as i64;
                }
            }
            let sign = if current.added { '+' } else { '-' };
            for line in lines {
                cur_range.push(format!("{sign}{line}"));
            }
            if current.added {
                new_line += lines.len() as i64;
            } else {
                old_line += lines.len() as i64;
            }
        } else {
            if old_range_start != 0 {
                if lines.len() <= CONTEXT * 2 && (i as i64) < len as i64 - 2 {
                    cur_range.extend(context_lines(lines));
                } else {
                    let context_size = lines.len().min(CONTEXT);
                    cur_range.extend(context_lines(&lines[..context_size]));
                    hunks.push(Hunk {
                        old_start: old_range_start,
                        old_lines: old_line - old_range_start + context_size as i64,
                        new_start: new_range_start,
                        new_lines: new_line - new_range_start + context_size as i64,
                        lines: std::mem::take(&mut cur_range),
                    });
                    old_range_start = 0;
                    new_range_start = 0;
                }
            }
            old_line += lines.len() as i64;
            new_line += lines.len() as i64;
        }
    }
    for hunk in &mut hunks {
        let mut out = Vec::with_capacity(hunk.lines.len());
        for line in hunk.lines.drain(..) {
            if let Some(stripped) = line.strip_suffix('\n') {
                out.push(stripped.to_string());
            } else {
                out.push(line);
                out.push("\\ No newline at end of file".to_string());
            }
        }
        hunk.lines = out;
    }
    hunks
}

/// `createTwoFilesPatch(oldFile, newFile, old, new)` with no headers and default options.
pub fn create_two_files_patch(old_file: &str, new_file: &str, old: &str, new: &str) -> String {
    let hunks = structured_hunks(old, new);
    let mut ret: Vec<String> = Vec::new();
    if old_file == new_file {
        ret.push(format!("Index: {old_file}"));
    }
    ret.push("===================================================================".to_string());
    ret.push(format!("--- {old_file}"));
    ret.push(format!("+++ {new_file}"));
    for mut hunk in hunks {
        if hunk.old_lines == 0 {
            hunk.old_start -= 1;
        }
        if hunk.new_lines == 0 {
            hunk.new_start -= 1;
        }
        ret.push(format!("@@ -{},{} +{},{} @@", hunk.old_start, hunk.old_lines, hunk.new_start, hunk.new_lines));
        ret.extend(hunk.lines);
    }
    let mut out = ret.join("\n");
    out.push('\n');
    out
}

/// `(additions, deletions)` summed over `diffLines(old, new)` change counts.
pub fn diff_lines_counts(old: &str, new: &str) -> (u64, u64) {
    diff_lines(old, new).iter().fold((0, 0), |(a, d), c| {
        if c.added {
            (a + c.lines.len() as u64, d)
        } else if c.removed {
            (a, d + c.lines.len() as u64)
        } else {
            (a, d)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_file_diff_example() {
        assert_eq!(
            create_two_files_patch("src/a.txt", "src/a.txt", "a\nb\nc\n", "a\nB\nc\n"),
            "Index: src/a.txt\n===================================================================\n--- src/a.txt\n+++ src/a.txt\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n"
        );
        assert_eq!(diff_lines_counts("a\nb\nc\n", "a\nB\nc\n"), (1, 1));
    }

    #[test]
    fn empty_before_is_zero_range() {
        assert!(create_two_files_patch("f", "f", "", "x\n").contains("@@ -0,0 +1,1 @@"));
    }

    #[test]
    fn tokenizer_keeps_separators() {
        assert_eq!(tokenize("a\r\nb\nc"), vec!["a\r\n", "b\n", "c"]);
        assert_eq!(tokenize("\n\n"), vec!["\n", "\n"]);
        assert_eq!(tokenize("x\ry"), vec!["x\ry"]);
        assert!(tokenize("").is_empty());
    }

    #[test]
    fn large_insert_is_fast() {
        let a: String = (0..20_000).map(|i| format!("l{i}\n")).collect();
        let b = format!("{a}tail\n");
        assert_eq!(diff_lines_counts(&a, &b), (1, 0));
    }

    #[test]
    fn golden_against_jsdiff_8_0_4() {
        for (i, (of, nf, old, new, patch, add, del)) in GOLDEN.iter().enumerate() {
            assert_eq!(&create_two_files_patch(of, nf, old, new), patch, "case {i}");
            assert_eq!(diff_lines_counts(old, new), (*add, *del), "counts case {i}");
        }
    }

    // Generated by running `diff@8.0.4` (createTwoFilesPatch / diffLines) under bun.
    const GOLDEN: &[(&str, &str, &str, &str, &str, u64, u64)] = &[
    ("a", "a", "", "", "Index: a\n===================================================================\n--- a\n+++ a\n", 0, 0),
    ("a", "a", "", "x\n", "Index: a\n===================================================================\n--- a\n+++ a\n@@ -0,0 +1,1 @@\n+x\n", 1, 0),
    ("a", "a", "x\n", "", "Index: a\n===================================================================\n--- a\n+++ a\n@@ -1,1 +0,0 @@\n-x\n", 0, 1),
    ("a", "a", "x", "", "Index: a\n===================================================================\n--- a\n+++ a\n@@ -1,1 +0,0 @@\n-x\n\\ No newline at end of file\n", 0, 1),
    ("a", "a", "", "x", "Index: a\n===================================================================\n--- a\n+++ a\n@@ -0,0 +1,1 @@\n+x\n\\ No newline at end of file\n", 1, 0),
    ("a", "a", "x\ny\n", "x\ny\n", "Index: a\n===================================================================\n--- a\n+++ a\n", 0, 0),
    ("a", "b", "x\ny\n", "x\ny\n", "===================================================================\n--- a\n+++ b\n", 0, 0),
    ("a", "a", "x\ny", "x\ny\n", "Index: a\n===================================================================\n--- a\n+++ a\n@@ -1,2 +1,2 @@\n x\n-y\n\\ No newline at end of file\n+y\n", 1, 1),
    ("a", "a", "x\ny\n", "x\ny", "Index: a\n===================================================================\n--- a\n+++ a\n@@ -1,2 +1,2 @@\n x\n-y\n+y\n\\ No newline at end of file\n", 1, 1),
    ("a", "a", "x\ny", "x\nz", "Index: a\n===================================================================\n--- a\n+++ a\n@@ -1,2 +1,2 @@\n x\n-y\n\\ No newline at end of file\n+z\n\\ No newline at end of file\n", 1, 1),
    ("src/a.txt", "src/a.txt", "a\nb\nc\n", "a\nB\nc\n", "Index: src/a.txt\n===================================================================\n--- src/a.txt\n+++ src/a.txt\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n", 1, 1),
    ("f", "f", "a\r\nb\r\nc\r\n", "a\r\nB\r\nc\r\n", "Index: f\n===================================================================\n--- f\n+++ f\n@@ -1,3 +1,3 @@\n a\r\n-b\r\n+B\r\n c\r\n", 1, 1),
    ("f", "f", "a\r\nb\r\nc", "a\nb\nc", "Index: f\n===================================================================\n--- f\n+++ f\n@@ -1,3 +1,3 @@\n-a\r\n-b\r\n+a\n+b\n c\n\\ No newline at end of file\n", 2, 2),
    ("f", "f", "\n\n\n", "\n\n", "Index: f\n===================================================================\n--- f\n+++ f\n@@ -1,3 +1,2 @@\n \n \n-\n", 0, 1),
    ("f", "f", "héllo\nwörld 😀\n", "héllo\nwörld 😃\n", "Index: f\n===================================================================\n--- f\n+++ f\n@@ -1,2 +1,2 @@\n héllo\n-wörld 😀\n+wörld 😃\n", 1, 1),
    ("old", "new", "1\n2\n", "1\n2\n3\n", "===================================================================\n--- old\n+++ new\n@@ -1,2 +1,3 @@\n 1\n 2\n+3\n", 1, 0),
    ("f", "f", "line 1\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7\nline 8\nline 9\nline 10\nline 11\nline 12\nline 13\nline 14\nline 15\nline 16\nline 17\nline 18\nline 19\nline 20\nline 21\nline 22\nline 23\nline 24\nline 25\nline 26\nline 27\nline 28\nline 29\nline 30\nline 31\nline 32\nline 33\nline 34\nline 35\nline 36\nline 37\nline 38\nline 39\nline 40\n", "line 1\nline 2\nX\nline 4\nline 5\nline 6\nline 7\nline 8\nline 9\nline 10\nline 11\nline 12\nline 13\nline 14\nline 15\nline 16\nline 17\nline 18\nline 19\nline 20\nline 21\nline 22\nline 23\nline 24\nline 25\nline 26\nline 27\nline 28\nline 29\nline 30\nY\nline 32\nline 33\nline 34\nline 35\nline 36\nline 37\nline 38\nline 39\nline 40\n", "Index: f\n===================================================================\n--- f\n+++ f\n@@ -1,7 +1,7 @@\n line 1\n line 2\n-line 3\n+X\n line 4\n line 5\n line 6\n line 7\n@@ -27,9 +27,9 @@\n line 27\n line 28\n line 29\n line 30\n-line 31\n+Y\n line 32\n line 33\n line 34\n line 35\n", 2, 2),
    ("f", "f", "line 1\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7\nline 8\nline 9\nline 10\nline 11\nline 12\nline 13\nline 14\nline 15\nline 16\nline 17\nline 18\nline 19\nline 20\nline 21\nline 22\nline 23\nline 24\nline 25\nline 26\nline 27\nline 28\nline 29\nline 30\nline 31\nline 32\nline 33\nline 34\nline 35\nline 36\nline 37\nline 38\nline 39\nline 40\n", "line 1\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7\nline 8\nline 9\nline 10\nX\nline 12\nline 13\nline 14\nline 15\nline 16\nline 17\nY\nline 19\nline 20\nline 21\nline 22\nline 23\nline 24\nline 25\nline 26\nline 27\nline 28\nline 29\nline 30\nline 31\nline 32\nline 33\nline 34\nline 35\nline 36\nline 37\nline 38\nline 39\nline 40\n", "Index: f\n===================================================================\n--- f\n+++ f\n@@ -7,16 +7,16 @@\n line 7\n line 8\n line 9\n line 10\n-line 11\n+X\n line 12\n line 13\n line 14\n line 15\n line 16\n line 17\n-line 18\n+Y\n line 19\n line 20\n line 21\n line 22\n", 2, 2),
    ("f", "f", "line 1\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7\nline 8\nline 9\nline 10\nline 11\nline 12\nline 13\nline 14\nline 15\nline 16\nline 17\nline 18\nline 19\nline 20\nline 21\nline 22\nline 23\nline 24\nline 25\nline 26\nline 27\nline 28\nline 29\nline 30\nline 31\nline 32\nline 33\nline 34\nline 35\nline 36\nline 37\nline 38\nline 39\nline 40\n", "line 1\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7\nline 8\nline 9\nline 10\nX\nline 12\nline 13\nline 14\nline 15\nline 16\nline 17\nline 18\nline 19\nY\nline 21\nline 22\nline 23\nline 24\nline 25\nline 26\nline 27\nline 28\nline 29\nline 30\nline 31\nline 32\nline 33\nline 34\nline 35\nline 36\nline 37\nline 38\nline 39\nline 40\n", "Index: f\n===================================================================\n--- f\n+++ f\n@@ -7,18 +7,18 @@\n line 7\n line 8\n line 9\n line 10\n-line 11\n+X\n line 12\n line 13\n line 14\n line 15\n line 16\n line 17\n line 18\n line 19\n-line 20\n+Y\n line 21\n line 22\n line 23\n line 24\n", 2, 2),
    ("f", "f", "line 1\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7\nline 8\nline 9\nline 10\nline 11\nline 12\nline 13\nline 14\nline 15\nline 16\nline 17\nline 18\nline 19\nline 20\nline 21\nline 22\nline 23\nline 24\nline 25\nline 26\nline 27\nline 28\nline 29\nline 30\nline 31\nline 32\nline 33\nline 34\nline 35\nline 36\nline 37\nline 38\nline 39\nline 40\n", "line 1\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7\nline 8\nline 9\nline 10\nX\nline 12\nline 13\nline 14\nline 15\nline 16\nline 17\nline 18\nline 19\nline 20\nY\nline 22\nline 23\nline 24\nline 25\nline 26\nline 27\nline 28\nline 29\nline 30\nline 31\nline 32\nline 33\nline 34\nline 35\nline 36\nline 37\nline 38\nline 39\nline 40\n", "Index: f\n===================================================================\n--- f\n+++ f\n@@ -7,9 +7,9 @@\n line 7\n line 8\n line 9\n line 10\n-line 11\n+X\n line 12\n line 13\n line 14\n line 15\n@@ -17,9 +17,9 @@\n line 17\n line 18\n line 19\n line 20\n-line 21\n+Y\n line 22\n line 23\n line 24\n line 25\n", 2, 2),
    ("f", "f", "line 1\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7\nline 8\nline 9\nline 10\nline 11\nline 12\nline 13\nline 14\nline 15\nline 16\nline 17\nline 18\nline 19\nline 20\nline 21\nline 22\nline 23\nline 24\nline 25\nline 26\nline 27\nline 28\nline 29\nline 30\nline 31\nline 32\nline 33\nline 34\nline 35\nline 36\nline 37\nline 38\nline 39\nline 40", "line 4\nline 5\nline 6\nline 7\nline 8\nline 9\nline 10\nline 11\nline 12\nline 13\nline 14\nline 15\nline 16\nline 17\nline 18\nline 19\nline 20\nline 21\nline 22\nline 23\nline 24\nline 25\nline 26\nline 27\nline 28\nline 29\nline 30\nline 31\nline 32\nline 33\nline 34\nline 35\nline 36\nline 37\nline 38\nline 39\nline 40\ntail", "Index: f\n===================================================================\n--- f\n+++ f\n@@ -1,7 +1,4 @@\n-line 1\n-line 2\n-line 3\n line 4\n line 5\n line 6\n line 7\n@@ -36,5 +33,6 @@\n line 36\n line 37\n line 38\n line 39\n-line 40\n\\ No newline at end of file\n+line 40\n+tail\n\\ No newline at end of file\n", 2, 4),
    ("f", "f", "line 1\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7\nline 8\nline 9\nline 10\nline 11\nline 12\nline 13\nline 14\nline 15\nline 16\nline 17\nline 18\nline 19\nline 20\nline 21\nline 22\nline 23\nline 24\nline 25\nline 26\nline 27\nline 28\nline 29\nline 30\nline 31\nline 32\nline 33\nline 34\nline 35\nline 36\nline 37\nline 38\nline 39\nline 40\n", "", "Index: f\n===================================================================\n--- f\n+++ f\n@@ -1,40 +0,0 @@\n-line 1\n-line 2\n-line 3\n-line 4\n-line 5\n-line 6\n-line 7\n-line 8\n-line 9\n-line 10\n-line 11\n-line 12\n-line 13\n-line 14\n-line 15\n-line 16\n-line 17\n-line 18\n-line 19\n-line 20\n-line 21\n-line 22\n-line 23\n-line 24\n-line 25\n-line 26\n-line 27\n-line 28\n-line 29\n-line 30\n-line 31\n-line 32\n-line 33\n-line 34\n-line 35\n-line 36\n-line 37\n-line 38\n-line 39\n-line 40\n", 0, 40),
    ("f", "f", "", "line 1\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7\nline 8\nline 9\nline 10\nline 11\nline 12\nline 13\nline 14\nline 15\nline 16\nline 17\nline 18\nline 19\nline 20\nline 21\nline 22\nline 23\nline 24\nline 25\nline 26\nline 27\nline 28\nline 29\nline 30\nline 31\nline 32\nline 33\nline 34\nline 35\nline 36\nline 37\nline 38\nline 39\nline 40", "Index: f\n===================================================================\n--- f\n+++ f\n@@ -0,0 +1,40 @@\n+line 1\n+line 2\n+line 3\n+line 4\n+line 5\n+line 6\n+line 7\n+line 8\n+line 9\n+line 10\n+line 11\n+line 12\n+line 13\n+line 14\n+line 15\n+line 16\n+line 17\n+line 18\n+line 19\n+line 20\n+line 21\n+line 22\n+line 23\n+line 24\n+line 25\n+line 26\n+line 27\n+line 28\n+line 29\n+line 30\n+line 31\n+line 32\n+line 33\n+line 34\n+line 35\n+line 36\n+line 37\n+line 38\n+line 39\n+line 40\n\\ No newline at end of file\n", 40, 0),
    ("file.txt", "file.txt", "\t}\n\nb\nbar\nd\n  x\n\t}\nbar\nc\n\t}\né ü\nd\nc\nbar\n\n  x\na\n", "\t}\nb\nd\na\né ü\n\t}\n\t}\n\t}\né ü\nd\nc\n  x\na\n\n", "Index: file.txt\n===================================================================\n--- file.txt\n+++ file.txt\n@@ -1,17 +1,14 @@\n \t}\n-\n b\n-bar\n d\n-  x\n+a\n+é ü\n \t}\n-bar\n-c\n \t}\n+\t}\n é ü\n d\n c\n-bar\n-\n   x\n a\n+\n", 4, 7),
    ("file.txt", "file.txt", "\t}\nbar\nc\nd\nd\na\n\n\n\nx\ry\na\nx\ry\nfoo\nx\ry\nb\nb\na\nd\nbar\n  x\n\t}\na\n\nx\ry\nd\n\nb\nbar\n\t}\n", "\t}\nbar\nc\nd\nd\na\n\n\n\nx\ry\na\nx\ry\nfoo\nx\ry\nb\nb\na\nd\n\n  x\n\t}\na\nx\ry\nd\nx\ry\n\nbar\n\t}\nfoo\n", "Index: file.txt\n===================================================================\n--- file.txt\n+++ file.txt\n@@ -15,15 +15,15 @@\n b\n b\n a\n d\n-bar\n+\n   x\n \t}\n a\n-\n x\ry\n d\n+x\ry\n \n-b\n bar\n \t}\n+foo\n", 3, 3),
    ("file.txt", "file.txt", "\t}\nd\né ü\nfoo\nb\n\t}\nfoo\na\né ü\n  x\n\t}\n\t}\nc\n  x\nbar\nc\na\n\n\nd\nd\nd\na\nc\nx\ry\n", "\t}\nb\né ü\nfoo\nb\n\t}\nfoo\na\né ü\n  x\n\t}\n\t}\nc\n  x\nbar\nc\na\nd\n\n\nd\nd\nb\nd\na\nd\n", "Index: file.txt\n===================================================================\n--- file.txt\n+++ file.txt\n@@ -1,6 +1,6 @@\n \t}\n-d\n+b\n é ü\n foo\n b\n \t}\n@@ -14,12 +14,13 @@\n   x\n bar\n c\n a\n+d\n \n \n d\n d\n+b\n d\n a\n-c\n-x\ry\n+d\n", 4, 3),
    ("file.txt", "file.txt", "\nx\ry\né ü\nfoo\nb\n\t}\nbar\n\t}\n\t}\nfoo\nb\n  x\né ü\nc\nfoo\n", "\nx\ry\né ü\nfoo\nb\nbar\n\t}\nfoo\nb\n  x\né ü\nc\nfoo\n", "Index: file.txt\n===================================================================\n--- file.txt\n+++ file.txt\n@@ -2,12 +2,10 @@\n x\ry\n é ü\n foo\n b\n-\t}\n bar\n \t}\n-\t}\n foo\n b\n   x\n é ü\n", 0, 2),
    ("file.txt", "file.txt", "\nd\nx\ry\n  x\nd\n\t}\nb\nfoo\na\na\n\n\nx\ry\n\t}\nbar\n\nb\nd\nb\na\nd\nfoo\n  x\n\né ü\né ü\nc\n\nbar\nfoo\nbar\nbar\nfoo\nd\nd\n", "\nd\nbar\nx\ry\n  x\nd\na\nx\ry\na\na\n\nx\ry\nbar\n\n\nb\nb\na\nd\nfoo\n  x\n\né ü\na\nbar\n\nfoo\nbar\nbar\nd\nd\n", "Index: file.txt\n===================================================================\n--- file.txt\n+++ file.txt\n@@ -1,35 +1,31 @@\n \n d\n+bar\n x\ry\n   x\n d\n-\t}\n-b\n-foo\n a\n+x\ry\n a\n+a\n \n-\n x\ry\n-\t}\n bar\n \n+\n b\n-d\n b\n a\n d\n foo\n   x\n \n é ü\n-é ü\n-c\n-\n+a\n bar\n+\n foo\n bar\n bar\n-foo\n d\n d\n", 6, 10),
    ("file.txt", "file.txt", "x\ry\nx\ry\nc\nbar\n\t}\nx\ry\n\n  x", "x\ry\nx\ry\nc\nbar\n\t}\nx\ry\n\n  x", "Index: file.txt\n===================================================================\n--- file.txt\n+++ file.txt\n", 0, 0),
    ("file.txt", "file.txt", "  x\n\n\t}\nfoo\n\t}\nd\n\t}\nfoo\nd\nbar\n\t}\n  x", "  x\nd\n\t}\nfoo\n\t}\n  x\nd\n\t}\nfoo\n\t}\nbar\n\t}\n  x\n", "Index: file.txt\n===================================================================\n--- file.txt\n+++ file.txt\n@@ -1,12 +1,13 @@\n   x\n-\n+d\n \t}\n foo\n \t}\n+  x\n d\n \t}\n foo\n-d\n+\t}\n bar\n \t}\n-  x\n\\ No newline at end of file\n+  x\n", 4, 3),
    ("file.txt", "file.txt", "\nfoo\nfoo\n  x\n\nbar\na\nb\nb\nb\nfoo\n\t}\n\t}\n\na\na\nc\n\nc\n\t}\n  x\n\t}\n\t}\nd\nb\n  x\nc\nbar\nd\nc\n\nbar\nbar\n  x\nb", "\nfoo\na\n\nbar\nc\na\nb\nb\nb\nfoo\n\t}\n\na\na\né ü\nc\na\nc\n\t}\n  x\n\t}\n\t}\nd\nb\n  x\nc\nbar\né ü\nd\nb\nc\n\nbar\nbar\nx\ry\n  x\nb\n", "Index: file.txt\n===================================================================\n--- file.txt\n+++ file.txt\n@@ -1,22 +1,22 @@\n \n foo\n-foo\n-  x\n+a\n \n bar\n+c\n a\n b\n b\n b\n foo\n \t}\n-\t}\n \n a\n a\n+é ü\n c\n-\n+a\n c\n \t}\n   x\n \t}\n@@ -25,11 +25,14 @@\n b\n   x\n c\n bar\n+é ü\n d\n+b\n c\n \n bar\n bar\n+x\ry\n   x\n-b\n\\ No newline at end of file\n+b\n", 8, 5),
    ("file.txt", "file.txt", "d\na\nd\n\né ü\nc\n\t}\nbar\nc\n\nbar\nc\n\t}\na\nb\nx\ry\nc\nbar\nbar\n  x\nbar\n\nb", "d\na\nd\nbar\n\né ü\nc\n\t}\nbar\nc\n\nbar\nc\nbar\na\nx\ry\nc\nbar\nbar\nbar\n\nb", "Index: file.txt\n===================================================================\n--- file.txt\n+++ file.txt\n@@ -1,7 +1,8 @@\n d\n a\n d\n+bar\n \n é ü\n c\n \t}\n@@ -9,15 +10,13 @@\n c\n \n bar\n c\n-\t}\n+bar\n a\n-b\n x\ry\n c\n bar\n bar\n-  x\n bar\n \n b\n\\ No newline at end of file\n", 2, 3),
    ("file.txt", "file.txt", "  x\na\n\na\n\nc\nfoo\n\nx\ry\nd\n  x\n\t}\nbar\na\n  x\nbar\nfoo\nbar", "\na\n\na\n\nfoo\nx\ry\nd\n  x\n\t}\nbar\na\n  x\nbar\nfoo\nbar", "Index: file.txt\n===================================================================\n--- file.txt\n+++ file.txt\n@@ -1,12 +1,10 @@\n-  x\n+\n a\n \n a\n \n-c\n foo\n-\n x\ry\n d\n   x\n \t}\n", 1, 3),
    ("file.txt", "file.txt", "b\na\nx\ry\na\nbar\n\t}\n\né ü\nd\n\t}\nfoo\nx\ry\n\nc\nb\nfoo\n\na\nb\nc\n  x\nfoo\n  x\nx\ry\nfoo\né ü\né ü\na\n\n\t}\nc\n  x", "b\na\nx\ry\na\nbar\n\t}\n\né ü\n  x\nfoo\nx\ry\n\nc\nb\nfoo\n\nx\ry\n\nc\n  x\nfoo\n  x\nx\ry\nfoo\n\n  x\n", "Index: file.txt\n===================================================================\n--- file.txt\n+++ file.txt\n@@ -5,28 +5,22 @@\n bar\n \t}\n \n é ü\n-d\n-\t}\n+  x\n foo\n x\ry\n \n c\n b\n foo\n \n-a\n-b\n+x\ry\n+\n c\n   x\n foo\n   x\n x\ry\n foo\n-é ü\n-é ü\n-a\n \n-\t}\n-c\n-  x\n\\ No newline at end of file\n+  x\n", 4, 10),
    ("file.txt", "file.txt", "a\nfoo\n\t}\nd\nc\né ü\n\na\n  x\n  x\n\t}\n  x\nfoo\n  x\nfoo\n  x\nc\nb\n\né ü\nd\nx\ry\nx\ry\nfoo\n  x\n", "a\nfoo\n\t}\nd\nc\né ü\n\na\n  x\nb\n  x\n\t}\n  x\nfoo\nfoo\n  x\nc\nb\nx\ry\n\nx\ry\né ü\nd\nx\ry\nx\ry\nd\nfoo\n  x\n", "Index: file.txt\n===================================================================\n--- file.txt\n+++ file.txt\n@@ -6,20 +6,23 @@\n é ü\n \n a\n   x\n+b\n   x\n \t}\n   x\n foo\n-  x\n foo\n   x\n c\n b\n+x\ry\n \n+x\ry\n é ü\n d\n x\ry\n x\ry\n+d\n foo\n   x\n", 4, 1),
    ("file.txt", "file.txt", "b\n  x\n\t}\n\nbar\n\t}\n  x\n\t}\nfoo\n  x\nbar\n  x\n\n  x\nfoo\nb\na", "b\n  x\n\nbar\n  x\n\t}\n\n  x\nd\nbar\n  x\n\n  x\nfoo\nb\n", "Index: file.txt\n===================================================================\n--- file.txt\n+++ file.txt\n@@ -1,17 +1,15 @@\n b\n   x\n-\t}\n \n bar\n-\t}\n   x\n \t}\n-foo\n+\n   x\n+d\n bar\n   x\n \n   x\n foo\n b\n-a\n\\ No newline at end of file\n", 2, 4),
    ("file.txt", "file.txt", "\t}\nx\ry\nbar\nb\n\n  x\né ü\nx\ry\nfoo\nfoo\nb\n\t}\n  x\nb\na\n\t}\nb\nbar\nbar\n  x\nx\ry\nbar\n\nbar\n\t}\n\t}\na\nc\nx\ry\nfoo\n\nbar\nfoo\nd\nfoo\n", "\t}\nx\ry\nbar\nb\n  x\n\n  x\né ü\nx\ry\nfoo\nfoo\nb\nfoo\na\nd\na\n\t}\nb\nbar\nbar\n  x\nx\ry\nbar\n\nbar\n\t}\n\t}\nb\na\nc\nx\ry\n\nbar\nd\nfoo\n\t}", "Index: file.txt\n===================================================================\n--- file.txt\n+++ file.txt\n@@ -1,19 +1,20 @@\n \t}\n x\ry\n bar\n b\n+  x\n \n   x\n é ü\n x\ry\n foo\n foo\n b\n-\t}\n-  x\n-b\n+foo\n a\n+d\n+a\n \t}\n b\n bar\n bar\n@@ -23,13 +24,13 @@\n \n bar\n \t}\n \t}\n+b\n a\n c\n x\ry\n-foo\n \n bar\n-foo\n d\n foo\n+\t}\n\\ No newline at end of file\n", 6, 5),
    ("file.txt", "file.txt", "foo\na\nfoo\na\nfoo\nbar\nfoo\na\nx\ry\né ü\nc\nb\nc\n\né ü\nx\ry\n\t}\n\t}\n", "foo\na\nfoo\na\na\nfoo\nbar\nfoo\na\nb\né ü\nc\nb\n\né ü\nx\ry\n\t}\n\nbar", "Index: file.txt\n===================================================================\n--- file.txt\n+++ file.txt\n@@ -1,18 +1,19 @@\n foo\n a\n foo\n a\n+a\n foo\n bar\n foo\n a\n-x\ry\n+b\n é ü\n c\n b\n-c\n \n é ü\n x\ry\n \t}\n-\t}\n+\n+bar\n\\ No newline at end of file\n", 4, 3),
    ("file.txt", "file.txt", "\t}\r\nx\ry\r\n  x\r\nc\r\na\r\né ü\r\na\r\nx\ry\r\nd\r\nx\ry", "\t}\r\nd\r\n  x\r\na\r\né ü\r\na\r\nx\ry\r\nd", "Index: file.txt\n===================================================================\n--- file.txt\n+++ file.txt\n@@ -1,10 +1,8 @@\n \t}\r\n-x\ry\r\n+d\r\n   x\r\n-c\r\n a\r\n é ü\r\n a\r\n x\ry\r\n-d\r\n-x\ry\n\\ No newline at end of file\n+d\n\\ No newline at end of file\n", 2, 4),
    ("file.txt", "file.txt", "foo\né ü\nc\nfoo\nb\nx\ry\nc\nc\n  x\n  x\n  x\nfoo\nfoo\nd\nfoo\n\na\n  x\n\n\n\t}\n\t}\nbar\né ü\n", "foo\né ü\nc\nx\ry\nb\nx\ry\nc\nc\n  x\n  x\nfoo\nbar\nfoo\nd\nfoo\n\na\n\n\n\t}\n\t}\nbar\nfoo\né ü\nc", "Index: file.txt\n===================================================================\n--- file.txt\n+++ file.txt\n@@ -1,24 +1,25 @@\n foo\n é ü\n c\n-foo\n+x\ry\n b\n x\ry\n c\n c\n   x\n   x\n-  x\n foo\n+bar\n foo\n d\n foo\n \n a\n-  x\n \n \n \t}\n \t}\n bar\n+foo\n é ü\n+c\n\\ No newline at end of file\n", 4, 3),
    ("file.txt", "file.txt", "a\r\né ü\r\na\r\né ü\r\n  x\r\nfoo\r\nd\r\n\r\n  x\r\nd", "a\r\né ü\r\na\r\né ü\r\n  x\r\nfoo\r\nd\r\n\r\n  x\r\nd", "Index: file.txt\n===================================================================\n--- file.txt\n+++ file.txt\n", 0, 0),
    ("file.txt", "file.txt", "c\n\né ü\n  x\nx\ry\n\t}\n\t}\n  x\nbar\nfoo\n\t}\nc\na\n  x\n", "\nc\nfoo\n  x\nx\ry\n\t}\n\t}\n  x\nbar\n\t}\nfoo\n\t}\na\nc\na\n  x\n", "Index: file.txt\n===================================================================\n--- file.txt\n+++ file.txt\n@@ -1,14 +1,16 @@\n-c\n \n-é ü\n+c\n+foo\n   x\n x\ry\n \t}\n \t}\n   x\n bar\n+\t}\n foo\n \t}\n+a\n c\n a\n   x\n", 4, 2),
    ("file.txt", "file.txt", "d\n\t}\né ü\n  x\n  x\nbar\nx\ry\nfoo\nx\ry\n\t}\na\n\t}\n\n\t}\nc\nb\n\t}\nfoo\n  x\n\t}\nfoo\nfoo\nbar\na\nd", "d\n\t}\né ü\na\n  x\nbar\nx\ry\nfoo\nx\ry\n\t}\nd\na\nc\n\t}\n\n\t}\nc\nb\n  x\nfoo\n  x\n\t}\nfoo\nfoo\nbar\na\nd\n", "Index: file.txt\n===================================================================\n--- file.txt\n+++ file.txt\n@@ -1,25 +1,27 @@\n d\n \t}\n é ü\n+a\n   x\n-  x\n bar\n x\ry\n foo\n x\ry\n \t}\n+d\n a\n+c\n \t}\n \n \t}\n c\n b\n-\t}\n+  x\n foo\n   x\n \t}\n foo\n foo\n bar\n a\n-d\n\\ No newline at end of file\n+d\n", 5, 3),
    ("file.txt", "file.txt", "d\nb\n\t}\nb\nbar", "d\na\nb\nb\nbar", "Index: file.txt\n===================================================================\n--- file.txt\n+++ file.txt\n@@ -1,5 +1,5 @@\n d\n+a\n b\n-\t}\n b\n bar\n\\ No newline at end of file\n", 1, 1),
    ("file.txt", "file.txt", "bar\n\t}\nx\ry\nb\na\n\t}\né ü\nbar\na\nc\nc\né ü\nbar\nb\nd\n  x\né ü\né ü\nx\ry\nbar\n\t}\nx\ry", "bar\n\t}\na\nb\na\n\t}\nx\ry\né ü\nbar\na\nc\né ü\né ü\nbar\nb\nd\n  x\né ü\né ü\nx\ry\nbar\n\t}\nx\ry", "Index: file.txt\n===================================================================\n--- file.txt\n+++ file.txt\n@@ -1,16 +1,17 @@\n bar\n \t}\n-x\ry\n+a\n b\n a\n \t}\n+x\ry\n é ü\n bar\n a\n c\n-c\n é ü\n+é ü\n bar\n b\n d\n   x\n", 3, 2),
    ("file.txt", "file.txt", "a\nfoo\nc\né ü\nc\n\nc\né ü\nd\na\nb\nc\nx\ry\nx\ry\nd\nd\n\t}\né ü\nc\nd\nb\n  x", "a\nfoo\nc\né ü\nc\n\nc\nc\né ü\nx\ry\n  x\na\nb\nc\nx\ry\nx\ry\nc\nd\n\t}\né ü\nc\n  x", "Index: file.txt\n===================================================================\n--- file.txt\n+++ file.txt\n@@ -4,19 +4,19 @@\n é ü\n c\n \n c\n+c\n é ü\n-d\n+x\ry\n+  x\n a\n b\n c\n x\ry\n x\ry\n+c\n d\n-d\n \t}\n é ü\n c\n-d\n-b\n   x\n\\ No newline at end of file\n", 4, 4),
    ("file.txt", "file.txt", "\t}\r\nc\r\na\r\na\r\nc\r\nc\r\nx\ry\r\n\t}\r\nfoo\r\nbar\r\nd\r\nbar\r\nc\r\nc\r\n\r\nbar\r\nbar\r\nx\ry\r\nbar\r\na\r\n\t}\r\nbar\r\n\r\nb\r\nb\r\n", "\t}\r\nc\r\na\r\nc\r\na\r\nc\r\nc\r\nx\ry\r\n\t}\r\nbar\r\nd\r\na\r\nc\r\nc\r\nfoo\r\n\r\nbar\r\nbar\r\nx\ry\r\nbar\r\na\r\nx\ry\r\n\r\nbar\r\n\r\nb\r\nb\r\n", "Index: file.txt\n===================================================================\n--- file.txt\n+++ file.txt\n@@ -1,25 +1,27 @@\n \t}\r\n c\r\n a\r\n+c\r\n a\r\n c\r\n c\r\n x\ry\r\n \t}\r\n-foo\r\n bar\r\n d\r\n-bar\r\n+a\r\n c\r\n c\r\n+foo\r\n \r\n bar\r\n bar\r\n x\ry\r\n bar\r\n a\r\n-\t}\r\n+x\ry\r\n+\r\n bar\r\n \r\n b\r\n b\r\n", 5, 3),
    ("file.txt", "file.txt", "c\nfoo\nbar\nb\nbar\nd\nc\nfoo\nd\nc\né ü\na\nfoo\n\t}\n\nbar\na\n  x\nb\n", "c\nfoo\n  x\nbar\nb\nbar\nd\nc\nfoo\nd\nc\n\né ü\na\nfoo\n\t}\n\nc\nx\ry\n  x\nb\n", "Index: file.txt\n===================================================================\n--- file.txt\n+++ file.txt\n@@ -1,19 +1,21 @@\n c\n foo\n+  x\n bar\n b\n bar\n d\n c\n foo\n d\n c\n+\n é ü\n a\n foo\n \t}\n \n-bar\n-a\n+c\n+x\ry\n   x\n b\n", 4, 2),
    ("file.txt", "file.txt", "d\nfoo\nx\ry\n\t}\né ü\na\nbar\nx\ry\nc\nb\na\nfoo\nd\n\t}\n\t}\n  x\na\nx\ry\nd\n  x\nd\nbar\né ü\né ü\n\t}\nd\nb\nb\nbar\nc\né ü\n  x\n  x\nx\ry\n", "d\nfoo\na\né ü\nc\nb\na\nd\n\t}\n\t}\n  x\na\nd\n  x\na\né ü\n\t}\nd\nb\nfoo\nb\nb\nc\né ü\n  x\nx\ry\n", "Index: file.txt\n===================================================================\n--- file.txt\n+++ file.txt\n@@ -1,34 +1,26 @@\n d\n foo\n-x\ry\n-\t}\n-é ü\n a\n-bar\n-x\ry\n+é ü\n c\n b\n a\n-foo\n d\n \t}\n \t}\n   x\n a\n-x\ry\n d\n   x\n-d\n-bar\n+a\n é ü\n-é ü\n \t}\n d\n b\n+foo\n b\n-bar\n+b\n c\n é ü\n   x\n-  x\n x\ry\n", 4, 12),
    ("file.txt", "file.txt", "foo\nb\né ü\n\nb\nx\ry\nx\ry\n  x\nfoo\nd\n\nc\nc\n\nx\ry\nfoo", "\t}\nb\né ü\n\nb\nx\ry\nx\ry\n  x\nfoo\nb\nc\nc\nx\ry\nfoo", "Index: file.txt\n===================================================================\n--- file.txt\n+++ file.txt\n@@ -1,16 +1,14 @@\n-foo\n+\t}\n b\n é ü\n \n b\n x\ry\n x\ry\n   x\n foo\n-d\n-\n+b\n c\n c\n-\n x\ry\n foo\n\\ No newline at end of file\n", 2, 4),
    ("file.txt", "file.txt", "é ü\n\t}\nb\né ü\nfoo\nbar\nx\ry\né ü\n  x\nc\nfoo\nb\n\t}\nx\ry\n  x\nx\ry\nc\nb\n\n  x\nb\nb\nbar\nb\nx\ry\nx\ry\n", "é ü\n\t}\nb\né ü\nd\nfoo\nx\ry\nbar\nx\ry\nc\nfoo\nb\n\t}\nx\ry\n  x\nx\ry\nc\n\nx\ry\nb\nb\nbar\nb\nd\nx\ry\nx\ry\nx\ry", "Index: file.txt\n===================================================================\n--- file.txt\n+++ file.txt\n@@ -1,26 +1,27 @@\n é ü\n \t}\n b\n é ü\n+d\n foo\n+x\ry\n bar\n x\ry\n-é ü\n-  x\n c\n foo\n b\n \t}\n x\ry\n   x\n x\ry\n c\n-b\n \n-  x\n+x\ry\n b\n b\n bar\n b\n+d\n x\ry\n x\ry\n+x\ry\n\\ No newline at end of file\n", 5, 4),
    ("file.txt", "file.txt", "\t}\r\nbar\r\nc\r\na\r\né ü\r\nx\ry\r\né ü\r\nc\r\nd\r\nd\r\nc\r\nc\r\nx\ry\r\n", "\t}\r\nc\r\nc\r\na\r\né ü\r\nd\r\nx\ry\r\né ü\r\n\t}\r\nc\r\nd\r\n\t}\r\nc\r\né ü\r\nx\ry\r\n", "Index: file.txt\n===================================================================\n--- file.txt\n+++ file.txt\n@@ -1,13 +1,15 @@\n \t}\r\n-bar\r\n c\r\n+c\r\n a\r\n é ü\r\n+d\r\n x\ry\r\n é ü\r\n+\t}\r\n c\r\n d\r\n-d\r\n+\t}\r\n c\r\n-c\r\n+é ü\r\n x\ry\r\n", 5, 3),
    ("file.txt", "file.txt", "\r\nfoo\r\né ü\r\nb\r\nd\r\nb\r\n\t}\r\nd\r\nd\r\n\r\nbar\r\nd\r\nb\r\n  x\r\n  x\r\n\r\n\r\na\r\nd\r\nbar\r\n\r\nb\r\n\t}\r\n  x\r\né ü\r\n  x\r\nbar\r\nx\ry\r\nc\r\na\r\né ü\r\nb\r\nx\ry\r\n\t}\r\nbar\r\na\r\n\t}\r\n\t}\r\nc\r\n", "\r\nfoo\r\n\r\nb\r\nd\r\nx\ry\r\n\t}\r\nd\r\n\r\nbar\r\nd\r\nb\r\n  x\r\n  x\r\n\t}\r\n\r\na\r\nd\r\né ü\r\nbar\r\n\r\nb\r\n\t}\r\né ü\r\n  x\r\nx\ry\r\nc\r\na\r\né ü\r\n\r\nx\ry\r\n\t}\r\nb\r\n\t}\r\nd\r\nc", "Index: file.txt\n===================================================================\n--- file.txt\n+++ file.txt\n@@ -1,39 +1,36 @@\n \r\n foo\r\n-é ü\r\n+\r\n b\r\n d\r\n-b\r\n+x\ry\r\n \t}\r\n d\r\n-d\r\n \r\n bar\r\n d\r\n b\r\n   x\r\n   x\r\n+\t}\r\n \r\n-\r\n a\r\n d\r\n+é ü\r\n bar\r\n \r\n b\r\n \t}\r\n-  x\r\n é ü\r\n   x\r\n-bar\r\n x\ry\r\n c\r\n a\r\n é ü\r\n-b\r\n+\r\n x\ry\r\n \t}\r\n-bar\r\n-a\r\n+b\r\n \t}\r\n-\t}\r\n-c\r\n+d\r\n+c\n\\ No newline at end of file\n", 8, 11),
    ("file.txt", "file.txt", "a\r\né ü\r\nc\r\nc\r\nc\r\n  x\r\n  x\r\nx\ry\r\n\r\n\r\nfoo\r\n  x\r\nbar\r\na\r\n  x\r\nx\ry\r\nb\r\na\r\nx\ry\r\né ü\r\nc\r\na\r\n  x\r\n\r\nbar\r\né ü\r\na\r\n\t}\r\nfoo\r\nb\r\nx\ry\r\nx\ry\r\n\r\n\r\n  x\r\nfoo\r\na\r\n", "a\r\né ü\r\n\r\nc\r\nc\r\n\t}\r\n  x\r\n  x\r\nx\ry\r\n\r\n\t}\r\n\r\n\r\nbar\r\na\r\n  x\r\n  x\r\nx\ry\r\na\r\nbar\r\nc\r\nc\r\nb\r\n  x\r\n\r\nbar\r\na\r\n\t}\r\nfoo\r\n  x\r\nx\ry\r\né ü\r\n\r\n  x\r\nfoo\r\na", "Index: file.txt\n===================================================================\n--- file.txt\n+++ file.txt\n@@ -1,37 +1,36 @@\n a\r\n é ü\r\n+\r\n c\r\n c\r\n-c\r\n+\t}\r\n   x\r\n   x\r\n x\ry\r\n \r\n+\t}\r\n \r\n-foo\r\n-  x\r\n+\r\n bar\r\n a\r\n   x\r\n+  x\r\n x\ry\r\n-b\r\n a\r\n-x\ry\r\n-é ü\r\n+bar\r\n c\r\n-a\r\n+c\r\n+b\r\n   x\r\n \r\n bar\r\n-é ü\r\n a\r\n \t}\r\n foo\r\n-b\r\n+  x\r\n x\ry\r\n-x\ry\r\n+é ü\r\n \r\n-\r\n   x\r\n foo\r\n-a\r\n+a\n\\ No newline at end of file\n", 11, 12),
    ("file.txt", "file.txt", "bar\nx\ry\n  x\né ü\nbar\nx\ry\n  x\nb\n\t}\n\t}\na\nfoo\nx\ry\nfoo\n  x\nfoo\nd\nbar\nd\n\t}\nd\nc\né ü\nbar\nc\n\nd\n\n", "bar\nx\ry\n  x\nfoo\né ü\nbar\nx\ry\n  x\nb\n\t}\na\n\t}\na\nfoo\nx\ry\nfoo\n  x\nfoo\nd\nbar\nd\n\t}\nfoo\nd\né ü\nbar\nc\n\nfoo\n\n", "Index: file.txt\n===================================================================\n--- file.txt\n+++ file.txt\n@@ -1,13 +1,15 @@\n bar\n x\ry\n   x\n+foo\n é ü\n bar\n x\ry\n   x\n b\n \t}\n+a\n \t}\n a\n foo\n x\ry\n@@ -17,12 +19,12 @@\n d\n bar\n d\n \t}\n+foo\n d\n-c\n é ü\n bar\n c\n \n-d\n+foo\n \n", 4, 2),
    ("file.txt", "file.txt", "a\n\t}\n\n\na\nx\ry\nb\na\nb\nc\nx\ry\n\t}\nc\na\na\nx\ry\né ü\n\t}\nx\ry\nfoo\n\t}\nx\ry\n\nc\né ü\na\n  x\n  x\nd\n  x\nc\nfoo\n", "a\n\t}\n\n\na\nx\ry\nb\na\nc\nb\nx\ry\nc\n\t}\na\na\nx\ry\né ü\n\t}\nx\ry\nx\ry\n\t}\nx\ry\nbar\nc\né ü\na\n  x\n  x\n\nd\n  x\na\nfoo\nx\ry", "Index: file.txt\n===================================================================\n--- file.txt\n+++ file.txt\n@@ -5,28 +5,30 @@\n a\n x\ry\n b\n a\n-b\n c\n+b\n x\ry\n-\t}\n c\n+\t}\n a\n a\n x\ry\n é ü\n \t}\n x\ry\n-foo\n+x\ry\n \t}\n x\ry\n-\n+bar\n c\n é ü\n a\n   x\n   x\n+\n d\n   x\n-c\n+a\n foo\n+x\ry\n\\ No newline at end of file\n", 7, 5),
    ("file.txt", "file.txt", "a\n\nb\nb\nc\n\n\nx\ry\nd\nd\nbar\nb\n\t}\nc\nb\nb\nd\nd\n  x\n\nc\nc\nd\n", "a\n\nb\nb\né ü\nc\n\nfoo\nx\ry\nd\nd\nbar\nb\nc\nb\nb\nd\nd\n  x\n\nx\ry\nc\nc\nc\n", "Index: file.txt\n===================================================================\n--- file.txt\n+++ file.txt\n@@ -1,23 +1,24 @@\n a\n \n b\n b\n+é ü\n c\n \n-\n+foo\n x\ry\n d\n d\n bar\n b\n-\t}\n c\n b\n b\n d\n d\n   x\n \n+x\ry\n c\n c\n-d\n+c\n", 4, 3),
    ("file.txt", "file.txt", "foo\r\nc\r\n\t}\r\n  x\r\nc\r\n\t}\r\n\t}\r\nbar\r\n  x\r\nfoo\r\nb\r\nb\r\nbar\r\na\r\n  x\r\n  x\r\na\r\n\r\nd\r\nb\r\nx\ry\r\nb\r\nc\r\nb\r\nc\r\né ü\r\na\r\n", "foo\r\nc\r\n\t}\r\n  x\r\n  x\r\nc\r\n  x\r\n\t}\r\nfoo\r\nbar\r\n  x\r\nb\r\nb\r\nbar\r\n  x\r\n  x\r\na\r\n\r\nd\r\nb\r\nx\ry\r\nb\r\n\t}\r\nb\r\nc\r\né ü\r\na\r\n", "Index: file.txt\n===================================================================\n--- file.txt\n+++ file.txt\n@@ -1,27 +1,27 @@\n foo\r\n c\r\n \t}\r\n   x\r\n+  x\r\n c\r\n+  x\r\n \t}\r\n-\t}\r\n+foo\r\n bar\r\n   x\r\n-foo\r\n b\r\n b\r\n bar\r\n-a\r\n   x\r\n   x\r\n a\r\n \r\n d\r\n b\r\n x\ry\r\n b\r\n-c\r\n+\t}\r\n b\r\n c\r\n é ü\r\n a\r\n", 4, 4),
    ("file.txt", "file.txt", "x\ry\nbar\nbar\né ü\na\nb\na\nb\n\t}\nbar\n\t}\nfoo\nc\né ü\na\nfoo\nd\né ü\nbar\na\n  x\nbar\nc\nb\né ü\nbar\n\nx\ry\nd\n\t}\na\né ü\na\nx\ry\nd\nb\né ü\n  x\nd", "x\ry\nbar\nbar\né ü\na\nb\nbar\nb\nbar\n\t}\nc\né ü\nx\ry\na\nfoo\nd\nx\ry\né ü\nbar\n  x\n  x\nb\né ü\nbar\n\nx\ry\nd\n\t}\na\né ü\na\nx\ry\nb\né ü\n  x\nd", "Index: file.txt\n===================================================================\n--- file.txt\n+++ file.txt\n@@ -3,25 +3,23 @@\n bar\n é ü\n a\n b\n-a\n+bar\n b\n-\t}\n bar\n \t}\n-foo\n c\n é ü\n+x\ry\n a\n foo\n d\n+x\ry\n é ü\n bar\n-a\n   x\n-bar\n-c\n+  x\n b\n é ü\n bar\n \n@@ -31,9 +29,8 @@\n a\n é ü\n a\n x\ry\n-d\n b\n é ü\n   x\n d\n\\ No newline at end of file\n", 4, 7),
    ("file.txt", "file.txt", "é ü\n\t}\na\nc\n\nbar\n\nbar\n\t}\nfoo\nc\nfoo\nc\nx\ry\na\n  x\n\n\n\t}\nbar\nd\nc\na\nbar\nc\na\n\t}\n  x\nfoo\nbar\nb\nb\n  x\nb\nb\na\n", "é ü\nb\n\t}\nc\n\nbar\n\t}\nfoo\nc\nfoo\n\t}\nc\nx\ry\na\na\nx\ry\n\n  x\n\t}\nbar\nd\nc\na\nc\n\t}\n  x\nfoo\nbar\nb\né ü\n  x\nd\nfoo\n", "Index: file.txt\n===================================================================\n--- file.txt\n+++ file.txt\n@@ -1,36 +1,33 @@\n é ü\n+b\n \t}\n-a\n c\n \n bar\n-\n-bar\n \t}\n foo\n c\n foo\n+\t}\n c\n x\ry\n a\n-  x\n+a\n+x\ry\n \n-\n+  x\n \t}\n bar\n d\n c\n a\n-bar\n c\n-a\n \t}\n   x\n foo\n bar\n b\n-b\n+é ü\n   x\n-b\n-b\n-a\n+d\n+foo\n", 8, 11),
    ("file.txt", "file.txt", "b\nd\n\t}\nfoo\né ü\né ü\n\t}\nbar\n  x\nc\n  x\nc\n", "b\nd\n\t}\nfoo\né ü\né ü\nbar\n  x\nc\n  x\nc\n", "Index: file.txt\n===================================================================\n--- file.txt\n+++ file.txt\n@@ -3,9 +3,8 @@\n \t}\n foo\n é ü\n é ü\n-\t}\n bar\n   x\n c\n   x\n", 0, 1),
    ("file.txt", "file.txt", "foo\r\nx\ry\r\n\t}\r\né ü\r\nx\ry\r\na\r\nbar\r\nc\r\né ü\r\né ü\r\nd\r\n\t}\r\nd\r\n  x\r\nfoo\r\n\r\nx\ry\r\n\t}\r\n\r\n\r\n\r\nc\r\n\r\n\r\nx\ry\r\nb\r\nd\r\n\r\n\r\nfoo\r\n\r\né ü\r\na\r\nd\r\nx\ry\r\nd\r\n", "\r\nx\ry\r\n\t}\r\né ü\r\nx\ry\r\nbar\r\nc\r\né ü\r\né ü\r\nd\r\n\t}\r\n  x\r\n  x\r\nfoo\r\n\t}\r\nx\ry\r\n\t}\r\n\r\n\r\nc\r\n\r\n\r\nb\r\nd\r\n\r\n\r\nfoo\r\n\r\nd\r\n\t}\r\na\r\nx\ry\r\nx\ry", "Index: file.txt\n===================================================================\n--- file.txt\n+++ file.txt\n@@ -1,36 +1,33 @@\n-foo\r\n+\r\n x\ry\r\n \t}\r\n é ü\r\n x\ry\r\n-a\r\n bar\r\n c\r\n é ü\r\n é ü\r\n d\r\n \t}\r\n-d\r\n   x\r\n+  x\r\n foo\r\n-\r\n+\t}\r\n x\ry\r\n \t}\r\n \r\n \r\n-\r\n c\r\n \r\n \r\n-x\ry\r\n b\r\n d\r\n \r\n \r\n foo\r\n \r\n-é ü\r\n-a\r\n d\r\n+\t}\r\n+a\r\n x\ry\r\n-d\r\n+x\ry\n\\ No newline at end of file\n", 6, 9),
    ("file.txt", "file.txt", "a\nfoo\né ü\na\nx\ry\n\t}\n", "é ü\na\nx\ry\n\t}\n", "Index: file.txt\n===================================================================\n--- file.txt\n+++ file.txt\n@@ -1,6 +1,4 @@\n-a\n-foo\n é ü\n a\n x\ry\n \t}\n", 0, 2),
    ];
}
