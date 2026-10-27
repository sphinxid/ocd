//! QR codes as upstream draws them in a terminal: `uqr` 0.1.3's `renderUnicodeCompact`, which encodes with
//! Nayuki's QR Code generator (the same algorithm as the `qrcodegen` crate): the shortest single segment
//! (numeric, alphanumeric or bytes), error correction level L without boosting, the smallest version that fits, and
//! the mask with the lowest penalty.

use qrcodegen::{QrCode, QrCodeEcc, QrSegment, Version};

/// `renderUnicodeCompact(text, { border })`: two rows per line, light modules drawn (`█`, `▀`, `▄`) and dark ones
/// left blank, so the code reads on a dark terminal. Rows past the bottom count as dark. Empty if the text does not
/// fit in a QR code.
pub fn render_unicode_compact(text: &str, border: i32) -> String {
    let segments = QrSegment::make_segments(text);
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
