//! Images attached to a message being written: a file dropped on the terminal, which
//! arrives as a paste of its path, or a picture on the clipboard. Each goes out inline
//! with the message, as the `dataUrl` the server takes, so it is read here, on the
//! machine tria runs on, whichever machine the server is on.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use serde_json::{Value, json};

/// The most the server takes for one image.
pub const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;
/// The most the server takes for all of a message's images together.
pub const MAX_TOTAL_BYTES: usize = 80 * 1024 * 1024;

/// An image to go out with the next message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub name: String,
    pub mime_type: String,
    pub size_bytes: usize,
    /// `data:<mime type>;base64,<bytes>`.
    pub data_url: String,
}

impl Image {
    fn new(name: String, mime_type: &str, bytes: &[u8]) -> Self {
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        Self {
            name,
            mime_type: mime_type.to_string(),
            size_bytes: bytes.len(),
            data_url: format!("data:{mime_type};base64,{encoded}"),
        }
    }

    /// As the server takes it in a message's `attachments`.
    pub fn to_json(&self) -> Value {
        json!({
            "type": "image",
            "name": self.name,
            "mimeType": self.mime_type,
            "sizeBytes": self.size_bytes,
            "dataUrl": self.data_url,
        })
    }

    /// `Screenshot.png · 340 KB`, for the line above the composer.
    pub fn label(&self) -> String {
        format!("{} · {}", self.name, size_label(self.size_bytes))
    }

    /// Read an image file. One the server would refuse as too big is made a JPEG, which
    /// a screenshot nearly always fits as; failing that it is refused here, where it can
    /// be said why.
    pub fn from_path(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "image".to_string());
        let format = image::guess_format(&bytes)
            .map_err(|_| anyhow!("{name} is not an image tria can read"))?;
        let mime_type = format.to_mime_type();
        if bytes.len() <= MAX_IMAGE_BYTES {
            return Ok(Self::new(name, mime_type, &bytes));
        }
        let decoded = image::load_from_memory(&bytes).with_context(|| format!("reading {name}"))?;
        Self::as_jpeg(name, &decoded)
    }

    /// The picture on the clipboard, as a PNG, or nothing when it holds no picture.
    pub fn from_clipboard(clipboard: &mut arboard::Clipboard) -> Result<Option<Self>> {
        let picture = match clipboard.get_image() {
            Ok(picture) => picture,
            Err(arboard::Error::ContentNotAvailable) => return Ok(None),
            Err(error) => return Err(anyhow!("reading the clipboard: {error}")),
        };
        let width = u32::try_from(picture.width)?;
        let height = u32::try_from(picture.height)?;
        let pixels = image::RgbaImage::from_raw(width, height, picture.bytes.into_owned())
            .context("the clipboard's picture has the wrong size")?;
        let decoded = image::DynamicImage::ImageRgba8(pixels);
        let mut png = Vec::new();
        decoded.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)?;
        let name = "Pasted image.png".to_string();
        if png.len() <= MAX_IMAGE_BYTES {
            return Ok(Some(Self::new(name, "image/png", &png)));
        }
        Self::as_jpeg("Pasted image.jpg".to_string(), &decoded).map(Some)
    }

    fn as_jpeg(name: String, decoded: &image::DynamicImage) -> Result<Self> {
        let mut jpeg = Vec::new();
        decoded.to_rgb8().write_to(
            &mut std::io::Cursor::new(&mut jpeg),
            image::ImageFormat::Jpeg,
        )?;
        if jpeg.len() > MAX_IMAGE_BYTES {
            bail!(
                "{name} is {} even as a JPEG; the server takes up to {}",
                size_label(jpeg.len()),
                size_label(MAX_IMAGE_BYTES)
            );
        }
        let name = match name.rsplit_once('.') {
            Some((stem, _)) => format!("{stem}.jpg"),
            None => format!("{name}.jpg"),
        };
        Ok(Self::new(name, "image/jpeg", &jpeg))
    }
}

/// The image files copied to the clipboard, as from Finder or the screenshot preview.
/// Copying a file also puts its icon there as a picture, so the files are what to look
/// for first: the picture is only a picture of the file.
pub fn copied_images(clipboard: &mut arboard::Clipboard) -> Vec<PathBuf> {
    clipboard
        .get()
        .file_list()
        .unwrap_or_default()
        .into_iter()
        .filter(|path| path.is_file() && is_image_name(path))
        .collect()
}

/// The image files a paste names, when that is all it is: what a terminal types for a
/// file dropped on it. Each path is as a shell would take it, with spaces and the like
/// escaped by backslashes or the whole of it quoted, or as a `file://` link. Anything
/// that is not wholly paths to images that are there is text, and is pasted as text.
pub fn dropped_images(text: &str) -> Option<Vec<PathBuf>> {
    let words = shell_words(text.trim())?;
    if words.is_empty() {
        return None;
    }
    words
        .into_iter()
        .map(|word| {
            let path = match word.strip_prefix("file://") {
                Some(rest) => PathBuf::from(percent_decode(rest)?),
                None => PathBuf::from(word),
            };
            (path.is_absolute() && path.is_file() && is_image_name(&path)).then_some(path)
        })
        .collect()
}

fn is_image_name(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .is_some_and(|ext| {
            matches!(
                ext.as_str(),
                "png" | "jpg" | "jpeg" | "gif" | "webp" | "heic" | "tif" | "tiff" | "bmp"
            )
        })
}

/// Split as a shell would: on unescaped blanks, with backslashes escaping the next
/// character and quotes keeping what is inside them together. Nothing for a quote that
/// is never closed.
fn shell_words(text: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => {
                word.push(chars.next()?);
                in_word = true;
            }
            '\'' | '"' => {
                let quote = ch;
                loop {
                    match chars.next()? {
                        c if c == quote => break,
                        '\\' if quote == '"' => word.push(chars.next()?),
                        c => word.push(c),
                    }
                }
                in_word = true;
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            c => {
                word.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(word);
    }
    Some(words)
}

fn percent_decode(text: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(text.len());
    let mut rest = text.as_bytes();
    while let Some((&byte, tail)) = rest.split_first() {
        if byte == b'%' {
            let hex = std::str::from_utf8(tail.get(..2)?).ok()?;
            bytes.push(u8::from_str_radix(hex, 16).ok()?);
            rest = &tail[2..];
        } else {
            bytes.push(byte);
            rest = tail;
        }
    }
    String::from_utf8(bytes).ok()
}

pub fn size_label(bytes: usize) -> String {
    match bytes {
        b if b < 1024 => format!("{b} B"),
        b if b < 1024 * 1024 => format!("{} KB", b / 1024),
        b => format!("{:.1} MB", b as f64 / (1024.0 * 1024.0)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory of its own, gone once the test is.
    struct Dir(PathBuf);

    impl Dir {
        fn new() -> Self {
            let dir =
                std::env::temp_dir().join(format!("tria-attachment-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn png() -> Vec<u8> {
        let mut bytes = Vec::new();
        image::DynamicImage::new_rgb8(2, 2)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        bytes
    }

    /// A file dropped on the terminal arrives as its path, escaped as the shell would
    /// take it; one with spaces in its name is the usual screenshot.
    #[test]
    fn a_dropped_screenshot_is_its_escaped_path() {
        let dir = Dir::new();
        let shot = dir.path().join("Screenshot 2026-10-09 at 20.10.png");
        std::fs::write(&shot, png()).unwrap();
        let escaped = shot.display().to_string().replace(' ', "\\ ");
        assert_eq!(
            dropped_images(&format!("{escaped} ")),
            Some(vec![shot.clone()])
        );
        let quoted = format!("'{}'", shot.display());
        assert_eq!(dropped_images(&quoted), Some(vec![shot.clone()]));
        let link = format!("file://{}", shot.display().to_string().replace(' ', "%20"));
        assert_eq!(dropped_images(&link), Some(vec![shot.clone()]));

        let image = Image::from_path(&shot).unwrap();
        assert_eq!(image.name, "Screenshot 2026-10-09 at 20.10.png");
        assert_eq!(image.mime_type, "image/png");
        assert!(image.data_url.starts_with("data:image/png;base64,"));
        assert_eq!(image.to_json()["type"], "image");
    }

    /// Text that only mentions a path, a path to something that is not an image, and one
    /// to nothing at all are pasted as they are.
    #[test]
    fn anything_but_image_paths_is_text() {
        let dir = Dir::new();
        let shot = dir.path().join("shot.png");
        std::fs::write(&shot, png()).unwrap();
        let notes = dir.path().join("notes.txt");
        std::fs::write(&notes, "hi").unwrap();
        assert_eq!(dropped_images(&format!("look at {}", shot.display())), None);
        assert_eq!(dropped_images(&notes.display().to_string()), None);
        assert_eq!(dropped_images("/nowhere/at/all.png"), None);
        assert_eq!(dropped_images("shot.png"), None, "relative");
        assert_eq!(dropped_images("'unclosed"), None);
    }
}
