//! Kitty graphics in the terminal pane.
//!
//! A program shows a picture by sending it as an APC escape sequence, and the vt100
//! parser the pane is built on drops those: it has no hook for one, and neither does the
//! state machine underneath it. So the output stream is split before it reaches the
//! parser — text on one side, graphics commands on the other — and what the commands
//! place is drawn over the pane's cells once the text has been laid out.
//!
//! What a program can do here is the part of the protocol it reaches for when it wants
//! to put a picture on the screen: ask whether graphics work at all, send an image as
//! PNG or as raw pixels, in one piece or in chunks, directly or by naming a file, place
//! it at the cursor, and delete it again. Animation, the unicode placeholder scheme and
//! the rest are refused by name, which is what tells a program to fall back to something
//! else rather than wait.
//!
//! A placement is remembered against the line of the pane's history it was made on, so
//! it follows the text as that scrolls, and it goes when the screen it was drawn over
//! does.

use std::collections::VecDeque;

use base64::Engine;
use ratatui::layout::Size;

/// The most an unfinished escape sequence may hold before it is given up on. A program
/// splits a transmission into chunks of a few kilobytes; one that sends an image whole
/// is still bounded by this, and a stream that opens a sequence and never closes it
/// cannot grow past it.
const MOST_CARRY: usize = 8 * 1024 * 1024;

/// How many images are kept for a program to place again after sending them, and how
/// many placements the pane will draw at once.
const KEPT: usize = 8;
const PLACEMENTS: usize = 16;

/// A file this large is not a picture, and reading it to find out is worse than saying
/// no.
const MOST_BYTES: u64 = 32 * 1024 * 1024;

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;

/// A piece of the output stream, in the order it arrived.
#[derive(Debug, PartialEq, Eq)]
pub enum Piece {
    /// Output for the parser, exactly as the program wrote it.
    Text(String),
    /// The screen was told to erase itself, so whatever was drawn over it is gone. The
    /// sequence itself is in the text before this, because the parser does the erasing.
    Erase,
    Command(Command),
}

/// A graphics command: what to do, and the base64 it came with.
#[derive(Debug, PartialEq, Eq)]
pub struct Command {
    control: Control,
    payload: Vec<u8>,
}

/// The keys in front of a command's payload, with the protocol's defaults filled in.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Control {
    /// `a`: query, transmit, transmit and display, place, delete.
    action: char,
    /// `f`: 100 for a file format the image crate reads, 24 or 32 for raw pixels.
    format: u32,
    /// `t`: direct, a file, a temporary file, shared memory.
    medium: char,
    id: u32,
    number: u32,
    /// `m`: another chunk of the same image follows.
    more: bool,
    /// `s` and `v`: the pixel size, which raw pixels do not carry themselves.
    width: u32,
    height: u32,
    /// `c` and `r`: the room to draw in, in cells.
    columns: u16,
    rows: u16,
    /// `o=z`: the payload is zlib compressed.
    compressed: bool,
    /// `q`: 1 says not to answer when it worked, 2 says not to answer at all.
    quiet: u8,
    /// `C`: whether the cursor stays where it is rather than moving past the image.
    still: bool,
    /// `d`: which placements a delete is for.
    delete: char,
}

impl Default for Control {
    fn default() -> Self {
        Self {
            action: 't',
            format: 32,
            medium: 'd',
            id: 0,
            number: 0,
            more: false,
            width: 0,
            height: 0,
            columns: 0,
            rows: 0,
            compressed: false,
            quiet: 0,
            still: false,
            delete: 'a',
        }
    }
}

impl Command {
    /// Read a command out of the body of an APC sequence, which is everything between
    /// `ESC _` and its terminator. `None` for an APC that is not a graphics command, or
    /// one with a key it cannot read.
    fn parse(body: &str) -> Option<Self> {
        let body = body.strip_prefix('G')?;
        let (keys, payload) = match body.split_once(';') {
            Some((keys, payload)) => (keys, payload),
            None => (body, ""),
        };
        let mut control = Control::default();
        for key in keys.split(',').filter(|key| !key.is_empty()) {
            let (name, value) = key.split_once('=')?;
            let letter = |value: &str| value.chars().next().unwrap_or_default();
            match name {
                "a" => control.action = letter(value),
                "f" => control.format = value.parse().ok()?,
                "t" => control.medium = letter(value),
                "i" => control.id = value.parse().ok()?,
                "I" => control.number = value.parse().ok()?,
                "m" => control.more = value == "1",
                "s" => control.width = value.parse().ok()?,
                "v" => control.height = value.parse().ok()?,
                "c" => control.columns = value.parse().ok()?,
                "r" => control.rows = value.parse().ok()?,
                "o" => control.compressed = value == "z",
                "q" => control.quiet = value.parse().ok()?,
                "C" => control.still = value == "1",
                "d" => control.delete = letter(value),
                // A key this does not know is one of the parts of the protocol it does
                // not answer; the action tells it whether that matters.
                _ => {}
            }
        }
        // The payload stays as it was written: a transmission split into chunks is only
        // base64 once the chunks are back together.
        Some(Self {
            control,
            payload: payload.trim().as_bytes().to_vec(),
        })
    }
}

/// Where the cursor is, in the pane's own coordinates: `line` counts from the top of
/// everything the pane has shown, so it keeps meaning the same line after a scroll.
#[derive(Debug, Clone, Copy)]
pub struct Spot {
    pub line: i64,
    pub column: u16,
}

/// An image the pane is drawing, and where.
pub struct Placement {
    /// What the image is called in the picture store.
    pub key: String,
    /// The line of the pane's history the top of the image sits on.
    pub line: i64,
    pub column: u16,
    pub size: Size,
    id: u32,
    /// Which feed placed it, for a pane that cannot follow its own scrolling.
    feed: u64,
}

/// What a command asks of the pane: an answer for the program, and the cursor movement
/// that displaying an image amounts to.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub reply: Option<String>,
    pub motion: Option<String>,
}

struct Kept {
    id: u32,
    key: String,
    source: Held,
}

/// An image as it was sent, kept so a program can place it again without sending it
/// twice.
enum Held {
    Encoded(Vec<u8>),
    Pixels {
        bytes: Vec<u8>,
        width: u32,
        height: u32,
        alpha: bool,
    },
}

pub struct Graphics {
    /// What names this pane's images in the picture store, so two panes showing the same
    /// picture do not share an entry that one of them may replace.
    prefix: String,
    /// An escape sequence cut in half by the end of a chunk.
    carry: String,
    /// A transmission still arriving. The chunks after the first describe nothing, so
    /// the first chunk's keys are what the whole image is read by.
    pending: Option<Command>,
    kept: VecDeque<Kept>,
    placements: Vec<Placement>,
    /// Counts feeds, so a pane that cannot say how far its text moved can at least drop
    /// what it drew before the text moved.
    feed: u64,
    /// Numbers the images, so an id sent twice with different pictures is two entries in
    /// the picture store rather than one.
    serial: u64,
}

impl Graphics {
    pub fn new(prefix: &str) -> Self {
        Self {
            prefix: prefix.to_string(),
            carry: String::new(),
            pending: None,
            kept: VecDeque::new(),
            placements: Vec::new(),
            feed: 0,
            serial: 0,
        }
    }

    pub fn placements(&self) -> &[Placement] {
        &self.placements
    }

    /// Forget every placement: the screen they were drawn over has gone.
    pub fn clear(&mut self) {
        self.placements.clear();
    }

    /// Drop everything placed before this feed, for a pane whose history cannot say how
    /// far the text under an image has moved.
    pub fn keep_only_the_newest(&mut self) {
        let feed = self.feed;
        self.placements.retain(|placement| placement.feed == feed);
    }

    /// Split a chunk of output into what the parser should see and what it cannot.
    ///
    /// A sequence that runs off the end of the chunk is held back whole, so a command
    /// split across two reads still arrives as one.
    pub fn split(&mut self, data: &str) -> Vec<Piece> {
        self.feed += 1;
        let input = if self.carry.is_empty() {
            data.to_string()
        } else {
            let mut carried = std::mem::take(&mut self.carry);
            carried.push_str(data);
            carried
        };
        let bytes = input.as_bytes();
        let mut pieces = Vec::new();
        // The run of plain output waiting to be handed over, and where we have read to.
        let mut text = 0;
        let mut at = 0;
        while at < bytes.len() {
            if bytes[at] != ESC {
                at += 1;
                continue;
            }
            match bytes.get(at + 1) {
                // An escape sequence with nothing after it yet.
                None => break,
                Some(b'_') => {
                    let Some((body, next)) = apc(bytes, at) else {
                        break;
                    };
                    // Whether or not it is a command we know, it is not for the parser:
                    // the state machine would swallow it and print nothing anyway.
                    push(&mut pieces, &input[text..at]);
                    if let Some(command) = Command::parse(&input[at + 2..body]) {
                        pieces.push(Piece::Command(command));
                    }
                    at = next;
                    text = next;
                }
                Some(b'[') => match csi(bytes, at) {
                    Some(next) => {
                        if erases(&input[at + 2..next]) {
                            // The parser does the erasing, so the sequence goes with the
                            // text and the note comes after it.
                            push(&mut pieces, &input[text..next]);
                            pieces.push(Piece::Erase);
                            text = next;
                        }
                        at = next;
                    }
                    // An unfinished sequence is left to the parser, which waits for the
                    // rest of it. Nothing in it is ours.
                    None => break,
                },
                // A full reset takes the screen with it.
                Some(b'c') => {
                    push(&mut pieces, &input[text..at + 2]);
                    pieces.push(Piece::Erase);
                    at += 2;
                    text = at;
                }
                _ => at += 2,
            }
        }
        push(&mut pieces, &input[text..at]);
        if at < bytes.len() {
            let rest = &input[at..];
            // A sequence that never ends is not worth holding the screen back for.
            if rest.len() <= MOST_CARRY {
                self.carry.push_str(rest);
            }
        }
        pieces
    }

    /// Do what a command asks, with the cursor where `spot` says and a pane `size` cells
    /// across and down.
    pub fn take(&mut self, command: Command, spot: Spot, size: (u16, u16)) -> Outcome {
        match command.control.action {
            'q' => Outcome {
                reply: self.answer(&command.control, self.supported(&command.control), true),
                motion: None,
            },
            'd' => {
                self.delete(&command.control);
                Outcome::default()
            }
            't' | 'T' => self.transmit(command, spot, size),
            'p' => {
                let control = command.control;
                let result = self.display(&control, spot, size);
                self.finish(&control, result)
            }
            _ => Outcome {
                reply: self.answer(
                    &command.control,
                    Err("ENOTSUPPORTED:tria draws pictures and no more".into()),
                    false,
                ),
                motion: None,
            },
        }
    }

    /// Take an image in, whole or a chunk at a time, and display it if asked.
    fn transmit(&mut self, command: Command, spot: Spot, size: (u16, u16)) -> Outcome {
        let Command {
            mut control,
            mut payload,
        } = command;
        // Only the first chunk describes the image, so the rest is appended to it.
        if let Some(pending) = self.pending.take() {
            let mut whole = pending.payload;
            whole.extend_from_slice(&payload);
            payload = whole;
            control = Control {
                more: control.more,
                quiet: control.quiet.max(pending.control.quiet),
                ..pending.control
            };
        }
        if control.more {
            if payload.len() <= MOST_CARRY {
                self.pending = Some(Command { control, payload });
            }
            return Outcome::default();
        }

        let result = match self.receive(&control, payload) {
            Ok(held) => {
                self.keep(&control, held);
                match control.action {
                    'T' => self.display(&control, spot, size),
                    _ => Ok(None),
                }
            }
            Err(problem) => Err(problem),
        };
        self.finish(&control, result)
    }

    /// Turn a reply and a cursor movement into what the pane should do next.
    fn finish(&mut self, control: &Control, result: Result<Option<String>, String>) -> Outcome {
        match result {
            Ok(motion) => Outcome {
                reply: self.answer(control, Ok(()), false),
                motion,
            },
            Err(problem) => Outcome {
                reply: self.answer(control, Err(problem), false),
                motion: None,
            },
        }
    }

    /// Read the bytes a transmission carries into something that can be drawn.
    fn receive(&self, control: &Control, payload: Vec<u8>) -> Result<Held, String> {
        self.supported(control)?;
        let payload = base64::engine::general_purpose::STANDARD
            .decode(payload)
            .map_err(|_| "EINVAL:the payload is not base64".to_string())?;
        let mut bytes = match control.medium {
            'd' => payload,
            _ => {
                let path = String::from_utf8(payload)
                    .map_err(|_| "EBADF:the file name is not text".to_string())?;
                let named = std::fs::metadata(&path)
                    .map_err(|err| format!("EBADF:{}", plainly(&err)))?
                    .len();
                if named > MOST_BYTES {
                    return Err("EINVAL:the file is far too big to be a picture".into());
                }
                let bytes =
                    std::fs::read(&path).map_err(|err| format!("EBADF:{}", plainly(&err)))?;
                // The protocol has the terminal delete a temporary file once it has read
                // it, and says which names are safe to delete by.
                if control.medium == 't' && path.contains("tty-graphics-protocol") {
                    let _ = std::fs::remove_file(&path);
                }
                bytes
            }
        };
        if control.compressed {
            let mut out = Vec::new();
            std::io::copy(
                &mut flate2::read::ZlibDecoder::new(bytes.as_slice()),
                &mut out,
            )
            .map_err(|_| "EINVAL:the compressed data does not unpack".to_string())?;
            bytes = out;
        }
        match control.format {
            100 => Ok(Held::Encoded(bytes)),
            24 | 32 => {
                let alpha = control.format == 32;
                let channels = if alpha { 4 } else { 3 };
                let wanted = control.width as usize * control.height as usize * channels;
                if wanted == 0 || bytes.len() < wanted {
                    return Err("EINVAL:the pixels do not fill the size given".into());
                }
                bytes.truncate(wanted);
                Ok(Held::Pixels {
                    bytes,
                    width: control.width,
                    height: control.height,
                    alpha,
                })
            }
            _ => Err("EFORMAT:tria reads PNG and raw pixels".into()),
        }
    }

    /// Whether a command asks for anything this cannot do. What a query answers.
    fn supported(&self, control: &Control) -> Result<(), String> {
        match control.medium {
            'd' => {}
            'f' | 't' if crate::picture::reads_files() => {}
            'f' | 't' => {
                return Err("EBADF:the pane's files are not on this machine".into());
            }
            _ => return Err("ENOTSUPPORTED:tria takes an image in the sequence or by name".into()),
        }
        match control.format {
            24 | 32 | 100 => Ok(()),
            _ => Err("EFORMAT:tria reads PNG and raw pixels".into()),
        }
    }

    /// Keep an image for a later placement.
    fn keep(&mut self, control: &Control, source: Held) {
        self.serial += 1;
        let key = format!("{}/{}", self.prefix, self.serial);
        if control.id != 0 {
            self.kept.retain(|kept| kept.id != control.id);
        }
        self.kept.push_front(Kept {
            id: control.id,
            key,
            source,
        });
        self.kept.truncate(KEPT);
    }

    /// Put an image on the screen at the cursor, and say how far that moves the cursor.
    fn display(
        &mut self,
        control: &Control,
        spot: Spot,
        size: (u16, u16),
    ) -> Result<Option<String>, String> {
        let kept = self
            .kept
            .iter()
            .find(|kept| control.id == 0 || kept.id == control.id)
            .ok_or_else(|| "ENOENT:there is no image by that name".to_string())?;
        let key = kept.key.clone();
        let source = match &kept.source {
            Held::Encoded(bytes) => crate::picture::Source::Bytes(bytes),
            Held::Pixels {
                bytes,
                width,
                height,
                alpha,
            } => crate::picture::Source::Pixels {
                bytes,
                width: *width,
                height: *height,
                alpha: *alpha,
            },
        };
        // An image is drawn at its own size, and shrunk to whatever room is left between
        // the cursor and the edges of the pane rather than spilling over them.
        let room = Size::new(
            match control.columns {
                0 => size.0.saturating_sub(spot.column),
                columns => columns.min(size.0.saturating_sub(spot.column)),
            },
            match control.rows {
                0 => size.1,
                rows => rows.min(size.1),
            },
        );
        let drawn = crate::picture::place(&key, source, room)
            .ok_or_else(|| "EINVAL:the image cannot be drawn".to_string())?;

        self.placements
            .retain(|placement| placement.line != spot.line || placement.column != spot.column);
        self.placements.push(Placement {
            key,
            line: spot.line,
            column: spot.column,
            size: drawn,
            id: control.id,
            feed: self.feed,
        });
        if self.placements.len() > PLACEMENTS {
            self.placements.remove(0);
        }

        // The cursor ends up past the bottom right of the image, which is where a
        // program expects to carry on writing. Line feeds rather than a jump, because
        // an image at the foot of the screen scrolls it.
        Ok((!control.still).then(|| {
            let column = (spot.column + drawn.width).min(size.0.saturating_sub(1));
            format!(
                "{}\x1b[{}G",
                "\n".repeat(drawn.height.saturating_sub(1) as usize),
                column + 1
            )
        }))
    }

    /// `a=d`: take placements off the screen, and with a capital letter the image too.
    fn delete(&mut self, control: &Control) {
        match control.delete {
            'i' | 'I' => {
                self.placements
                    .retain(|placement| placement.id != control.id);
                if control.delete == 'I' {
                    self.kept.retain(|kept| kept.id != control.id);
                }
            }
            'a' | 'A' => {
                self.placements.clear();
                if control.delete == 'A' {
                    self.kept.clear();
                }
            }
            // The rest name placements by where they are, which is close enough to all
            // of them for a pane that holds a handful.
            _ => self.placements.clear(),
        }
    }

    /// What to tell the program. It hears about a command it named, and about a query
    /// whether it named it or not, unless it asked to be spared the answer.
    fn answer(&self, control: &Control, result: Result<(), String>, asked: bool) -> Option<String> {
        let quiet = match result {
            Ok(()) => 1,
            Err(_) => 2,
        };
        if control.quiet >= quiet {
            return None;
        }
        if !asked && control.id == 0 && control.number == 0 {
            return None;
        }
        let mut keys = String::new();
        if control.id != 0 {
            keys.push_str(&format!("i={}", control.id));
        }
        if control.number != 0 {
            if !keys.is_empty() {
                keys.push(',');
            }
            keys.push_str(&format!("I={}", control.number));
        }
        let message = match result {
            Ok(()) => "OK".to_string(),
            Err(problem) => problem,
        };
        Some(format!("\x1b_G{keys};{message}\x1b\\"))
    }
}

/// A message for the program, with nothing in it that would end the reply early.
fn plainly(err: &std::io::Error) -> String {
    err.to_string().replace([':', ';', '\x1b'], " ")
}

fn push(pieces: &mut Vec<Piece>, text: &str) {
    if !text.is_empty() {
        pieces.push(Piece::Text(text.to_string()));
    }
}

/// The end of the APC sequence starting at `at`: where its body stops, and where the
/// stream carries on. `None` while the terminator has not arrived.
fn apc(bytes: &[u8], at: usize) -> Option<(usize, usize)> {
    let mut i = at + 2;
    while i < bytes.len() {
        match bytes[i] {
            BEL => return Some((i, i + 1)),
            ESC if bytes.get(i + 1) == Some(&b'\\') => return Some((i, i + 2)),
            // An escape that is not the terminator ends the sequence the same way a real
            // terminal would: whatever follows is a new sequence, not more payload.
            ESC if i + 1 < bytes.len() => return Some((i, i)),
            _ => i += 1,
        }
    }
    None
}

/// Where the CSI sequence starting at `at` ends, one past its final byte.
fn csi(bytes: &[u8], at: usize) -> Option<usize> {
    let mut i = at + 2;
    while i < bytes.len() {
        if (0x40..=0x7e).contains(&bytes[i]) {
            return Some(i + 1);
        }
        i += 1;
    }
    None
}

/// Whether a CSI sequence, given without its introducer, blanks the whole screen.
fn erases(sequence: &str) -> bool {
    let Some(parameters) = sequence.strip_suffix('J') else {
        return false;
    };
    let first = parameters
        .trim_start_matches(|c: char| !c.is_ascii_digit())
        .split(';')
        .next()
        .unwrap_or_default();
    matches!(first, "2" | "3")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graphics() -> Graphics {
        crate::picture::draw_in_halfblocks();
        Graphics::new("pane")
    }

    /// Run a stretch of output through, and hand back what the program is told.
    fn send(graphics: &mut Graphics, data: &str) -> Vec<String> {
        let mut replies = Vec::new();
        for piece in graphics.split(data) {
            match piece {
                Piece::Command(command) => {
                    let spot = Spot { line: 0, column: 0 };
                    replies.extend(graphics.take(command, spot, (80, 24)).reply);
                }
                Piece::Erase => graphics.clear(),
                Piece::Text(_) => {}
            }
        }
        replies
    }

    fn png() -> String {
        crate::picture::test_png(80, 80)
    }

    #[test]
    fn a_picture_is_taken_out_of_the_stream_and_the_rest_is_left_alone() {
        let mut graphics = graphics();
        let pieces = graphics.split(&format!("before\x1b_Ga=T,f=100;{}\x1b\\after", png()));
        let [
            Piece::Text(before),
            Piece::Command(command),
            Piece::Text(after),
        ] = &pieces[..]
        else {
            panic!("a picture between two runs of text, and not on the screen: {pieces:?}");
        };
        assert_eq!(before, "before");
        assert_eq!(after, "after");
        assert_eq!(command.control.action, 'T');
    }

    #[test]
    fn a_command_split_across_two_reads_arrives_as_one() {
        let mut graphics = graphics();
        let whole = format!("\x1b_Ga=T,f=100;{}\x1b\\", png());
        let (head, tail) = whole.split_at(whole.len() / 2);
        assert_eq!(graphics.split(head), Vec::new());
        assert!(matches!(graphics.split(tail)[..], [Piece::Command(_)]));
        // An escape at the very end of a read is held back too, in case it opens one.
        assert_eq!(graphics.split("a\x1b"), vec![Piece::Text("a".into())]);
        assert!(matches!(
            graphics.split("_Ga=q,i=1;AAAA\x1b\\")[..],
            [Piece::Command(_)]
        ));
    }

    #[test]
    fn a_program_asking_whether_pictures_work_is_answered() {
        let mut graphics = graphics();
        // The question a program asks before it sends anything.
        assert_eq!(
            send(&mut graphics, "\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\"),
            vec!["\x1b_Gi=31;OK\x1b\\"]
        );
        // And one about a way of handing the image over that tria has none of, which is
        // what sends the program looking for another.
        let [refused] = &send(&mut graphics, "\x1b_Gi=7,a=q,t=s,f=32;AAAA\x1b\\")[..] else {
            panic!("a query is always answered");
        };
        assert!(
            refused.starts_with("\x1b_Gi=7;ENOTSUPPORTED"),
            "{refused:?}"
        );
    }

    #[test]
    fn a_picture_sent_a_chunk_at_a_time_is_one_picture() {
        let mut graphics = graphics();
        let data = png();
        // The protocol splits on a base64 boundary, and only the first chunk says what
        // the image is.
        let cut = data.len() / 2 / 4 * 4;
        let (head, tail) = data.split_at(cut);
        assert!(
            send(
                &mut graphics,
                &format!("\x1b_Ga=T,f=100,i=3,m=1;{head}\x1b\\")
            )
            .is_empty()
        );
        assert_eq!(
            send(&mut graphics, &format!("\x1b_Gm=0;{tail}\x1b\\")),
            vec!["\x1b_Gi=3;OK\x1b\\"]
        );
        let [placement] = graphics.placements() else {
            panic!("the chunks are one picture");
        };
        assert!(placement.size.width > 0 && placement.size.height > 0);
    }

    #[test]
    fn pixels_with_nothing_around_them_are_a_picture_too() {
        let mut graphics = graphics();
        let pixels = base64::engine::general_purpose::STANDARD.encode(vec![128u8; 4 * 4 * 3]);
        assert_eq!(
            send(
                &mut graphics,
                &format!("\x1b_Ga=T,f=24,s=4,v=4,i=1;{pixels}\x1b\\")
            ),
            vec!["\x1b_Gi=1;OK\x1b\\"]
        );
        assert_eq!(graphics.placements().len(), 1);
        // A size the pixels do not fill is nothing anyone can draw, and saying so is
        // what stops the program waiting.
        let [refused] = &send(
            &mut graphics,
            &format!("\x1b_Ga=T,f=24,s=40,v=40,i=2;{pixels}\x1b\\"),
        )[..] else {
            panic!("a picture that cannot be drawn is answered");
        };
        assert!(refused.starts_with("\x1b_Gi=2;EINVAL"), "{refused:?}");
    }

    #[test]
    fn a_picture_is_placed_where_the_cursor_is_and_the_cursor_moves_past_it() {
        let mut graphics = graphics();
        let pieces = graphics.split(&format!("\x1b_Ga=T,f=100,c=10,r=4;{}\x1b\\", png()));
        let Ok([Piece::Command(command)]) = <[Piece; 1]>::try_from(pieces) else {
            panic!("the stream is one command and nothing else");
        };
        let spot = Spot {
            line: 12,
            column: 5,
        };
        let outcome = graphics.take(command, spot, (80, 24));
        let [placement] = graphics.placements() else {
            panic!("the picture is on the screen");
        };
        assert_eq!((placement.line, placement.column), (12, 5));
        // Square in, square out: the room it was given is the most it may take, not the
        // shape it is stretched to.
        assert_eq!(placement.size, Size::new(8, 4));
        // Three line feeds put the cursor on the picture's last row, and the column is
        // the one after its right edge, which is where the program writes next.
        assert_eq!(outcome.motion.as_deref(), Some("\n\n\n\x1b[14G"));
    }

    #[test]
    fn a_picture_is_deleted_when_the_program_says_so() {
        let mut graphics = graphics();
        send(
            &mut graphics,
            &format!("\x1b_Ga=T,f=100,i=9;{}\x1b\\", png()),
        );
        assert_eq!(graphics.placements().len(), 1);
        send(&mut graphics, "\x1b_Ga=d,d=i,i=8\x1b\\");
        assert_eq!(graphics.placements().len(), 1, "another picture's name");
        send(&mut graphics, "\x1b_Ga=d,d=i,i=9\x1b\\");
        assert!(graphics.placements().is_empty());
    }

    #[test]
    fn the_screen_being_wiped_is_noticed() {
        let mut graphics = graphics();
        assert!(graphics.split("\x1b[2J").contains(&Piece::Erase));
        assert!(graphics.split("\x1b[3J").contains(&Piece::Erase));
        assert!(graphics.split("\x1bc").contains(&Piece::Erase));
        // Erasing from the cursor down leaves what is above it.
        assert!(!graphics.split("\x1b[0J").contains(&Piece::Erase));
        assert!(!graphics.split("\x1b[1;1H").contains(&Piece::Erase));
        // And the sequences themselves still reach the parser, which does the erasing.
        assert_eq!(graphics.split("\x1b[2J")[0], Piece::Text("\x1b[2J".into()));
    }
}
