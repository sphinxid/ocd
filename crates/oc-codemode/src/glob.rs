//! Glob matching as Bun 1.4's `Bun.Glob#match` does it, for the `path` global's `matchesGlob`: `*` (any characters
//! but `/`, dot files included), `**` as a whole segment (any number of segments, none included), `?`, character
//! classes (`[a-z]`, `[!a]`, `[^a]`, a leading `]` is literal), braces (`{a,b}`, nested, empty alternatives,
//! expanded before anything else), the
//! extended patterns `@(a|b)`, `?(a)`, `+(a)`, `*(a)` and `!(a)`, backslash escapes (`\b`, `\n`, `\r` and `\t` are
//! control characters), and leading `!`s negating the whole pattern. Matching is by Unicode scalar value and
//! case-sensitive. An unclosed `[`, `{` or `(` is literal.

use std::collections::BTreeSet;

#[derive(Debug, Clone)]
enum Node {
    Char(char),
    /// `?`: one character other than `/`.
    Any,
    /// `*`: any characters other than `/`.
    Star,
    /// `**/` starting a segment: nothing, or whole segments each ending in `/`.
    GlobstarSlash,
    /// `**` as the last segment: anything.
    Globstar,
    Class { negated: bool, ranges: Vec<(char, char)> },
    /// `@(a|b)` and the like.
    Extended { op: char, alternatives: Vec<Vec<Node>> },
}

/// Whether `path` matches `pattern`.
pub fn matches(pattern: &str, path: &str) -> bool {
    let chars: Vec<char> = pattern.chars().collect();
    let mut start = 0;
    let mut negated = false;
    while start < chars.len() && chars[start] == '!' && chars.get(start + 1) != Some(&'(') {
        negated = !negated;
        start += 1;
    }
    let path: Vec<char> = path.chars().collect();
    let mut expanded = Vec::new();
    expand(&chars[start..], &mut expanded);
    let found = expanded.iter().any(|pattern| {
        let nodes = Parser { chars: pattern }.sequence(0, pattern.len(), &[]).0;
        ends(&nodes, &path, 0).contains(&path.len())
    });
    found != negated
}

/// Where an expanded brace group started and ended, which match nothing: a `**` first in a group can start a segment
/// and one last in it can be followed by `/` (`{**,b}/y` is `**/y` or `b/y`), but one right after a group doesn't
/// start a segment (`{}**` is `*`), nor is one right before a group last (`**{}` is `*`).
const GROUP_START: char = '\u{E000}';
const GROUP_END: char = '\u{E001}';

/// The most patterns brace expansion makes; the rest are dropped.
const MAX_EXPANSIONS: usize = 4096;

// Brace expansion, before anything else is matched (so `{**,b}/y` is `**/y` or `b/y`): the first closed `{...}`
// outside classes and escapes, split at its top-level commas, each alternative expanded again with what follows.
fn expand(pattern: &[char], out: &mut Vec<Vec<char>>) {
    if out.len() >= MAX_EXPANSIONS {
        return;
    }
    let parser = Parser { chars: pattern };
    let mut i = 0;
    while i < pattern.len() {
        match pattern[i] {
            '\\' => i += 2,
            '[' => match parser.class(i + 1, pattern.len()) {
                Some((_, next)) => i = next,
                None => i += 1,
            },
            '{' => {
                let Some(close) = parser.closing(i + 1, pattern.len(), '{', '}') else {
                    i += 1;
                    continue;
                };
                let mut alternatives = Vec::new();
                let (mut depth, mut from, mut j) = (0, i + 1, i + 1);
                while j < close {
                    match pattern[j] {
                        '\\' => j += 1,
                        '[' => {
                            if let Some((_, next)) = parser.class(j + 1, close) {
                                j = next;
                                continue;
                            }
                        }
                        '{' => depth += 1,
                        '}' => depth -= 1,
                        ',' if depth == 0 => {
                            alternatives.push(&pattern[from..j]);
                            from = j + 1;
                        }
                        _ => {}
                    }
                    j += 1;
                }
                alternatives.push(&pattern[from..close]);
                for alternative in alternatives {
                    let mut next = pattern[..i].to_vec();
                    next.push(GROUP_START);
                    next.extend_from_slice(alternative);
                    next.push(GROUP_END);
                    next.extend_from_slice(&pattern[close + 1..]);
                    expand(&next, out);
                }
                return;
            }
            _ => i += 1,
        }
    }
    out.push(pattern.to_vec());
}

struct Parser<'a> {
    chars: &'a [char],
}

impl Parser<'_> {
    // Parses up to `end` or the first of `stops` at this level; returns the nodes and where it stopped.
    fn sequence(&self, mut i: usize, end: usize, stops: &[char]) -> (Vec<Node>, usize) {
        let c = self.chars;
        let segment_start = i;
        let mut nodes = Vec::new();
        while i < end {
            let ch = c[i];
            if stops.contains(&ch) {
                break;
            }
            match ch {
                '\\' => {
                    if i + 1 < end {
                        nodes.push(Node::Char(escaped(c[i + 1])));
                        i += 2;
                    } else {
                        nodes.push(Node::Char('\\'));
                        i += 1;
                    }
                }
                '*' | '?' | '+' | '@' | '!' if c.get(i + 1) == Some(&'(') => match self.group(i + 2, end, '(', ')', '|') {
                    Some((alternatives, next)) => {
                        nodes.push(Node::Extended { op: ch, alternatives });
                        i = next;
                    }
                    None => {
                        nodes.push(self.plain(ch));
                        i += 1;
                    }
                },
                '*' => {
                    let mut j = i;
                    while j < end && c[j] == '*' {
                        j += 1;
                    }
                    // The last star of `**(b)` starts an extended pattern.
                    if j < end && c[j] == '(' && self.closing(j + 1, end, '(', ')').is_some() {
                        j -= 1;
                    }
                    // A run of stars is read two at a time, each `**` a possible globstar, then a lone `*`.
                    while i < j {
                        if j - i == 1 {
                            nodes.push(Node::Star);
                            i = j;
                            break;
                        }
                        let after = i + 2;
                        // `**/**/**` counts as one `**`, which then starts a segment.
                        let mut k = after;
                        while k + 3 <= end && c[k] == '/' && c[k + 1] == '*' && c[k + 2] == '*' && c.get(k + 3) != Some(&'*') {
                            k += 3;
                        }
                        let mut b = i;
                        while b > segment_start && c[b - 1] == GROUP_START {
                            b -= 1;
                        }
                        let starts_segment = b == segment_start || c[b - 1] == '/' || k > after;
                        // What follows, past where an expanded brace group ended.
                        let mut f = k;
                        while f < end && c[f] == GROUP_END {
                            f += 1;
                        }
                        let ends_pattern = f == end || stops.contains(&c[f]);
                        if starts_segment && f < end && c[f] == '/' {
                            nodes.push(Node::GlobstarSlash);
                            i = f + 1;
                        } else if (starts_segment && ends_pattern) || k > after {
                            // `**` ending the pattern, or `**/**` before more of a segment: anything.
                            nodes.push(Node::Globstar);
                            i = k;
                        } else {
                            nodes.push(Node::Star);
                            i = after;
                        }
                        if i != after || k > after {
                            break;
                        }
                    }
                }
                GROUP_START | GROUP_END => i += 1,
                '?' => {
                    nodes.push(Node::Any);
                    i += 1;
                }
                '[' => match self.class(i + 1, end) {
                    Some((node, next)) => {
                        nodes.push(node);
                        i = next;
                    }
                    None => {
                        nodes.push(Node::Char('['));
                        i += 1;
                    }
                },
                other => {
                    nodes.push(Node::Char(other));
                    i += 1;
                }
            }
        }
        (nodes, i)
    }

    fn plain(&self, ch: char) -> Node {
        match ch {
            '*' => Node::Star,
            '?' => Node::Any,
            other => Node::Char(other),
        }
    }

    // The alternatives of a group opened before `i`, and the index after its `close`; `None` when it never closes.
    fn group(&self, i: usize, end: usize, open: char, close: char, separator: char) -> Option<(Vec<Vec<Node>>, usize)> {
        let close_at = self.closing(i, end, open, close)?;
        let mut alternatives = Vec::new();
        let mut at = i;
        loop {
            let (nodes, stop) = self.sequence(at, close_at, &[separator]);
            alternatives.push(nodes);
            if stop >= close_at {
                break;
            }
            at = stop + 1;
        }
        Some((alternatives, close_at + 1))
    }

    // Where the group opened before `i` closes, skipping escapes, classes and nested groups.
    fn closing(&self, mut i: usize, end: usize, open: char, close: char) -> Option<usize> {
        let c = self.chars;
        let mut depth = 0;
        while i < end {
            match c[i] {
                '\\' => i += 1,
                '[' => {
                    if let Some((_, next)) = self.class(i + 1, end) {
                        i = next;
                        continue;
                    }
                }
                ch if ch == open => depth += 1,
                ch if ch == close => {
                    if depth == 0 {
                        return Some(i);
                    }
                    depth -= 1;
                }
                _ => {}
            }
            i += 1;
        }
        None
    }

    // A character class whose `[` is before `i`; `None` when it never closes.
    fn class(&self, mut i: usize, end: usize) -> Option<(Node, usize)> {
        let c = self.chars;
        let mut negated = false;
        if i < end && (c[i] == '!' || c[i] == '^') {
            negated = true;
            i += 1;
        }
        let mut ranges = Vec::new();
        let mut first = true;
        while i < end && (first || c[i] != ']') {
            let (low, next) = self.class_char(i, end)?;
            i = next;
            let high = if i + 1 < end && c[i] == '-' && c[i + 1] != ']' {
                let (high, next) = self.class_char(i + 1, end)?;
                i = next;
                high
            } else {
                low
            };
            ranges.push((low, high));
            first = false;
        }
        if i >= end {
            return None;
        }
        Some((Node::Class { negated, ranges }, i + 1))
    }

    fn class_char(&self, i: usize, end: usize) -> Option<(char, usize)> {
        let c = self.chars;
        if c[i] == '\\' {
            if i + 1 < end {
                return Some((escaped(c[i + 1]), i + 2));
            }
            return None;
        }
        Some((c[i], i + 1))
    }
}

// An escaped character: Bun reads `\b`, `\n`, `\r` and `\t` as control characters, as C does.
fn escaped(c: char) -> char {
    match c {
        'b' => '\u{8}',
        'n' => '\n',
        'r' => '\r',
        't' => '\t',
        other => other,
    }
}

// Every index of `path` where `nodes`, matched from `start`, can end.
fn ends(nodes: &[Node], path: &[char], start: usize) -> BTreeSet<usize> {
    let mut at = BTreeSet::from([start]);
    for (index, node) in nodes.iter().enumerate() {
        let mut next = BTreeSet::new();
        for &p in &at {
            step(node, path, p, &mut next);
            // A `**/` that a `*` follows doesn't take a `/` that ends the path.
            if matches!(node, Node::GlobstarSlash) && matches!(nodes.get(index + 1), Some(Node::Star)) && p < path.len() {
                next.remove(&path.len());
            }
        }
        if next.is_empty() {
            return next;
        }
        at = next;
    }
    at
}

fn step(node: &Node, path: &[char], p: usize, out: &mut BTreeSet<usize>) {
    match node {
        Node::Char(c) => {
            if path.get(p) == Some(c) {
                out.insert(p + 1);
            }
        }
        Node::Any => {
            if matches!(path.get(p), Some(c) if *c != '/') {
                out.insert(p + 1);
            }
        }
        Node::Star => {
            out.insert(p);
            let mut q = p;
            while q < path.len() && path[q] != '/' {
                q += 1;
                out.insert(q);
            }
        }
        Node::GlobstarSlash => {
            out.insert(p);
            for (q, c) in path.iter().enumerate().skip(p) {
                if *c == '/' {
                    out.insert(q + 1);
                }
            }
        }
        Node::Globstar => out.extend(p..=path.len()),
        Node::Class { negated, ranges } => {
            if let Some(&c) = path.get(p) {
                let inside = ranges.iter().any(|&(low, high)| low <= c && c <= high);
                if inside != *negated {
                    out.insert(p + 1);
                }
            }
        }
        Node::Extended { op, alternatives } => {
            let once = |from: usize| -> BTreeSet<usize> {
                let mut found = BTreeSet::new();
                for alternative in alternatives {
                    found.extend(ends(alternative, path, from));
                }
                found
            };
            match op {
                '@' => out.extend(once(p)),
                '?' => {
                    out.insert(p);
                    out.extend(once(p));
                }
                '+' | '*' => {
                    if *op == '*' {
                        out.insert(p);
                    }
                    let mut frontier: Vec<usize> = vec![p];
                    let mut seen = BTreeSet::new();
                    while let Some(from) = frontier.pop() {
                        for q in once(from) {
                            if q > from && seen.insert(q) {
                                frontier.push(q);
                            }
                            out.insert(q);
                        }
                    }
                }
                _ => {
                    // `!(...)`: any run within the segment that the alternatives don't match exactly.
                    let excluded = once(p);
                    let mut q = p;
                    loop {
                        if !excluded.contains(&q) {
                            out.insert(q);
                        }
                        if q >= path.len() || path[q] == '/' {
                            break;
                        }
                        q += 1;
                    }
                }
            }
        }
    }
}
