//! Images a tool call looked at, drawn in the terminal.
//!
//! An image is the one tool result that is not text, and it reaches the row two ways. A
//! subagent's transcript is the provider's own file and carries the picture inside it. A
//! thread's own rows carry only the path the tool read, because the server projects tool
//! results down to a summary before they go on the wire and an image does not survive
//! that — so the file is read from the disk it names, which is this machine when the
//! server runs here, and nothing at all when it runs somewhere else.
//!
//! Terminals disagree about how a picture is drawn, so the terminal is asked once what it
//! speaks — kitty, iTerm2 or sixel — and where it speaks none of them the image is drawn
//! in half-blocks, which is coarse but is still the image.
//!
//! Decoding and encoding are the expensive part and happen once per image and size, when
//! the row is opened. Everything after that is placement, so scrolling past an open image
//! costs no more than scrolling past text.

use std::{cell::RefCell, collections::VecDeque, time::Duration};

use base64::Engine;
use ratatui::{
    Frame,
    layout::{Rect, Size},
};
use ratatui_image::{
    Resize,
    picker::{Picker, cap_parser::QueryStdioOptions},
    sliced::{SignedPosition, SlicedImage, SlicedProtocol},
};

/// How many images are kept ready to draw. Each holds the encoded form the terminal was
/// given, which for a picture in the chat is large, so this is not a cache of everything
/// a thread ever showed — it is enough that the rows anyone has open stay ready, and that
/// a list of projects drawn with their icons does not push them all out. An image that
/// falls out of it is encoded again the next time it is drawn, which costs a few
/// milliseconds.
const KEPT: usize = 48;

struct Ready {
    key: String,
    /// The room the row offered when this was encoded. A narrower window is a different
    /// image as far as the terminal is concerned, so it is encoded again.
    offered: Size,
    size: Size,
    protocol: SlicedProtocol,
}

#[derive(Default)]
struct Store {
    /// What the terminal answered. `None` until asked, and after an answer that left no
    /// way to draw anything.
    picker: Option<Picker>,
    /// Whether a path the server names is a path here. False against a server on another
    /// machine, where the same path is either nothing or, worse, another file entirely.
    local_files: bool,
    ready: VecDeque<Ready>,
}

/// Where an image's bytes are.
#[derive(Debug, Clone, Copy)]
pub enum Source<'a> {
    /// Base64, as the provider wrote it into the result.
    Data(&'a str),
    /// A path on the machine that ran the tool.
    File(&'a str),
    /// The bytes themselves, for an image that was fetched rather than read.
    Bytes(&'a [u8]),
}

/// A file this large is not a screenshot and is not worth the memory of finding out.
const MOST_BYTES: u64 = 32 * 1024 * 1024;

thread_local! {
    static STORE: RefCell<Store> = RefCell::new(Store::default());
}

/// Ask the terminal what it can draw, and say whether the paths the server names are
/// paths here. Called once, after the screen is taken over and before events are read:
/// the answer comes back on stdin, and the event reader would take it for typing.
pub fn ask_terminal(local_files: bool) {
    let picker = Picker::from_query_stdio_with_options(QueryStdioOptions {
        // A terminal answers a question about itself in the time it takes to get there
        // and back. One that says nothing at all is not going to, and waiting on it is
        // an empty screen for as long as we wait.
        timeout: Duration::from_millis(500),
        ..QueryStdioOptions::default()
    });
    match &picker {
        // Which way images will be drawn is the first thing to know when they come out
        // wrong, and it is decided before there is anywhere on the screen to say it.
        Ok(picker) => tracing::info!(
            "drawing images as {:?} at {:?}",
            picker.protocol_type(),
            picker.font_size()
        ),
        Err(err) => tracing::warn!("no image support: {err}"),
    }
    STORE.with(|store| {
        let mut store = store.borrow_mut();
        store.picker = picker.ok();
        store.local_files = local_files;
    });
}

/// Whether an image named by a path can be shown at all, which is what tells a row with
/// nothing but a path whether it has anything to unfold.
pub fn reads_files() -> bool {
    STORE.with(|store| store.borrow().local_files)
}

/// Forget what has been drawn. The screen has been handed to another program and back,
/// and whatever the terminal was holding went with it.
pub fn forget() {
    STORE.with(|store| store.borrow_mut().ready.clear());
}

/// Take the image in, and say how much room it wants within `offered`. `None` when it
/// cannot be drawn: a file that is not there or not ours to read, an encoding this build
/// does not read, bytes that are not an image, or a terminal with no way to draw one.
pub fn place(key: &str, source: Source<'_>, offered: Size) -> Option<Size> {
    if offered.width == 0 || offered.height == 0 {
        return None;
    }
    STORE.with(|store| {
        let mut store = store.borrow_mut();
        if let Some(index) = store
            .ready
            .iter()
            .position(|ready| ready.key == key && ready.offered == offered)
        {
            // Draw it again, and move it to the front so the rows in view are the ones
            // that survive.
            let ready = store.ready.remove(index)?;
            let size = ready.size;
            store.ready.push_front(ready);
            return Some(size);
        }
        let picker = store.picker.as_ref()?;
        let bytes = match source {
            Source::Data(data) => base64::engine::general_purpose::STANDARD
                .decode(data)
                .ok()?,
            Source::File(path) if store.local_files => {
                if std::fs::metadata(path).ok()?.len() > MOST_BYTES {
                    return None;
                }
                std::fs::read(path).ok()?
            }
            Source::File(_) => return None,
            Source::Bytes(bytes) => bytes.to_vec(),
        };
        let image = image::load_from_memory(&bytes).ok()?;
        // Fit shrinks but never enlarges, so an image smaller than the room it is given
        // is drawn at its own size rather than blown up.
        let natural = Resize::natural_size(&image, picker.font_size());
        let within = Size::new(
            natural.width.min(offered.width),
            natural.height.min(offered.height),
        );
        let protocol =
            SlicedProtocol::new_with_resize(picker, image, within, Resize::Fit(None)).ok()?;
        let size = protocol.size();
        store.ready.retain(|ready| ready.key != key);
        store.ready.push_front(Ready {
            key: key.to_string(),
            offered,
            size,
            protocol,
        });
        store.ready.truncate(KEPT);
        Some(size)
    })
}

/// Draw a placed image at `position` within `area`, clipped to it. The position may sit
/// above or below the area: a chat line scrolls, and half an image is still the image.
pub fn draw(frame: &mut Frame, key: &str, area: Rect, position: SignedPosition) {
    STORE.with(|store| {
        let store = store.borrow();
        if let Some(ready) = store.ready.iter().find(|ready| ready.key == key) {
            frame.render_widget(SlicedImage::new(&ready.protocol, position), area);
        }
    });
}

/// Draw in half-blocks, which is the one way that needs no terminal to answer, and read
/// the files a test names.
#[cfg(test)]
pub fn draw_in_halfblocks() {
    STORE.with(|store| {
        let mut store = store.borrow_mut();
        store.picker = Some(Picker::halfblocks());
        store.local_files = true;
        store.ready.clear();
    });
}

/// A PNG of a size a test can reason about, as base64.
#[cfg(test)]
pub fn test_png(width: u32, height: u32) -> String {
    let image =
        image::RgbImage::from_fn(width, height, |x, _| image::Rgb([(x % 256) as u8, 64, 128]));
    let mut bytes: Vec<u8> = Vec::new();
    image
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .unwrap();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;

    #[test]
    fn an_image_takes_the_room_it_is_offered_and_no_more() {
        draw_in_halfblocks();
        let data = test_png(400, 400);
        let size = place("wide", Source::Data(&data), Size::new(40, 10))
            .expect("halfblocks draws anything");
        assert!(size.width <= 40 && size.height <= 10);
        // Square in, square out: a half-block cell is twice as tall as it is wide, so ten
        // rows of it are twenty cells' worth of picture.
        assert_eq!(size.height, 10);
        // Asked again, it answers from the encoding it already has.
        assert_eq!(
            place("wide", Source::Data(&data), Size::new(40, 10)),
            Some(size)
        );
    }

    #[test]
    fn what_is_not_an_image_takes_no_room() {
        draw_in_halfblocks();
        assert_eq!(
            place("junk", Source::Data("bm90IGFuIGltYWdl"), Size::new(40, 10)),
            None
        );
        assert_eq!(
            place("empty", Source::Data(&test_png(4, 4)), Size::new(0, 10)),
            None
        );
    }

    /// A thread's own rows name a file rather than carrying the picture, so the file is
    /// the image — but only where the path means this machine.
    #[test]
    fn a_named_file_is_read_when_the_disk_is_ours() {
        draw_in_halfblocks();
        let path = std::env::temp_dir().join("tria-a-named-file.png");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(test_png(80, 80))
            .unwrap();
        std::fs::write(&path, bytes).unwrap();
        let path = path.to_str().unwrap();
        assert!(place("named", Source::File(path), Size::new(20, 6)).is_some());
        assert_eq!(
            place("missing", Source::File("/nowhere.png"), Size::new(20, 6)),
            None
        );

        // Against a server on another machine the path is not ours to follow: the same
        // name here is either nothing or somebody else's file.
        STORE.with(|store| {
            let mut store = store.borrow_mut();
            store.local_files = false;
            store.ready.clear();
        });
        assert_eq!(place("named", Source::File(path), Size::new(20, 6)), None);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn only_the_part_of_an_image_inside_the_area_is_drawn() {
        draw_in_halfblocks();
        let size = place(
            "scrolled",
            Source::Data(&test_png(80, 80)),
            Size::new(20, 6),
        )
        .unwrap();
        let mut terminal = Terminal::new(TestBackend::new(30, 8)).unwrap();
        // Two lines above the top of the area: the first two rows of the image are gone
        // and the rest has moved up.
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, 30, 8);
                draw(frame, "scrolled", area, SignedPosition::from((2, -2)));
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        // Half-blocks are colour rather than glyphs: a cell of the image is one the
        // protocol has painted.
        let drawn = |x: u16, y: u16| buffer[(x, y)].bg != ratatui::style::Color::Reset;
        assert!(drawn(2, 0), "the image starts at the left of its indent");
        assert!(!drawn(1, 0), "and not before it");
        let left = size.height - 2;
        assert!(drawn(2, left - 1), "down to the last row still in the area");
        assert!(!drawn(2, left), "and no further");
    }

    #[test]
    fn what_the_terminal_held_is_dropped_when_the_screen_goes() {
        draw_in_halfblocks();
        place("gone", Source::Data(&test_png(8, 8)), Size::new(20, 6)).unwrap();
        forget();
        STORE.with(|store| assert!(store.borrow().ready.is_empty()));
    }
}
