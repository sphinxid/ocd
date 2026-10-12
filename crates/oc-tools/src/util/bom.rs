//! `util/src/bom.ts`.

const BOM: char = '\u{FEFF}';

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Split {
    pub bom: bool,
    pub text: String,
}

/// Strip all leading U+FEFF.
pub fn split(text: &str) -> Split {
    let stripped = text.trim_start_matches(BOM);
    Split { bom: stripped.len() != text.len(), text: stripped.to_string() }
}

pub fn join(text: &str, bom: bool) -> String {
    let stripped = text.trim_start_matches(BOM);
    if bom { format!("{BOM}{stripped}") } else { stripped.to_string() }
}

pub fn has(content: &[u8]) -> bool {
    content.len() >= 3 && content[0] == 0xef && content[1] == 0xbb && content[2] == 0xbf
}

/// `TextDecoder("utf-8", {ignoreBOM: true})`: lossy decode keeping the BOM.
pub fn decode(content: &[u8]) -> String {
    String::from_utf8_lossy(content).into_owned()
}

pub fn decode_bytes(content: &[u8]) -> Split {
    split(&decode(content))
}

/// Returns the BOM-less text and, when the canonical form differs, the bytes to write.
pub fn sync_bytes(content: &[u8], bom: bool) -> (String, Option<Vec<u8>>) {
    let decoded = decode(content);
    let current = split(&decoded);
    let canonical = if bom { format!("{BOM}{}", current.text) } else { current.text.clone() };
    let bytes = if decoded == canonical { None } else { Some(canonical.into_bytes()) };
    (current.text, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_join() {
        assert_eq!(split("\u{FEFF}\u{FEFF}a"), Split { bom: true, text: "a".into() });
        assert_eq!(join("\u{FEFF}a", true), "\u{FEFF}a");
        assert_eq!(join("\u{FEFF}a", false), "a");
        assert!(has(&[0xef, 0xbb, 0xbf, b'a']));
        let (text, bytes) = sync_bytes("a".as_bytes(), true);
        assert_eq!(text, "a");
        assert_eq!(bytes.unwrap(), "\u{FEFF}a".as_bytes());
        assert!(sync_bytes("\u{FEFF}a".as_bytes(), true).1.is_none());
    }
}
