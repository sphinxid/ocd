//! ASCII ("mono") presentation helpers (`src/mini/mono.ts`).

fn prefix(c: char) -> Option<&'static str> {
    Some(match c as u32 {
        0x2192 => "->",
        0x2190 => "<-",
        0x2731 => "*",
        0x2699 => "*",
        0x2716 => "!",
        0x2717 => "!",
        0x25c8 => "*",
        0x25c9 => "*",
        0x27f3 => "*",
        _ => return None,
    })
}

/// The `markdown` substitution table (prefixes plus typographic characters).
pub fn markdown_sub(c: char) -> Option<&'static str> {
    if let Some(p) = prefix(c) {
        return Some(p);
    }
    Some(match c as u32 {
        0x00a0 => " ",
        0x00b7 => "-",
        0x2010..=0x2013 => "-",
        0x2014 => "--",
        0x2018 | 0x2019 => "'",
        0x201c | 0x201d => "\"",
        0x2022 => "*",
        0x2026 => "...",
        0x2191 => "up",
        0x2193 => "down",
        _ => return None,
    })
}

/// `monoText`: non-printable-ASCII characters (except tab/newline) become their substitution or `?`.
pub fn mono_text(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        if c == '\t' || c == '\n' || (' '..='~').contains(&c) {
            out.push(c);
        } else {
            out.push_str(markdown_sub(c).unwrap_or("?"));
        }
    }
    out
}

/// `monoCell(point)`: the single ASCII character a snapshot cell becomes.
pub fn mono_cell(c: char) -> char {
    match c as u32 {
        0x2500 => '-',
        0x2502 => '|',
        _ => markdown_sub(c).and_then(|s| s.chars().next()).unwrap_or('?'),
    }
}

/// `monoPrefix(value, mono)`.
pub fn mono_prefix(value: &str, mono: bool) -> String {
    if !mono {
        return value.to_string();
    }
    let Some(first) = value.chars().next() else { return value.to_string() };
    match prefix(first) {
        Some(p) => format!("{p}{}", &value[first.len_utf8()..]),
        None => value.to_string(),
    }
}

/// `monoToolText(value, mono)`.
pub fn mono_tool_text(value: &str, mono: bool) -> String {
    let result = mono_prefix(value, mono);
    if !mono {
        return result;
    }
    let separator = " \u{b7} ";
    let Some(index) = result.rfind(separator) else { return result };
    let head = &result[..index];
    let questions = regex::Regex::new(r"^\d+ questions$").unwrap();
    if !head.contains(" completed") && head != "patch" && !questions.is_match(head) {
        return result;
    }
    format!("{head} - {}", &result[index + separator.len()..])
}

/// `monoShortcut(value, mono)`.
pub fn mono_shortcut(value: &str, mono: bool) -> String {
    if !mono {
        return value.to_string();
    }
    value.replace('\u{2192}', "right").replace('\u{2190}', "left").replace('\u{2191}', "up").replace('\u{2193}', "down")
}

/// `monoTruncate(value, width, mono)` (UTF-16 length like upstream).
pub fn mono_truncate(value: &str, width: usize, mono: bool) -> String {
    let len = value.encode_utf16().count();
    if !mono || len <= width {
        return value.to_string();
    }
    if width <= 3 {
        return ".".repeat(width);
    }
    let units: Vec<u16> = value.encode_utf16().take(width - 3).collect();
    String::from_utf16_lossy(&units) + "..."
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes() {
        assert_eq!(mono_prefix("→ Read a", true), "-> Read a");
        assert_eq!(mono_prefix("✖ shell failed", true), "! shell failed");
        assert_eq!(mono_prefix("→ Read a", false), "→ Read a");
        assert_eq!(mono_tool_text("shell completed · 1.0s", true), "shell completed - 1.0s");
        assert_eq!(mono_tool_text("2 questions · 1.0s", true), "2 questions - 1.0s");
        assert_eq!(mono_tool_text("Thing · x", true), "Thing · x");
        assert_eq!(mono_text("a·b…"), "a-b...");
        assert_eq!(mono_truncate("abcdefgh", 6, true), "abc...");
    }
}
