//! `String.prototype.localeCompare` approximation (ICU root collation, the default Bun/V8 locale).
//!
//! Primary weights follow the CLDR root order: whitespace, then punctuation and symbols in DUCET order
//! (`_ - , ; : ! ? . ' " ( ) [ ] { } @ * / \ & # % ` ^ + < = > | ~ $`), then digits, then letters
//! compared case- and accent-insensitively. Ties break on accents, then lowercase before uppercase, then
//! code points. Upstream sorts integrations, skills, references and MCP servers this way.

use std::cmp::Ordering;

use unicode_normalization::UnicodeNormalization;

const PUNCTUATION: &str = "_-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$";

fn base_char(c: char) -> char {
    let first = std::iter::once(c).nfd().next().unwrap_or(c);
    first.to_lowercase().next().unwrap_or(first)
}

/// (class, rank within class, base char): 0 whitespace, 1 punctuation/symbols, 2 digits, 3 letters.
fn primary(c: char) -> (u8, u32, char) {
    if c.is_whitespace() {
        return (0, c as u32, c);
    }
    if let Some(i) = PUNCTUATION.find(c) {
        return (1, i as u32, c);
    }
    if c.is_numeric() {
        return (2, c.to_digit(10).unwrap_or(c as u32), c);
    }
    if c.is_alphanumeric() {
        let b = base_char(c);
        return (3, b as u32, b);
    }
    // Other symbols: after the known punctuation, by code point.
    (1, 0x100 + c as u32, c)
}

pub fn locale_compare(a: &str, b: &str) -> Ordering {
    let pa: Vec<_> = a.chars().map(primary).collect();
    let pb: Vec<_> = b.chars().map(primary).collect();
    match pa.cmp(&pb) {
        Ordering::Equal => {}
        other => return other,
    }
    let secondary = |s: &str| -> String { s.to_lowercase().nfd().collect() };
    match secondary(a).cmp(&secondary(b)) {
        Ordering::Equal => {}
        other => return other,
    }
    let tertiary = |s: &str| -> Vec<u8> { s.chars().map(|c| u8::from(c.is_uppercase())).collect() };
    match tertiary(a).cmp(&tertiary(b)) {
        Ordering::Equal => a.cmp(b),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order() {
        let mut items = vec!["b", "A", "a", "_x", "1", "B", "é", "e"];
        items.sort_by(|a, b| locale_compare(a, b));
        assert_eq!(items, vec!["_x", "1", "a", "A", "b", "B", "e", "é"]);
        // Upstream integration list order (sorted by name).
        let mut names = vec!["AI21 Labs", "ai&", "AI-ROUTER", "Agnes AI"];
        names.sort_by(|a, b| locale_compare(a, b));
        assert_eq!(names, vec!["Agnes AI", "AI-ROUTER", "ai&", "AI21 Labs"]);
    }
}
