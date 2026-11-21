//! Terminal images (OpenTUI's `<image>` renderable and its native renderer, `@opentui/core` 0.8.4).
//!
//! The TUI shows images with the Kitty graphics protocol, Sixel, or quadrant characters when the terminal does
//! neither ([`caps`] picks one). [`decode`] reads sources, [`frame`] places images in a frame and computes the
//! escapes, [`kitty`], [`sixel`] and [`blocks`] encode them, and [`snapshot`] writes images into scrollback (mini
//! mode).

pub mod blocks;
pub mod caps;
pub mod decode;
pub mod frame;
pub mod kitty;
pub mod resize;
pub mod sixel;
pub mod snapshot;
pub mod store;

pub use caps::{Caps, Protocol, Requested};
pub use decode::Picture;
pub use frame::{Fit, Graphics};

#[cfg(test)]
pub(crate) mod tests {
    /// A PNG of `w` x `h` pixels colored by `f(x, y)`.
    pub fn png(w: u32, h: u32, f: impl Fn(u32, u32) -> [u8; 4]) -> Vec<u8> {
        let img = image::RgbaImage::from_fn(w, h, |x, y| image::Rgba(f(x, y)));
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img).write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }
}
