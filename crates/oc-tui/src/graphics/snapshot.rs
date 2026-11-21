//! Images written into scrollback (OpenTUI `prepareSnapshotImages`, `writeSnapshotNativeImagesForRow`): mini
//! mode's scrollback entries are snapshots written at the output cursor, and an image in one goes out after the
//! row it ends on. The cursor is saved, moved up to the image's first row and over to its column, the image is
//! sent (Kitty: transmitted and placed at the cursor; Sixel: painted there) and the cursor restored. Images that
//! can't be addressed that way (the commit starts mid-line, an image is taller than the rows above the footer,
//! images overlap or mix protocols) are drawn as text.

use std::sync::Arc;

use super::caps::{Caps, Protocol};
use super::decode::Picture;
use super::{kitty, resize, sixel};

/// One image of a snapshot, at a cell of it.
#[derive(Clone, Debug)]
pub struct SnapImage {
    pub picture: Arc<Picture>,
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub pixel_width: u32,
    pub pixel_height: u32,
}

impl PartialEq for SnapImage {
    fn eq(&self, o: &Self) -> bool {
        self.picture.handle == o.picture.handle
            && (self.x, self.y, self.width, self.height, self.pixel_width, self.pixel_height) == (o.x, o.y, o.width, o.height, o.pixel_width, o.pixel_height)
    }
}

impl SnapImage {
    fn overlaps(&self, o: &SnapImage) -> bool {
        self.x < o.x + o.width && o.x < self.x + self.width && self.y < o.y + o.height && o.y < self.y + self.height
    }
}

/// `prepareSnapshotImages`: the protocol the commit's images go out with, or `None` to draw them as text.
pub fn prepare(caps: &Caps, images: &[SnapImage], row_columns: u32, start_on_new_line: bool, previous_column: u32, pinned: u32) -> Option<Protocol> {
    let first = images.first()?;
    let protocol = caps.resolve(super::Requested::Auto, (first.pixel_width, first.pixel_height));
    for (i, im) in images.iter().enumerate() {
        if caps.resolve(super::Requested::Auto, (im.pixel_width, im.pixel_height)) != protocol || images[..i].iter().any(|p| p.overlaps(im)) {
            return None;
        }
    }
    if protocol == Protocol::Blocks {
        return None;
    }
    if pinned == 0 || (!start_on_new_line && previous_column != 0) {
        return None;
    }
    if images.iter().any(|p| p.x as u32 + p.width as u32 > row_columns || p.height as u32 > pinned) {
        return None;
    }
    Some(protocol)
}

/// The escapes after row `row` for the images ending on it (`writeSnapshotNativeImagesForRow`). `first_id` is
/// the base of the Kitty image ids reserved for this commit (image `i` uses `first_id + i + 1`).
pub fn after_row(out: &mut String, caps: &Caps, images: &[SnapImage], row: u16, protocol: Protocol, first_id: u32) {
    let tmux = caps.tmux();
    for (i, im) in images.iter().enumerate() {
        if im.y + im.height - 1 != row {
            continue;
        }
        out.push_str("\x1b[s");
        if im.height > 1 {
            out.push_str(&format!("\x1b[{}A", im.height - 1));
        }
        out.push('\r');
        if im.x > 0 {
            out.push_str(&format!("\x1b[{}C", im.x));
        }
        let placement_id = i as u32 + 1;
        match protocol {
            Protocol::Sixel => {
                let resized = ((im.pixel_width, im.pixel_height) != (im.picture.width, im.picture.height)).then(|| resize::area(&im.picture, im.pixel_width, im.pixel_height));
                if let Some(payload) = sixel::encode(resized.as_ref().unwrap_or(&im.picture)) {
                    sixel::framed(out, &payload, tmux);
                }
            }
            Protocol::Kitty => {
                let pixels = im.pixel_width as u64 * im.pixel_height as u64;
                let downscale = pixels > 0 && im.picture.width as u64 * im.picture.height as u64 >= pixels * 4;
                let id = first_id + placement_id;
                if downscale {
                    kitty::transmit(out, &resize::area(&im.picture, im.pixel_width, im.pixel_height), id, tmux);
                } else {
                    kitty::transmit(out, &im.picture, id, tmux);
                }
                kitty::placement_at_cursor(out, id, placement_id, im.width as u32, im.height as u32, kitty::placement_z(placement_id), tmux);
            }
            Protocol::Blocks => {}
        }
        out.push_str("\x1b[u");
    }
}

/// The quadrant fallback of image `im`'s cell `(cx, cy)`: `(char, fg, bg)`.
pub fn block_cell(im: &SnapImage, cx: u16, cy: u16) -> (char, [u8; 4], [u8; 4]) {
    let src = (0, 0, im.picture.width, im.picture.height);
    super::blocks::quadrant(super::blocks::samples(&im.picture, src, im.width as u32, im.height as u32, cx as u32, cy as u32))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn im(x: u16, y: u16, w: u16, h: u16) -> SnapImage {
        SnapImage { picture: Arc::new(Picture::from_rgba(2, 2, vec![255; 16])), x, y, width: w, height: h, pixel_width: 20, pixel_height: 40 }
    }

    fn kitty_caps() -> Caps {
        let mut caps = Caps::from_env(&|_| None);
        caps.apply_replies(b"\x1b_Gi=31337;OK\x1b\\");
        caps
    }

    #[test]
    fn addressable_images_only() {
        let caps = kitty_caps();
        assert_eq!(prepare(&caps, &[im(0, 1, 4, 2)], 80, true, 0, 10), Some(Protocol::Kitty));
        assert_eq!(prepare(&caps, &[im(0, 1, 4, 2)], 80, false, 5, 10), None);
        assert_eq!(prepare(&caps, &[im(0, 1, 4, 20)], 80, true, 0, 10), None);
        assert_eq!(prepare(&caps, &[im(0, 1, 4, 2), im(2, 2, 4, 2)], 80, true, 0, 10), None);
        assert_eq!(prepare(&Caps::from_env(&|_| None), &[im(0, 1, 4, 2)], 80, true, 0, 10), None);
    }

    #[test]
    fn kitty_after_its_last_row() {
        let caps = kitty_caps();
        let images = [im(3, 1, 4, 2)];
        let mut out = String::new();
        after_row(&mut out, &caps, &images, 1, Protocol::Kitty, 100);
        assert!(out.is_empty());
        after_row(&mut out, &caps, &images, 2, Protocol::Kitty, 100);
        assert!(out.starts_with("\x1b[s\x1b[1A\r\x1b[3C\x1b_Ga=t,f=24,s=2,v=2,i=101,m=0,q=2;"), "{out:?}");
        assert!(out.ends_with("\x1b_Ga=p,i=101,p=1,c=4,r=2,C=1,z=-1073741823,q=2\x1b\\\x1b[u"), "{out:?}");
    }
}
