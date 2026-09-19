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

/// The classes a project's name is read for, in the order they are tried, each with the
/// icon and colour it is drawn in. This is the desktop app's own table: a project nobody
/// gave an icon and whose checkout carries none is still drawn, and drawn the same in
/// both, which is the whole point of copying the guess rather than inventing one.
const CLASSES: &[(&str, &str, &[&str])] = &[
    (
        "bot",
        "violet",
        &["ai", "agent", "bot", "gpt", "llm", "ml", "model", "neural"],
    ),
    (
        "smartphone",
        "lime",
        &[
            "android",
            "expo",
            "ios",
            "mobile",
            "native",
            "reactnative",
            "swift",
        ],
    ),
    (
        "monitor",
        "indigo",
        &[
            "desktop", "electron", "linux", "mac", "macos", "tauri", "windows",
        ],
    ),
    (
        "book-open",
        "amber",
        &[
            "book",
            "docs",
            "documentation",
            "guide",
            "handbook",
            "manual",
            "wiki",
        ],
    ),
    (
        "shield-check",
        "teal",
        &["auth", "identity", "oauth", "security", "sso", "vault"],
    ),
    (
        "database",
        "cyan",
        &[
            "analytics",
            "data",
            "database",
            "db",
            "mongo",
            "mysql",
            "postgres",
            "redis",
            "sql",
            "storage",
        ],
    ),
    (
        "cloud-cog",
        "sky",
        &[
            "aws",
            "azure",
            "cloud",
            "deploy",
            "devops",
            "docker",
            "gcp",
            "infra",
            "kubernetes",
            "terraform",
        ],
    ),
    (
        "server",
        "blue",
        &["api", "backend", "gateway", "server", "service", "worker"],
    ),
    (
        "terminal",
        "green",
        &[
            "automation",
            "bash",
            "cli",
            "command",
            "script",
            "shell",
            "terminal",
        ],
    ),
    (
        "package",
        "orange",
        &[
            "component",
            "kit",
            "lib",
            "library",
            "package",
            "plugin",
            "sdk",
            "toolkit",
        ],
    ),
    (
        "flask-conical",
        "yellow",
        &["benchmark", "e2e", "fixture", "spec", "test", "testing"],
    ),
    (
        "shopping-bag",
        "rose",
        &["cart", "commerce", "market", "shop", "store"],
    ),
    ("gamepad-2", "emerald", &["game", "gaming", "play"]),
    (
        "music",
        "fuchsia",
        &["audio", "music", "podcast", "radio", "sound"],
    ),
    ("video", "red", &["film", "movie", "stream", "video"]),
    (
        "image",
        "pink",
        &["camera", "gallery", "image", "photo", "picture"],
    ),
    (
        "globe-2",
        "sky",
        &[
            "browser", "frontend", "nextjs", "react", "site", "svelte", "ui", "vue", "web",
            "website",
        ],
    ),
];

/// What a name nothing above matched is drawn with. Which one is the name's own hash, so
/// a project keeps the icon it has had rather than taking a new one every release.
const GENERIC: &[(&str, &str)] = &[
    ("code-2", "blue"),
    ("braces", "purple"),
    ("circuit-board", "teal"),
    ("folder-code", "orange"),
    ("layers-3", "fuchsia"),
];

/// The icon and colour a project falls back to, read out of its name the way the desktop
/// app reads it. Nothing about this is sent over the wire — both ends guess, so both
/// ends have to guess alike.
pub fn guess(title: &str, workspace_root: &str) -> (&'static str, &'static str) {
    // A project with no title of its own is the directory it sits in.
    let name = match title.trim() {
        "" => workspace_root
            .rsplit(['/', '\\'])
            .find(|part| !part.is_empty())
            .unwrap_or("project"),
        title => title,
    };
    let words = words(name);
    let mut best: Option<(&'static str, &'static str)> = None;
    let mut best_score = 0;
    for (icon, colour, terms) in CLASSES {
        let score: u32 = words
            .iter()
            .map(|word| {
                terms
                    .iter()
                    .map(|term| term_score(word, term))
                    .max()
                    .unwrap_or(0)
            })
            .sum();
        // Strictly better, so a tie is settled by the order of the table.
        if score > best_score {
            best = Some((icon, colour));
            best_score = score;
        }
    }
    best.unwrap_or_else(|| GENERIC[stable_index(&name.to_lowercase(), GENERIC.len())])
}

/// A name in the words it is made of: `nix-config` is two, and so is `NixConfig`.
fn words(name: &str) -> Vec<String> {
    let mut spaced = String::new();
    let mut last: Option<char> = None;
    for ch in name.chars() {
        if ch.is_ascii_uppercase()
            && last.is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        {
            spaced.push(' ');
        }
        spaced.push(ch);
        last = Some(ch);
    }
    spaced
        .to_lowercase()
        .split(|c: char| !c.is_ascii_lowercase() && !c.is_ascii_digit())
        .filter(|word| !word.is_empty())
        .map(str::to_string)
        .collect()
}

/// How much a word says a project is of a class: the term itself is worth most, and a
/// word a long term starts or ends is worth something. A short term matches only whole,
/// since three letters inside a longer word are a coincidence.
fn term_score(word: &str, term: &str) -> u32 {
    if word == term {
        3
    } else if term.len() >= 4 && (word.starts_with(term) || word.ends_with(term)) {
        1
    } else {
        0
    }
}

/// The desktop app's hash, which is FNV-1a over the name as the language it is written
/// in counts characters — sixteen bits at a time — so that the same name lands on the
/// same icon here.
fn stable_index(value: &str, length: usize) -> usize {
    let mut hash: u32 = 2_166_136_261;
    for unit in value.encode_utf16() {
        hash ^= u32::from(unit);
        hash = hash.wrapping_mul(16_777_619);
    }
    hash as usize % length
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

    /// The guess reads the name for what the project is, and a name that says nothing
    /// still gets an icon rather than a blank.
    #[test]
    fn a_project_with_no_icon_is_guessed_at_from_its_name() {
        // A word the table knows, wherever in the name it is.
        assert_eq!(guess("backend", "/src/backend").0, "server");
        assert_eq!(guess("shell-scripts", "/src/shell-scripts").0, "terminal");
        assert_eq!(guess("docs-site", "/src/docs-site").0, "book-open");
        // Two classes matching is settled by the order of the table, as it is there.
        assert_eq!(guess("react-native-true-image", "/src/rn").0, "smartphone");
        // CamelCase is words too.
        assert_eq!(guess("MyTestSpec", "/src/spec").0, "flask-conical");
        // A name that matches nothing keeps one of the generic icons, and keeps the
        // same one: it is the name's own hash that picks it.
        assert_eq!(guess("tria", "/src/tria"), ("folder-code", "orange"));
        assert_eq!(
            guess("nix-config", "/src/nix-config"),
            ("circuit-board", "teal")
        );
        assert_eq!(guess("tria", "/elsewhere"), guess("Tria", "/src/tria"));
        // A project with no name of its own is the directory it sits in.
        assert_eq!(guess("  ", "/src/backend/"), guess("backend", ""));
        // And every icon the guess can land on is one the set actually has.
        for (name, _) in GENERIC {
            assert!(draw(name, None, 32).is_some(), "{name}");
        }
        for (name, _, _) in CLASSES {
            assert!(draw(name, None, 32).is_some(), "{name}");
        }
    }

    /// A colour nobody has heard of is still an icon; it is the grey one.
    #[test]
    fn an_unknown_colour_is_the_colour_of_everything_else() {
        assert_eq!(ink(Some("burnt-sienna")), ink(None));
        assert_eq!(ink(Some("gray")), ink(None));
    }
}
