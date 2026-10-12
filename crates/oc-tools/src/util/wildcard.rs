//! `core/src/util/wildcard.ts`.

/// `Wildcard.match(input, pattern)`.
pub fn matches(input: &str, pattern: &str) -> bool {
    let normalized = input.replace('\\', "/");
    let pattern = pattern.replace('\\', "/");
    let mut escaped = String::with_capacity(pattern.len() * 2);
    for c in pattern.chars() {
        match c {
            '.' | '+' | '^' | '$' | '{' | '}' | '(' | ')' | '|' | '[' | ']' | '\\' => {
                escaped.push('\\');
                escaped.push(c);
            }
            '*' => escaped.push_str(".*"),
            '?' => escaped.push('.'),
            _ => escaped.push_str(&regex::escape(&c.to_string())),
        }
    }
    if escaped.ends_with(" .*") {
        escaped.truncate(escaped.len() - 3);
        escaped.push_str("( .*)?");
    }
    let flags = if cfg!(windows) { "(?si)" } else { "(?s)" };
    match regex::Regex::new(&format!("{flags}^{escaped}$")) {
        Ok(re) => re.is_match(&normalized),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::matches;

    #[test]
    fn basic() {
        assert!(matches("edit", "edit"));
        assert!(matches("anything", "*"));
        assert!(matches("git status", "git *"));
        assert!(matches("git", "git *"));
        assert!(!matches("gitx", "git *"));
        assert!(matches("a.b", "a.b"));
        assert!(!matches("axb", "a.b"));
        assert!(matches("abc", "a?c"));
        assert!(matches("opencode_session_rename", "opencode_*"));
        assert!(matches("line\nbreak", "line*"));
    }
}
