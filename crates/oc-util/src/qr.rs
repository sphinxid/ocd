//! QR codes as upstream draws them in a terminal: `renderUnicodeCompact` of `@opencode/util/qr` (2.2), which
//! encodes the text's UTF-8 bytes as one byte-mode segment following Nayuki's QR Code generator (the algorithm of
//! the `qrcodegen` crate): error correction level L without boosting, the smallest version that fits, and the mask
//! with the lowest penalty.

use qrcodegen::{QrCode, QrCodeEcc, QrSegment, Version};

/// `renderUnicodeCompact(text, { border })`: two rows per line, light modules drawn (`█`, `▀`, `▄`) and dark ones
/// left blank, so the code reads on a dark terminal. Rows past the bottom count as dark. Empty if the text does not
/// fit in a QR code.
pub fn render_unicode_compact(text: &str, border: i32) -> String {
    let segments = [QrSegment::make_bytes(text.as_bytes())];
    let Ok(qr) = QrCode::encode_segments_advanced(&segments, QrCodeEcc::Low, Version::MIN, Version::MAX, None, false)
    else {
        return String::new();
    };
    let size = qr.size() + border * 2;
    let dark = |x: i32, y: i32| -> bool {
        if x < 0 || y < 0 || x >= size || y >= size {
            return true;
        }
        qr.get_module(x - border, y - border)
    };
    let mut lines = Vec::new();
    let mut y = 0;
    while y < size {
        let line: String = (0..size)
            .map(|x| match (dark(x, y), dark(x, y + 1)) {
                (false, false) => '█',
                (false, true) => '▀',
                (true, false) => '▄',
                (true, true) => ' ',
            })
            .collect();
        lines.push(line);
        y += 2;
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `renderUnicodeCompact(text, { border: 2 })` of 2.2.2's `@opencode/util/qr` under Bun 1.4.4: numeric and
    /// alphanumeric texts encode as bytes too.
    #[test]
    fn matches_opencode_util_qr() {
        let cases: Vec<(String, String)> = serde_json::from_str(include_str!("qr_testdata.json")).unwrap();
        for (text, expected) in cases {
            assert_eq!(render_unicode_compact(&text, 2), expected, "{text}");
        }
    }

    /// `renderUnicodeCompact("https://example.com", { border: 1 })` from uqr 0.1.3 under Bun.
    #[test]
    fn matches_uqr() {
        let expected = [
            "█▀▀▀▀▀▀▀████▀▀█▀███▀▀▀▀▀▀▀█",
            "█ █▀▀▀█ ██▄▄█▀▄ █▄█ █▀▀▀█ █",
            "█ █   █ █ ▀██▄ ██ █ █   █ █",
            "█ ▀▀▀▀▀ █▀█▀▄ █ █▀█ ▀▀▀▀▀ █",
            "█▀▀███▀▀▀█▀▄ ██▄▄▄███▀▀████",
            "█ █▀▀▄█▀▄ ▄▀█▄    ▄█ ▄ ▄ ▀█",
            "█▀  ██ ▀▀▀ █▀  █▀ █  █▄██ █",
            "█ ▄█▀▀▀▀ ▄█▀▀▀██ ▄█▀ ▀▀█ ▀█",
            "█ █▄▄ ▀▀ ▀█▀ ████▀   ▀▄ █▄█",
            "█▀▀▀▀▀▀▀█ ██▀▄▄▄█ █▀█ ███▀█",
            "█ █▀▀▀█ █▄  █ ▄▀  ▀▀▀ █▀▄▀█",
            "█ █   █ ██▀▄▀▀█▄▀▄▄▄██▀▀▄ █",
            "█ ▀▀▀▀▀ █ ▄▀▄█▀ ▄ ▄▀▄▄▀██ █",
            "▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀",
        ];
        assert_eq!(render_unicode_compact("https://example.com", 1), expected.join("\n"));
    }
}
