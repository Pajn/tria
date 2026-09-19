//! The drawing set a project's icon can be named from.
//!
//! The server sends a name and a colour — `file-json`, `blue` — and leaves the drawing
//! to whoever is showing it, because the icon itself is a picture that no wire format
//! here carries. Lucide publishes the same set as a font, so the name is looked up as a
//! character and the character is rasterised: a terminal that can draw a picture at all
//! can draw this one, at whatever size the room it was given comes to in pixels.

use std::cell::RefCell;

use fontdue::{Font, FontSettings};
use lucide_icons::{Icon, LUCIDE_FONT_BYTES};

/// An icon drawn into a square of pixels, opaque where the icon is and clear elsewhere.
pub struct Drawn {
    pub bytes: Vec<u8>,
    pub side: u32,
}

thread_local! {
    /// Parsing the font is the expensive part and says nothing about any one icon, so it
    /// happens once. `None` where the font would not parse, which leaves every icon
    /// undrawable rather than pretending about one.
    static FONT: RefCell<Option<Font>> = RefCell::new(
        Font::from_bytes(LUCIDE_FONT_BYTES, FontSettings::default()).ok(),
    );
}

/// Draw `name` at `side` pixels square in `colour`. `None` for a name the set does not
/// have, which is what a client older than the icon somebody chose sees.
pub fn draw(name: &str, colour: Option<&str>, side: u32) -> Option<Drawn> {
    if side == 0 {
        return None;
    }
    let icon = Icon::try_from(name).ok()?;
    let [r, g, b] = ink(colour);
    FONT.with(|font| {
        let font = font.borrow();
        let font = font.as_ref()?;
        // Asked for at the size of the square it goes in: the glyph's own box is about
        // the em, and the little it can stand out of it is cut off rather than shrinking
        // every icon to the width of the widest.
        let (metrics, coverage) = font.rasterize(char::from(icon), side as f32);
        let mut bytes = vec![0u8; (side * side * 4) as usize];
        // Centred on its ink rather than its baseline: these are drawings, and a drawing
        // sitting high in its box for want of a descender looks misplaced.
        let left = (side as i64 - metrics.width as i64) / 2;
        let top = (side as i64 - metrics.height as i64) / 2;
        let mut inked = false;
        for (index, alpha) in coverage.iter().enumerate() {
            if *alpha == 0 {
                continue;
            }
            let x = left + (index % metrics.width) as i64;
            let y = top + (index / metrics.width) as i64;
            if x < 0 || y < 0 || x >= side as i64 || y >= side as i64 {
                continue;
            }
            let at = ((y as u32 * side + x as u32) * 4) as usize;
            bytes[at..at + 4].copy_from_slice(&[r, g, b, *alpha]);
            inked = true;
        }
        // A glyph that came out empty is nothing to draw, and drawing nothing leaves the
        // room it asked for blank.
        inked.then_some(Drawn { bytes, side })
    })
}

/// What an icon is drawn in. The names are the palette the icon was chosen from; the
/// shades are the light end of it, since a terminal is dark far more often than not and
/// the dark end disappears into it. A colour this does not know, including none at all,
/// is drawn in the grey the rest of the list's furniture uses.
fn ink(colour: Option<&str>) -> [u8; 3] {
    match colour.unwrap_or("gray") {
        "red" => [0xf8, 0x71, 0x71],
        "orange" => [0xfb, 0x92, 0x3c],
        "amber" => [0xfb, 0xbf, 0x24],
        "yellow" => [0xfa, 0xcc, 0x15],
        "lime" => [0xa3, 0xe6, 0x35],
        "green" => [0x4a, 0xde, 0x80],
        "emerald" => [0x34, 0xd3, 0x99],
        "teal" => [0x2d, 0xd4, 0xbf],
        "cyan" => [0x22, 0xd3, 0xee],
        "sky" => [0x38, 0xbd, 0xf8],
        "blue" => [0x60, 0xa5, 0xfa],
        "indigo" => [0x81, 0x8c, 0xf8],
        "violet" => [0xa7, 0x8b, 0xfa],
        "purple" => [0xc0, 0x84, 0xfc],
        "fuchsia" => [0xe8, 0x79, 0xf9],
        "pink" => [0xf4, 0x72, 0xb6],
        "rose" => [0xfb, 0x71, 0x85],
        _ => [0x9c, 0xa3, 0xaf],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ink of a drawing, as the rows of it that have any.
    fn ink_rows(drawn: &Drawn) -> Vec<usize> {
        (0..drawn.side as usize)
            .filter(|y| {
                (0..drawn.side as usize)
                    .any(|x| drawn.bytes[(y * drawn.side as usize + x) * 4 + 3] > 0)
            })
            .collect()
    }

    #[test]
    fn an_icon_is_drawn_from_the_name_the_server_sent() {
        let drawn = draw("file-json", Some("blue"), 32).expect("file-json is a lucide icon");
        assert_eq!(drawn.bytes.len(), 32 * 32 * 4);
        let rows = ink_rows(&drawn);
        assert!(!rows.is_empty(), "the icon has ink in it");
        // Drawn where it was asked for: inside the square, and not all down one edge.
        assert!(rows.first() > Some(&0) || rows.len() > 1);
        assert!(*rows.last().unwrap() < 32);
        // In the colour it was given, wherever it is solid.
        let solid = drawn
            .bytes
            .chunks(4)
            .find(|pixel| pixel[3] == 255)
            .expect("an icon is solid somewhere");
        assert_eq!(&solid[..3], &[0x60, 0xa5, 0xfa]);
    }

    #[test]
    fn a_name_the_set_does_not_have_is_not_drawn() {
        assert!(draw("not-a-lucide-icon", None, 32).is_none());
        assert!(draw("file-json", None, 0).is_none());
    }

    /// A colour nobody has heard of is still an icon; it is the grey one.
    #[test]
    fn an_unknown_colour_is_the_colour_of_everything_else() {
        assert_eq!(ink(Some("burnt-sienna")), ink(None));
        assert_eq!(ink(Some("gray")), ink(None));
    }
}
