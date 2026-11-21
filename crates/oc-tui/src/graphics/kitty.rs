//! Kitty graphics protocol escapes (OpenTUI `terminal-image.zig`, raw transport).
//!
//! Images are sent once per placement id (`a=t`, quiet `q=2`): a PNG as it is (`f=100`), anything else as RGB
//! (`f=24`) or RGBA (`f=32`) pixels, in chunks of 3072 bytes (4096 base64 characters, `m=1` on all but the last).
//! A placement (`a=p`) puts the image at a cell with its size in cells (`c`, `r`), the shown source rectangle in
//! pixels (`x`, `y`, `w`, `h`), no cursor movement (`C=1`) and a z-index under text but above cell backgrounds.
//! Inside tmux every escape is wrapped in a passthrough DCS with its ESCs doubled.

use std::fmt::Write as _;

use base64::Engine as _;

use super::decode::Picture;

const RAW_CHUNK: usize = 3072;
/// `gp.IMAGE_ID_MASK`: placement ids stay below this.
pub const IMAGE_ID_MASK: u32 = 0x03FF_FFFF;
/// Run placements (parts of an image a dialog does not cover) are numbered from here.
pub const RUN_ID_MIN: u32 = 0x8000_0000;

fn open(out: &mut String, tmux: bool) {
    out.push_str(if tmux { "\x1bPtmux;\x1b\x1b_G" } else { "\x1b_G" });
}

fn close(out: &mut String, tmux: bool) {
    out.push_str(if tmux { "\x1b\x1b\\\x1b\\" } else { "\x1b\\" });
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// `writeKittyTransmit` (format `auto`): the PNG when there is one, else RGB or RGBA pixels.
pub fn transmit(out: &mut String, image: &Picture, id: u32, tmux: bool) {
    if let Some(png) = &image.png {
        for (i, chunk) in png.chunks(RAW_CHUNK).enumerate() {
            let more = (i + 1) * RAW_CHUNK < png.len();
            open(out, tmux);
            if i == 0 {
                let _ = write!(out, "a=t,f=100,i={id},m={},q=2;", more as u8);
            } else {
                let _ = write!(out, "m={},q=2;", more as u8);
            }
            out.push_str(&b64(chunk));
            close(out, tmux);
        }
        return;
    }
    let (format, payload) = if image.has_alpha {
        (32, std::borrow::Cow::Borrowed(&image.rgba[..]))
    } else {
        (24, std::borrow::Cow::Owned(image.rgba.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect::<Vec<u8>>()))
    };
    for (i, chunk) in payload.chunks(RAW_CHUNK).enumerate() {
        let more = (i + 1) * RAW_CHUNK < payload.len();
        open(out, tmux);
        if i == 0 {
            let _ = write!(out, "a=t,f={format},s={},v={},i={id},m={},q=2;", image.width, image.height, more as u8);
        } else {
            let _ = write!(out, "m={},q=2;", more as u8);
        }
        out.push_str(&b64(chunk));
        close(out, tmux);
    }
}

/// `kittyPlacementZ`: above cell backgrounds (`INT32_MIN / 2` and up), under glyphs.
pub fn placement_z(placement_id: u32) -> i32 {
    i32::MIN / 2 + placement_id.min(IMAGE_ID_MASK) as i32
}

/// A placement's cell and source rectangles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Place {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub source_x: u32,
    pub source_y: u32,
    pub source_width: u32,
    pub source_height: u32,
}

/// `writeKittyPlacement`: move to the cell, then place.
pub fn placement(out: &mut String, id: u32, placement_id: u32, p: Place, z: i32, tmux: bool) {
    let _ = write!(out, "\x1b[{};{}H", p.y + 1, p.x + 1);
    open(out, tmux);
    let _ = write!(
        out,
        "a=p,i={id},p={placement_id},c={},r={},x={},y={},w={},h={},C=1,z={z},q=2",
        p.width, p.height, p.source_x, p.source_y, p.source_width, p.source_height
    );
    close(out, tmux);
}

/// `writeKittyPlacementAtCursor` (scrollback images).
pub fn placement_at_cursor(out: &mut String, id: u32, placement_id: u32, width: u32, height: u32, z: i32, tmux: bool) {
    open(out, tmux);
    let _ = write!(out, "a=p,i={id},p={placement_id},c={width},r={height},C=1,z={z},q=2");
    close(out, tmux);
}

/// `writeKittyDelete`: `d=I` frees the image and all its placements, `d=i` with a placement id drops that one.
pub fn delete(out: &mut String, id: u32, placement_id: Option<u32>, free_image: bool, tmux: bool) {
    open(out, tmux);
    let _ = write!(out, "a=d,d={},i={id}", if free_image { 'I' } else { 'i' });
    if let Some(p) = placement_id {
        let _ = write!(out, ",p={p}");
    }
    out.push_str(",q=2");
    close(out, tmux);
}

/// The capability query OpenTUI sends at startup; a reply naming `i=31337` means the terminal speaks the protocol.
pub const QUERY: &str = "\x1b_Gi=31337,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transmits_png_bytes_in_chunks() {
        let mut image = Picture::from_rgba(1, 1, vec![1, 2, 3, 255]);
        image.png = Some((0..5000u32).map(|i| i as u8).collect());
        let mut out = String::new();
        transmit(&mut out, &image, 7, false);
        let first = b64(&image.png.as_ref().unwrap()[..3072]);
        let second = b64(&image.png.as_ref().unwrap()[3072..]);
        assert_eq!(out, format!("\x1b_Ga=t,f=100,i=7,m=1,q=2;{first}\x1b\\\x1b_Gm=0,q=2;{second}\x1b\\"));
    }

    #[test]
    fn transmits_rgb_or_rgba_pixels() {
        let mut out = String::new();
        transmit(&mut out, &Picture::from_rgba(2, 1, vec![1, 2, 3, 255, 4, 5, 6, 255]), 9, false);
        assert_eq!(out, format!("\x1b_Ga=t,f=24,s=2,v=1,i=9,m=0,q=2;{}\x1b\\", b64(&[1, 2, 3, 4, 5, 6])));
        let mut out = String::new();
        transmit(&mut out, &Picture::from_rgba(1, 1, vec![1, 2, 3, 4]), 9, true);
        assert_eq!(out, format!("\x1bPtmux;\x1b\x1b_Ga=t,f=32,s=1,v=1,i=9,m=0,q=2;{}\x1b\x1b\\\x1b\\", b64(&[1, 2, 3, 4])));
        // 1024 RGB pixels fill exactly one chunk.
        let mut out = String::new();
        transmit(&mut out, &Picture::from_rgba(1024, 1, vec![9; 4096].chunks(4).flat_map(|_| [9, 9, 9, 255]).collect()), 1, false);
        assert_eq!(out.matches("\x1b_G").count(), 1);
        assert!(out.contains("m=0"));
    }

    #[test]
    fn places_and_deletes() {
        let mut out = String::new();
        let p = Place { x: 3, y: 4, width: 16, height: 8, source_x: 0, source_y: 10, source_width: 96, source_height: 28 };
        placement(&mut out, 100, 2, p, placement_z(2), false);
        assert_eq!(out, "\x1b[5;4H\x1b_Ga=p,i=100,p=2,c=16,r=8,x=0,y=10,w=96,h=28,C=1,z=-1073741822,q=2\x1b\\");
        let mut out = String::new();
        delete(&mut out, 100, None, true, false);
        delete(&mut out, 100, Some(2), false, false);
        assert_eq!(out, "\x1b_Ga=d,d=I,i=100,q=2\x1b\\\x1b_Ga=d,d=i,i=100,p=2,q=2\x1b\\");
        let mut out = String::new();
        placement_at_cursor(&mut out, 5, 1, 4, 2, placement_z(1), false);
        assert_eq!(out, "\x1b_Ga=p,i=5,p=1,c=4,r=2,C=1,z=-1073741823,q=2\x1b\\");
    }
}
