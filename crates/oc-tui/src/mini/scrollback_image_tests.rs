//! `test/mini/scrollback.image.test.ts`: scrollback image entries (`session.image_preview`) fit the terminal at the
//! left edge under their caption and commit only their rows; a failed decode prints `No preview`; previews off or
//! monochrome keep the captions only.

use super::super::theme;
use super::super::types::{EntryKind, Phase, Source, StreamCommit};
use super::*;
use crate::graphics::Caps;
use crate::theme::Mode;
use base64::Engine as _;

const WIDE: &[u8] = include_bytes!("../graphics/testdata/landscape.png");
/// A 48 x 192 PNG with four colored quadrants.
const TALL: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAADAAAADACAIAAADN8nfJAAAA+klEQVR4nO3bUQ0AIAxDwUmZEETMf+YFTDR8XYIAQmBs7WvdM5HVoVU25ITcIa9MHVIYfR0+V+2HBk3HqKc2dZjLDIpGaWIDOYZgRUGjMVJh6dSEc9YC84U9xcDjKPJcudJ8eyAB1AKMAtcBNCGsMGgoPRwjsBJ6Cs6FLwO8EeeYfKkFuQ5BE1EcYaU/ca6dyOrQsiEn5A55ZeqQwujr8LlqPzRoOkY9tanDXGZQNEoTG8gxBCsKGo2RCkunJpyzFpgv7CkGHkeR58qV5tsDCaAWYBS4DqAJYYVBQ+nhGIGV0FNwLnwZ4I04x+RLLch1CJqI4ggr7Y841wMtbiD+34CxQgAAAABJRU5ErkJggg==";

fn wide() -> String {
    format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(WIDE))
}

struct Sink3 {
    width: usize,
    height: u16,
    footer: usize,
    tail: usize,
    caps: Caps,
    commits: Vec<Commit>,
}

impl Sink for Sink3 {
    fn width(&self) -> usize {
        self.width
    }
    fn tail_column(&self) -> usize {
        self.tail
    }
    fn commit(&mut self, c: Commit) {
        self.tail = tail_after_commit(&c, self.tail, self.width);
        self.commits.push(c);
    }
    fn graphics(&self) -> Option<(Caps, (u16, u16), usize)> {
        Some((self.caps.clone(), (self.width as u16, self.height), self.footer))
    }
}

fn setup(width: usize, height: u16, preview: bool, mono: bool) -> (ScrollbackStream, Sink3) {
    let t = if mono { theme::mono_theme(Mode::Dark) } else { theme::fallback(Mode::Dark) };
    (ScrollbackStream::new(t, false, mono, true, preview), Sink3 { width, height, footer: 6, tail: 0, caps: Caps::from_env(&|_| None), commits: vec![] })
}

fn image(source: &str, text: &str, part: &str) -> StreamCommit {
    StreamCommit {
        kind: EntryKind::User,
        source: Source::System,
        phase: Phase::Final,
        message_id: Some("message-1".into()),
        part_id: Some(part.into()),
        text: text.into(),
        image: Some(source.into()),
        ..Default::default()
    }
}

fn image_commits(s: &Sink3) -> Vec<&Commit> {
    s.commits.iter().filter(|c| !c.images.is_empty() || c.buf.row_text(0).trim() != "").collect()
}

#[test]
fn fits_a_wide_and_a_tall_image_at_the_left_edge() {
    for (src, w, h, size) in [(wide(), 40, 30, (40, 10)), (TALL.to_string(), 80, 24, (9, 17))] {
        let (mut st, mut sink) = setup(w, h, true, false);
        st.append(&mut sink, image(&src, "landscape.png", "image-1"));
        let c = image_commits(&sink);
        assert_eq!(c.len(), 1);
        let im = &c[0].images[0];
        assert_eq!((im.width, im.height), size);
        assert_eq!((im.x, im.y), (0, 1));
        assert_eq!(c[0].buf.row_text(0).trim_end(), "\u{203a} landscape.png");
        assert_eq!(c[0].buf.height, size.1 + 1);
    }
}

#[test]
fn reserves_wrapped_caption_rows_and_allows_caption_only_output() {
    let (mut st, mut sink) = setup(20, 10, true, false);
    st.append(&mut sink, image(TALL, "first caption wraps here\nsecond caption line", "image-1"));
    let c = image_commits(&sink)[0].clone();
    assert_eq!(c.images[0].y, 3);
    assert_eq!(c.images[0].height, 1);
    assert_eq!(c.buf.height, 4);
    sink.height = 8;
    st.append(&mut sink, image(&wide(), "first caption wraps here\nsecond caption line", "image-2"));
    let last = sink.commits.last().unwrap();
    assert!(last.images.is_empty());
    assert_eq!(last.buf.height, 3);
}

#[test]
fn uses_physical_cell_geometry_without_enlarging() {
    let (mut st, mut sink) = setup(80, 24, true, false);
    sink.caps.resolution = Some((800, 720));
    st.append(&mut sink, image(&wide(), "landscape.png", "image-1"));
    let im = &image_commits(&sink)[0].images[0];
    assert_eq!((im.width, im.height, im.x), (6, 1, 0));
    assert_eq!((im.pixel_width, im.pixel_height), (60, 30));
}

#[test]
fn renders_every_image_in_order_after_active_text() {
    let (mut st, mut sink) = setup(80, 24, true, false);
    let first = StreamCommit { kind: EntryKind::Tool, source: Source::Tool, tool: Some("read".into()), ..image(&wide(), "image 1", "image-1") };
    st.append(&mut sink, StreamCommit { image: None, tool: Some("shell".into()), phase: Phase::Progress, text: "before images".into(), ..first.clone() });
    st.append(&mut sink, first.clone());
    for i in 2..=5 {
        st.append(&mut sink, StreamCommit { part_id: Some(format!("image-{i}")), text: format!("image {i}"), image: Some(TALL.into()), ..first.clone() });
    }
    st.append(&mut sink, StreamCommit::new(EntryKind::System, "after images", Phase::Final, Source::System));
    st.complete(&mut sink, false);
    let firsts: Vec<String> = sink.commits.iter().map(|c| c.buf.row_text(0).trim().to_string()).filter(|t| !t.is_empty()).collect();
    assert_eq!(firsts, ["before images", "image 1", "image 2", "image 3", "image 4", "image 5", "after images"]);
    assert_eq!(sink.commits.iter().filter(|c| !c.images.is_empty()).count(), 5);
}

#[test]
fn keeps_captions_without_images_when_off_or_monochrome() {
    for (preview, mono) in [(false, false), (true, true)] {
        let (mut st, mut sink) = setup(80, 24, preview, mono);
        let commit = image("data:image/png;base64,AQID", "attachment.png", "image-1");
        st.append(&mut sink, commit.clone());
        st.append(&mut sink, StreamCommit { kind: EntryKind::Tool, source: Source::Tool, tool: Some("read".into()), part_id: Some("image-2".into()), ..commit });
        assert!(sink.commits.iter().all(|c| c.images.is_empty()));
        let firsts: Vec<String> = sink.commits.iter().map(|c| c.buf.row_text(0).trim().to_string()).filter(|t| !t.is_empty()).collect();
        assert_eq!(firsts, [format!("{} attachment.png", if mono { ">" } else { "\u{203a}" }), "attachment.png".into()]);
    }
}

#[test]
fn prints_a_failed_decode_caption_and_continues() {
    let (mut st, mut sink) = setup(80, 24, true, false);
    st.append(&mut sink, image("data:image/png;base64,AQID", "broken.png", "image-1"));
    st.append(&mut sink, StreamCommit::new(EntryKind::System, "still running", Phase::Final, Source::System));
    st.complete(&mut sink, false);
    let text: Vec<String> = sink.commits.iter().flat_map(|c| (0..c.buf.height).map(|y| c.buf.row_text(y).trim_end().to_string()).collect::<Vec<_>>()).collect();
    let joined = text.join("\n");
    assert!(joined.contains("\u{203a} broken.png\nNo preview"), "{joined}");
    assert!(joined.contains("still running"), "{joined}");
}
