//! `core/src/util/wildcard.ts`

pub fn to_regex(pattern: &str) -> regex::Regex {
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
    let flags = if cfg!(windows) { "(?si)" } else { "(?s)" };
    regex::Regex::new(&format!("{flags}^{escaped}$")).unwrap_or_else(|_| regex::Regex::new("$^").unwrap())
}

pub fn matches(input: &str, pattern: &str) -> bool {
    to_regex(pattern).is_match(&input.replace('\\', "/"))
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
