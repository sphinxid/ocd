//! `core/src/util/wildcard.ts`

pub fn to_regex(pattern: &str) -> regex::Regex {
    to_regex_case(pattern, false)
}

/// [`to_regex`] that also ignores case when `ignore_case` (Windows always does).
pub fn to_regex_case(pattern: &str, ignore_case: bool) -> regex::Regex {
    let mut escaped = String::with_capacity(pattern.len() * 2);
    for ch in pattern.replace('\\', "/").chars() {
        match ch {
            '.' | '+' | '^' | '$' | '{' | '}' | '(' | ')' | '|' | '[' | ']' | '\\' => {
                escaped.push('\\');
                escaped.push(ch);
            }
            '*' => escaped.push_str(".*"),
            '?' => escaped.push('.'),
            c => escaped.push_str(&regex::escape(&c.to_string())),
        }
    }
    if let Some(stripped) = escaped.strip_suffix(" .*") {
        escaped = format!("{stripped}( .*)?");
    }
    let flags = if cfg!(windows) || ignore_case { "(?si)" } else { "(?s)" };
    regex::Regex::new(&format!("{flags}^{escaped}$")).unwrap_or_else(|_| regex::Regex::new("$^").unwrap())
}

pub fn matches(input: &str, pattern: &str) -> bool {
    matches_case(input, pattern, false)
}

/// `Wildcard.match(input, pattern, ignoreCase)`: Windows always matches without case; `ignore_case` extends that to
/// other callers, such as file rules on macOS (2.4.1).
pub fn matches_case(input: &str, pattern: &str, ignore_case: bool) -> bool {
    to_regex_case(pattern, ignore_case).is_match(&input.replace('\\', "/"))
}

#[cfg(test)]
mod tests {
    use super::matches;
    #[test]
    fn basics() {
        assert!(matches("read", "*"));
        assert!(matches("/a/b.env", "*.env"));
        assert!(!matches("/a/b.env.example", "*.env"));
        assert!(matches("git status", "git *"));
        assert!(matches("git", "git *"));
        assert!(matches("a.b", "a?b"));
        assert!(!matches("axb", "a.b"));
    }
}
