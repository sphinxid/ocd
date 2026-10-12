//! Character classification helpers (acorn's `identifier.js`, `whitespace.js` and friends).

use super::id_tables::{ASTRAL_ID_CHAR, ASTRAL_ID_START, BMP_ID_CHAR, BMP_ID_START};
use std::cmp::Ordering;

/// Stand-in for the `NaN` that `charCodeAt` returns past the end of the input. It compares like `NaN` in every
/// "less than" test acorn does on character codes; the few places where it would not are handled explicitly.
pub(super) const EOF_CODE: u32 = u32::MAX;

fn in_table(table: &[(u16, u16)], code: u32) -> bool {
    let c = code as u16;
    table
        .binary_search_by(|&(lo, hi)| {
            if hi < c {
                Ordering::Less
            } else if lo > c {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        })
        .is_ok()
}

fn is_in_astral_set(code: u32, set: &[u32]) -> bool {
    let mut pos: u64 = 0x10000;
    let code = code as u64;
    let mut i = 0;
    while i + 1 < set.len() {
        pos += set[i] as u64;
        if pos > code {
            return false;
        }
        pos += set[i + 1] as u64;
        if pos >= code {
            return true;
        }
        i += 2;
    }
    false
}

/// acorn's `isIdentifierStart(code, astral)`.
pub(super) fn is_identifier_start(code: u32, astral: bool) -> bool {
    if code < 65 {
        return code == 36;
    }
    if code < 91 {
        return true;
    }
    if code < 97 {
        return code == 95;
    }
    if code < 123 {
        return true;
    }
    if code <= 0xffff {
        return code >= 0xaa && in_table(BMP_ID_START, code);
    }
    if !astral {
        return false;
    }
    is_in_astral_set(code, ASTRAL_ID_START)
}

/// acorn's `isIdentifierChar(code, astral)`.
pub(super) fn is_identifier_char(code: u32, astral: bool) -> bool {
    if code < 48 {
        return code == 36;
    }
    if code < 58 {
        return true;
    }
    if code < 65 {
        return false;
    }
    if code < 91 {
        return true;
    }
    if code < 97 {
        return code == 95;
    }
    if code < 123 {
        return true;
    }
    if code <= 0xffff {
        return code >= 0xaa && in_table(BMP_ID_CHAR, code);
    }
    if !astral {
        return false;
    }
    is_in_astral_set(code, ASTRAL_ID_START) || is_in_astral_set(code, ASTRAL_ID_CHAR)
}

/// acorn's `isNewLine`.
pub(super) fn is_new_line(code: u32) -> bool {
    code == 10 || code == 13 || code == 0x2028 || code == 0x2029
}

/// acorn's `nonASCIIwhitespace` (`[  -   　﻿]`).
pub(super) fn is_non_ascii_whitespace(code: u32) -> bool {
    code == 0x1680
        || (0x2000..=0x200a).contains(&code)
        || code == 0x202f
        || code == 0x205f
        || code == 0x3000
        || code == 0xfeff
}

/// JavaScript's regular expression `\s`.
pub(super) fn is_js_space(code: u32) -> bool {
    matches!(code, 9..=13 | 32 | 0xa0 | 0x1680 | 0x2000..=0x200a | 0x2028 | 0x2029 | 0x202f | 0x205f | 0x3000 | 0xfeff)
}

/// `lineBreak.test(input.slice(from, to))`.
pub(super) fn has_line_break(s: &[u16], from: usize, to: usize) -> bool {
    let to = to.min(s.len());
    from < to && s[from..to].iter().any(|&c| is_new_line(c as u32))
}

/// Runs acorn's `skipWhiteSpace` regex (`/(?:\s|\/\/.*|\/\*[^]*?\*\/)*/g`) at `start` and returns the end of the
/// match.
pub(super) fn skip_white_space(s: &[u16], start: usize) -> usize {
    let len = s.len();
    let mut i = start;
    loop {
        if i < len && is_js_space(s[i] as u32) {
            i += 1;
            continue;
        }
        if i + 1 < len && s[i] == b'/' as u16 && s[i + 1] == b'/' as u16 {
            i += 2;
            while i < len && !is_new_line(s[i] as u32) {
                i += 1;
            }
            continue;
        }
        if i + 1 < len && s[i] == b'/' as u16 && s[i + 1] == b'*' as u16 {
            match find(s, &[b'*' as u16, b'/' as u16], i + 2) {
                Some(end) => {
                    i = end + 2;
                    continue;
                }
                None => break,
            }
        }
        break;
    }
    i
}

/// `String.prototype.indexOf` on UTF-16 code units.
pub(super) fn find(s: &[u16], needle: &[u16], from: usize) -> Option<usize> {
    if needle.is_empty() {
        return Some(from.min(s.len()));
    }
    if from >= s.len() || s.len() - from < needle.len() {
        return None;
    }
    (from..=s.len() - needle.len()).find(|&i| &s[i..i + needle.len()] == needle)
}

/// acorn's `nextLineBreak(code, from, end)`.
pub(super) fn next_line_break(s: &[u16], from: usize, end: usize) -> Option<usize> {
    let end = end.min(s.len());
    let mut i = from;
    while i < end {
        let next = s[i] as u32;
        if is_new_line(next) {
            return Some(if i + 1 < end && next == 13 && s[i + 1] == 10 { i + 2 } else { i + 1 });
        }
        i += 1;
    }
    None
}

/// acorn's `getLineInfo(input, offset)`: `(line, column)`.
pub(super) fn get_line_info(s: &[u16], offset: usize) -> (u32, u32) {
    let mut line = 1u32;
    let mut cur = 0usize;
    loop {
        match next_line_break(s, cur, offset) {
            None => return (line, offset.saturating_sub(cur) as u32),
            Some(next) => {
                line += 1;
                cur = next;
            }
        }
    }
}

/// acorn's `codePointToString`, appended to `out`.
pub(super) fn push_code_point(out: &mut Vec<u16>, code: u32) {
    if code <= 0xffff {
        out.push(code as u16);
    } else {
        let c = code - 0x10000;
        out.push(((c >> 10) + 0xd800) as u16);
        out.push(((c & 1023) + 0xdc00) as u16);
    }
}
