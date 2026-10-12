//! Fuzzy path ranking for `fs.find` — an approximation of `fuzzysort.go` (upstream's ripgrep
//! index) / fff ranking: every query term must match as a case-insensitive subsequence; matches
//! that are contiguous, start at word boundaries or fall in the basename rank higher, and shorter
//! targets win ties.

fn boundary(chars: &[char], i: usize) -> bool {
    if i == 0 {
        return true;
    }
    let prev = chars[i - 1];
    let cur = chars[i];
    matches!(prev, '/' | '\\' | '.' | '_' | '-' | ' ') || (prev.is_lowercase() && cur.is_uppercase()) || (!prev.is_ascii_digit() && cur.is_ascii_digit())
}

fn term_score(term: &[char], target: &[char], lower: &[char]) -> Option<f64> {
    if term.is_empty() {
        return Some(0.0);
    }
    // Contiguous occurrence (best: at a boundary, inside the basename).
    let base_start = target.iter().rposition(|c| *c == '/' || *c == '\\').map(|p| p + 1).unwrap_or(0);
    let trimmed_end = if target.last().is_some_and(|c| *c == '/' || *c == '\\') { target.len() - 1 } else { target.len() };
    let base_start = if base_start >= trimmed_end {
        target[..trimmed_end].iter().rposition(|c| *c == '/' || *c == '\\').map(|p| p + 1).unwrap_or(0)
    } else {
        base_start
    };
    let mut best: Option<f64> = None;
    if term.len() <= lower.len() {
        for start in 0..=lower.len() - term.len() {
            if lower[start..start + term.len()] == *term {
                let mut s = 1000.0;
                if boundary(target, start) {
                    s += 200.0;
                }
                if start >= base_start {
                    s += 300.0;
                    if start == base_start {
                        s += 200.0;
                    }
                }
                s -= start as f64 * 0.5;
                best = Some(best.map_or(s, |b: f64| b.max(s)));
            }
        }
    }
    if best.is_some() {
        return best;
    }
    // Subsequence: prefer boundary positions greedily, fall back to the next occurrence.
    let mut s = 0.0;
    let mut pos = 0usize;
    let mut prev: Option<usize> = None;
    for &c in term {
        let next_boundary = (pos..lower.len()).find(|&i| lower[i] == c && boundary(target, i));
        let next_any = (pos..lower.len()).find(|&i| lower[i] == c)?;
        let i = match (prev, next_boundary) {
            (Some(p), _) if next_any == p + 1 => next_any,
            (_, Some(b)) => b,
            _ => next_any,
        };
        s += 10.0;
        if prev.is_some_and(|p| i == p + 1) {
            s += 15.0;
        } else if let Some(p) = prev {
            s -= ((i - p) as f64).min(20.0);
        }
        if boundary(target, i) {
            s += 12.0;
        }
        if i >= base_start {
            s += 5.0;
        }
        prev = Some(i);
        pos = i + 1;
    }
    Some(s)
}

/// Score `target` against `query`; `None` when it does not match. Empty queries match nothing
/// (like `fuzzysort.go` without `all`).
pub fn score(query: &str, target: &str) -> Option<f64> {
    let terms: Vec<Vec<char>> = query.split_whitespace().map(|t| t.to_lowercase().chars().collect()).collect();
    if terms.is_empty() {
        return None;
    }
    let chars: Vec<char> = target.chars().collect();
    let lower: Vec<char> = target.to_lowercase().chars().collect();
    if lower.len() != chars.len() {
        return None;
    }
    let mut total = 0.0;
    for t in &terms {
        total += term_score(t, &chars, &lower)?;
    }
    Some(total - chars.len() as f64 * 0.1)
}

#[cfg(test)]
mod tests {
    use super::score;

    #[test]
    fn subsequence_required() {
        assert!(score("abc", "a/b/c.txt").is_some());
        assert!(score("abd", "a/b/c.txt").is_none());
        assert!(score("", "anything").is_none());
    }

    #[test]
    fn contiguous_basename_wins() {
        let a = score("main", "src/main.rs").unwrap();
        let b = score("main", "src/m/a/i/n.rs").unwrap();
        let c = score("main", "domain/x.rs").unwrap();
        assert!(a > b);
        assert!(a > c);
    }

    #[test]
    fn shorter_wins_ties() {
        assert!(score("lib", "lib.rs").unwrap() > score("lib", "lib.rs.bak").unwrap());
    }

    #[test]
    fn terms_all_match() {
        assert!(score("src main", "src/main.rs").is_some());
        assert!(score("src zzz", "src/main.rs").is_none());
    }
}
