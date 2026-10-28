//! A port of `fuzzysort` 3.1.0 `go(search, targets, {keys})` (scores, ordering and the result heap),
//! working on UTF-16 code units like the JavaScript original.

use unicode_normalization::UnicodeNormalization;

const NEG_INF: f64 = f64::NEG_INFINITY;

fn is_latin(c: char) -> bool {
    c.is_ascii_alphabetic()
        || matches!(c as u32, 0xAA | 0xBA | 0xC0..=0xD6 | 0xD8..=0xF6 | 0xF8..=0x24F | 0x1E00..=0x1EFF | 0x2C60..=0x2C7F | 0xA720..=0xA7FF | 0xFF21..=0xFF3A | 0xFF41..=0xFF5A)
}

/// `remove_accents`: NFD on Latin runs, then drop U+0300..U+036F.
fn remove_accents(s: &str) -> String {
    let mut out = String::new();
    let mut run = String::new();
    for c in s.chars() {
        if is_latin(c) {
            run.push(c);
            continue;
        }
        if !run.is_empty() {
            out.extend(run.nfd());
            run.clear();
        }
        out.push(c);
    }
    out.extend(run.nfd());
    out.chars().filter(|c| !(0x300..=0x36F).contains(&(*c as u32))).collect()
}

fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

struct LowerInfo {
    codes: Vec<u16>,
    bitflags: u32,
    contains_space: bool,
    lower: Vec<u16>,
}

fn lower_info(s: &str) -> LowerInfo {
    let s = remove_accents(s);
    let lower = utf16(&s.to_lowercase());
    let mut bitflags = 0u32;
    let mut contains_space = false;
    for &c in &lower {
        if c == 32 {
            contains_space = true;
            continue;
        }
        let bit = if (97..=122).contains(&c) {
            c as u32 - 97
        } else if (48..=57).contains(&c) {
            26
        } else if c <= 127 {
            30
        } else {
            31
        };
        bitflags |= 1 << bit;
    }
    LowerInfo { codes: lower.clone(), bitflags, contains_space, lower }
}

struct Prepared {
    lower: Vec<u16>,
    bitflags: u32,
    next_beginning: Vec<usize>,
}

fn beginning_indexes(target: &[u16]) -> Vec<usize> {
    let mut out = Vec::new();
    let (mut was_upper, mut was_alnum) = (false, false);
    for (i, &c) in target.iter().enumerate() {
        let upper = (65..=90).contains(&c);
        let alnum = upper || (97..=122).contains(&c) || (48..=57).contains(&c);
        if (upper && !was_upper) || !was_alnum || !alnum {
            out.push(i);
        }
        was_upper = upper;
        was_alnum = alnum;
    }
    out
}

fn next_beginning_indexes(target: &str) -> Vec<usize> {
    let t = utf16(&remove_accents(target));
    let begin = beginning_indexes(&t);
    let len = t.len();
    let mut out = Vec::with_capacity(len);
    let mut last = begin.first().copied();
    let mut li = 0;
    for i in 0..len {
        match last {
            Some(l) if l > i => out.push(l),
            _ => {
                li += 1;
                last = begin.get(li).copied();
                out.push(last.unwrap_or(len));
            }
        }
    }
    out
}

fn prepare(target: &str) -> Prepared {
    let info = lower_info(target);
    Prepared { lower: info.lower, bitflags: info.bitflags, next_beginning: next_beginning_indexes(target) }
}

struct Search {
    codes: Vec<u16>,
    lower: Vec<u16>,
    contains_space: bool,
    bitflags: u32,
    spaces: Vec<Search>,
}

fn prepare_search(search: &str) -> Search {
    let search = search.trim();
    let info = lower_info(search);
    let mut spaces = Vec::new();
    if info.contains_space {
        let mut seen: Vec<&str> = Vec::new();
        for part in search.split(char::is_whitespace) {
            if seen.contains(&part) {
                continue;
            }
            seen.push(part);
            if part.is_empty() {
                continue;
            }
            let i = lower_info(part);
            spaces.push(Search { codes: i.codes, lower: utf16(&part.to_lowercase()), contains_space: false, bitflags: 0, spaces: vec![] });
        }
    }
    Search { codes: info.codes, lower: info.lower, contains_space: info.contains_space, bitflags: info.bitflags, spaces }
}

#[derive(Clone, Debug)]
struct Hit {
    score: f64,
    indexes: Vec<usize>,
}

fn index_of(hay: &[u16], needle: &[u16], from: usize) -> Option<usize> {
    if needle.is_empty() {
        return Some(from.min(hay.len()));
    }
    (from..=hay.len().saturating_sub(needle.len())).find(|&i| i + needle.len() <= hay.len() && hay[i..i + needle.len()] == *needle)
}

fn algorithm(search: &Search, p: &mut Prepared, allow_spaces: bool, partial: bool, partial_scores: &mut Vec<f64>) -> Option<Hit> {
    if !allow_spaces && search.contains_space {
        return algorithm_spaces(search, p, partial, partial_scores);
    }
    let sc = &search.codes;
    let tc = &p.lower;
    let slen = sc.len();
    let tlen = tc.len();
    if slen == 0 {
        return None;
    }
    let mut simple: Vec<usize> = Vec::with_capacity(slen);
    let mut si = 0;
    let mut ti = 0;
    loop {
        if ti >= tlen {
            return None;
        }
        if sc[si] == tc[ti] {
            simple.push(ti);
            si += 1;
            if si == slen {
                break;
            }
        }
        ti += 1;
        if ti >= tlen {
            return None;
        }
    }
    let next = p.next_beginning.clone();
    let mut si = 0;
    let mut strict: Vec<usize> = Vec::new();
    let mut success_strict = false;
    let mut ti = if simple[0] == 0 { 0 } else { next[simple[0] - 1] };
    let mut backtrack = 0;
    if ti != tlen {
        loop {
            if ti >= tlen {
                if si == 0 {
                    break;
                }
                backtrack += 1;
                if backtrack > 200 {
                    break;
                }
                si -= 1;
                let last = strict.pop().unwrap();
                ti = next[last];
            } else if sc[si] == tc[ti] {
                strict.push(ti);
                si += 1;
                if si == slen {
                    success_strict = true;
                    break;
                }
                ti += 1;
            } else {
                ti = next[ti];
            }
        }
    }
    let mut substring_index = if slen <= 1 { None } else { index_of(&p.lower, &search.lower, simple[0]) };
    let is_substring = substring_index.is_some();
    let mut is_substring_beginning = substring_index.is_some_and(|s| s == 0 || next[s - 1] == s);
    if is_substring && !is_substring_beginning {
        let si0 = substring_index.unwrap();
        let mut i = 0;
        while i < next.len() {
            if i > si0 {
                let mut s = 0;
                while s < slen && i + s < tlen && sc[s] == tc[i + s] {
                    s += 1;
                }
                if s == slen {
                    substring_index = Some(i);
                    is_substring_beginning = true;
                    break;
                }
            }
            i = next[i];
        }
    }
    let calc = |m: &[usize]| -> f64 {
        let mut score = 0.0;
        let mut groups = 0.0;
        for i in 1..slen {
            if m[i] as i64 - m[i - 1] as i64 != 1 {
                score -= m[i] as f64;
                groups += 1.0;
            }
        }
        let unmatched = m[slen - 1] as f64 - m[0] as f64 - (slen as f64 - 1.0);
        score -= (12.0 + unmatched) * groups;
        if m[0] != 0 {
            score -= (m[0] * m[0]) as f64 * 0.2;
        }
        if !success_strict {
            score *= 1000.0;
        } else {
            let mut unique = 1;
            let mut i = next.first().copied().unwrap_or(tlen);
            while i < tlen {
                unique += 1;
                i = next[i];
            }
            if unique > 24 {
                score *= (unique - 24) as f64 * 10.0;
            }
        }
        score -= (tlen as f64 - slen as f64) / 2.0;
        if is_substring {
            score /= 1.0 + (slen * slen) as f64;
        }
        if is_substring_beginning {
            score /= 1.0 + (slen * slen) as f64;
        }
        score -= (tlen as f64 - slen as f64) / 2.0;
        score
    };
    let (best, score) = if !success_strict {
        if let Some(s) = substring_index {
            for (i, m) in simple.iter_mut().enumerate() {
                *m = s + i;
            }
        }
        let sc = calc(&simple);
        (simple, sc)
    } else if is_substring_beginning {
        let s = substring_index.unwrap();
        for (i, m) in simple.iter_mut().enumerate() {
            *m = s + i;
        }
        let sc = calc(&simple);
        (simple, sc)
    } else {
        let sc = calc(&strict);
        (strict, sc)
    };
    Some(Hit { score, indexes: best })
}

fn algorithm_spaces(search: &Search, p: &mut Prepared, partial: bool, partial_scores: &mut Vec<f64>) -> Option<Hit> {
    let mut seen: Vec<usize> = Vec::new();
    let mut score = 0.0;
    let mut first_seen_last = 0usize;
    let n = search.spaces.len();
    let original = p.next_beginning.clone();
    let mut any = false;
    partial_scores.clear();
    partial_scores.resize(n, NEG_INF);
    let mut last: Option<Hit> = None;
    for (i, s) in search.spaces.iter().enumerate() {
        let mut dummy = Vec::new();
        let result = algorithm(s, p, false, false, &mut dummy);
        let Some(result) = result else {
            if partial {
                continue;
            }
            p.next_beginning = original;
            return None;
        };
        any = true;
        if i != n - 1 {
            let idx = &result.indexes;
            let consecutive = idx.windows(2).all(|w| w[1] - w[0] == 1);
            if consecutive {
                let new_begin = idx[idx.len() - 1] + 1;
                let to_replace = p.next_beginning[new_begin - 1];
                let mut j = new_begin as isize - 1;
                while j >= 0 {
                    if p.next_beginning[j as usize] != to_replace {
                        break;
                    }
                    p.next_beginning[j as usize] = new_begin;
                    j -= 1;
                }
            }
        }
        score += result.score / n as f64;
        partial_scores[i] = result.score / n as f64;
        if result.indexes[0] < first_seen_last {
            score -= (first_seen_last - result.indexes[0]) as f64 * 2.0;
        }
        first_seen_last = result.indexes[0];
        for &j in &result.indexes {
            if !seen.contains(&j) {
                seen.push(j);
            }
        }
        last = Some(result);
    }
    if partial && !any {
        return None;
    }
    p.next_beginning = original;
    let mut dummy = Vec::new();
    if let Some(r) = algorithm(search, p, true, false, &mut dummy)
        && r.score > score
    {
        if partial {
            for s in partial_scores.iter_mut() {
                *s = r.score / n as f64;
            }
        }
        return Some(r);
    }
    let _ = last;
    Some(Hit { score, indexes: seen })
}

/// `fastpriorityqueue` (min-heap on score) as fuzzysort uses it.
struct Queue {
    e: Vec<(f64, usize)>,
}

impl Queue {
    fn add(&mut self, r: (f64, usize)) {
        let mut a = self.e.len();
        self.e.push(r);
        while a > 0 {
            let v = (a - 1) >> 1;
            if r.0 < self.e[v].0 {
                self.e[a] = self.e[v];
                a = v;
            } else {
                break;
            }
        }
        self.e[a] = r;
    }

    fn poll(&mut self) -> Option<(f64, usize)> {
        if self.e.is_empty() {
            return None;
        }
        let top = self.e[0];
        let last = self.e.pop().unwrap();
        let o = self.e.len();
        if o > 0 {
            self.e[0] = last;
            // `v()` (percolate down) exactly as written in the minified source.
            let mut a = 0;
            let v = self.e[0];
            let mut c = 1;
            while c < o {
                let s = c + 1;
                a = c;
                if s < o && self.e[s].0 < self.e[c].0 {
                    a = s;
                }
                self.e[(a - 1) >> 1] = self.e[a];
                c = 1 + (a << 1);
            }
            let mut f = if a == 0 { 0 } else { (a - 1) >> 1 };
            while a > 0 && v.0 < self.e[f].0 {
                self.e[a] = self.e[f];
                a = f;
                f = if a == 0 { 0 } else { (a - 1) >> 1 };
            }
            self.e[a] = v;
        }
        Some(top)
    }
}

/// `fuzzysort.go(search, targets, { keys })`: the indexes of the matching targets, best first.
/// `keys(target)` returns each key's text (`None` for a missing key).
pub fn go<T>(search: &str, targets: &[T], keys: impl Fn(&T) -> Vec<Option<String>>) -> Vec<usize> {
    go_scored(search, targets, keys).into_iter().map(|(i, _)| i).collect()
}

/// [`go`] with the raw (`_score`) scores.
pub fn go_scored<T>(search: &str, targets: &[T], keys: impl Fn(&T) -> Vec<Option<String>>) -> Vec<(usize, f64)> {
    if search.is_empty() {
        return Vec::new();
    }
    let s = prepare_search(search);
    let mut q = Queue { e: Vec::new() };
    let mut partial_scores: Vec<f64> = Vec::new();
    for (i, t) in targets.iter().enumerate() {
        let ks = keys(t);
        let mut prepared: Vec<Option<Prepared>> = ks.iter().map(|k| k.as_deref().filter(|k| !k.is_empty()).map(prepare)).collect();
        let flags = prepared.iter().flatten().fold(0, |a, p| a | p.bitflags);
        if s.bitflags & flags != s.bitflags {
            continue;
        }
        let n_spaces = s.spaces.len();
        let mut best_spaces = vec![NEG_INF; n_spaces];
        let mut results: Vec<f64> = Vec::new();
        for p in prepared.iter_mut() {
            let Some(p) = p else {
                results.push(NEG_INF);
                continue;
            };
            let r = algorithm(&s, p, false, s.contains_space, &mut partial_scores);
            results.push(r.as_ref().map(|r| r.score).unwrap_or(NEG_INF));
            if s.contains_space && r.is_some() {
                for (k, best) in best_spaces.iter_mut().enumerate() {
                    let ps = partial_scores.get(k).copied().unwrap_or(NEG_INF);
                    if ps > -1000.0 && *best > NEG_INF {
                        let tmp = (*best + ps) / 4.0;
                        if tmp > *best {
                            *best = tmp;
                        }
                    }
                    if ps > *best {
                        *best = ps;
                    }
                }
            }
        }
        let score = if s.contains_space {
            if best_spaces.contains(&NEG_INF) {
                continue;
            }
            best_spaces.iter().sum()
        } else {
            if !results.iter().any(|r| *r != NEG_INF) {
                continue;
            }
            let mut score = NEG_INF;
            for r in &results {
                if *r > -1000.0 && score > NEG_INF {
                    let tmp = (score + r) / 4.0;
                    if tmp > score {
                        score = tmp;
                    }
                }
                if *r > score {
                    score = *r;
                }
            }
            score
        };
        q.add((score, i));
    }
    let n = q.e.len();
    let mut out = vec![(0, 0.0); n];
    for k in (0..n).rev() {
        let (score, i) = q.poll().unwrap();
        out[k] = (i, score);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Orders and `_score`s from fuzzysort 3.1.0.
    #[test]
    fn matches_fuzzysort() {
        type Case<'a> = (&'a str, Vec<Vec<&'a str>>, Vec<usize>, Vec<f64>);
        let cases: Vec<Case> = vec![
            ("e", vec![vec!["compact"], vec!["new"], vec!["editor"], vec!["exit"], vec!["settings"], vec!["quit"], vec!["q"], vec!["review"]], vec![3, 2, 1, 7, 4], vec![-3.0, -5.0, -202.0, -205.0, -207.0]),
            ("cmp", vec![vec!["compact"], vec!["new"], vec!["editor"]], vec![0], vec![-15004.0]),
            ("se", vec![vec!["/settings", "settings", "configure Mini transcript output"], vec!["/session", "session", "x"], vec!["/exit", "exit", "close OpenCode"], vec!["/review", "review", "review changes"], vec!["/new", "new", "start a new session"]], vec![1, 0, 4, 2], vec![-1.432, -1.692, -9.992, -367.2]),
            ("re", vec![vec!["review", "/review", "Review changes [commit|branch|pr], defaults to uncommitted"], vec!["init", "/init", "create/update AGENTS.md"], vec!["/compact", "compact", "compact older session context to free space"]], vec![0, 1, 2], vec![-1.172, -52.6, -46264.6]),
            ("str berry", vec![vec!["strawberry"], vec!["straw berry"], vec!["berry str"], vec!["blueberry"]], vec![2, 1, 0], vec![-2.5524792899408286, -3.5275443786982246, -99.21942307692308]),
            ("ab", vec![vec!["a b"], vec!["ab"], vec!["xab"], vec!["a_b"], vec!["AxxB"], vec!["aaa bbb"]], vec![1, 0, 3, 4, 5, 2], vec![0.0, -16.0, -16.0, -19.0, -24.0, -40.6]),
            ("ts", vec![vec!["src/a.ts"], vec!["src/b.tsx"], vec!["tests/x"], vec!["TsConfig"], vec!["notes.md"]], vec![3, 0, 1, 2, 4], vec![-3.12, -3.408, -3.928, -363.0, -17806.0]),
            ("hello", vec![vec!["hello.txt"], vec!["notes.md"], vec!["Hello World"], vec!["he llo"]], vec![0, 2, 3], vec![-2.002958579881657, -3.0044378698224854, -17.0]),
        ];
        for (search, rows, order, scores) in cases {
            let got = go_scored(search, &rows, |r| r.iter().map(|k| Some(k.to_string())).collect());
            assert_eq!(got.iter().map(|g| g.0).collect::<Vec<_>>(), order, "{search}");
            for (g, w) in got.iter().zip(&scores) {
                assert!((g.1 - w).abs() < 1e-6, "{search}: {} vs {w}", g.1);
            }
        }
    }
}
