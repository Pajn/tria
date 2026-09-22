//! Terminal-only colour for tool text. Never rewrites bytes: exports, copying, and
//! searching continue to use the original payload.
use ratatui::{
    style::{Color, Modifier, Style},
    text::Span,
};

pub fn highlight(text: &str, base: Style) -> Vec<Vec<Span<'static>>> {
    // A recovered JSON result can be followed by a provenance note or more logs.
    // Colour only a complete JSON value ending at a line boundary.
    let mut values = serde_json::Deserializer::from_str(text).into_iter::<serde_json::Value>();
    let json_lines = match values.next() {
        Some(Ok(value)) if value.is_object() || value.is_array() => {
            let end = values.byte_offset();
            let tail = text[end..].trim_start_matches([' ', '\t', '\r']);
            if tail.is_empty() || tail.starts_with('\n') {
                text[..end].lines().count()
            } else {
                0
            }
        }
        _ => 0,
    };
    let lines: Vec<_> = text.lines().collect();
    let mut hunk = None;
    let mut in_diff = false;
    lines
        .iter()
        .enumerate()
        .map(|(index, line)| {
            if index < json_lines {
                return json_line(line, base);
            }
            let header = line.starts_with("diff --git ")
                || (line.starts_with("--- ")
                    && lines
                        .get(index + 1)
                        .is_some_and(|next| next.starts_with("+++ ")));
            let style = if header {
                in_diff = true;
                hunk = None;
                base.fg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else if in_diff
                && line.starts_with("@@ ")
                && let Some(counts) = hunk_counts(line)
            {
                hunk = Some(counts);
                base.fg(Color::Cyan)
            } else if let Some((old, new)) = hunk.as_mut() {
                match line.as_bytes().first() {
                    Some(b'+') if *new > 0 => {
                        *new -= 1;
                        base.fg(Color::Green)
                    }
                    Some(b'-') if *old > 0 => {
                        *old -= 1;
                        base.fg(Color::Red)
                    }
                    Some(b' ') if *old > 0 && *new > 0 => {
                        *old -= 1;
                        *new -= 1;
                        base
                    }
                    _ if line.starts_with("\\ No newline at end of file") => {
                        base.fg(Color::DarkGray)
                    }
                    _ => {
                        hunk = None;
                        in_diff = false;
                        base
                    }
                }
            } else if in_diff && line.starts_with("+++ ") {
                base.fg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else if in_diff
                && [
                    "index ",
                    "old mode ",
                    "new mode ",
                    "new file mode ",
                    "deleted file mode ",
                    "similarity index ",
                    "dissimilarity index ",
                    "rename from ",
                    "rename to ",
                    "copy from ",
                    "copy to ",
                    "Binary files ",
                ]
                .iter()
                .any(|prefix| line.starts_with(prefix))
            {
                base.fg(Color::DarkGray)
            } else {
                in_diff = false;
                base
            };
            vec![Span::styled((*line).to_string(), style)]
        })
        .collect()
}

fn hunk_counts(line: &str) -> Option<(usize, usize)> {
    fn count(range: &str, sign: char) -> Option<usize> {
        let range = range.strip_prefix(sign)?;
        let (start, count) = range.split_once(',').unwrap_or((range, "1"));
        start.parse::<usize>().ok()?;
        count.parse().ok()
    }
    let mut words = line.split_whitespace();
    if words.next()? != "@@" {
        return None;
    }
    let old = count(words.next()?, '-')?;
    let new = count(words.next()?, '+')?;
    (words.next()? == "@@").then_some((old, new))
}

fn json_line(line: &str, base: Style) -> Vec<Span<'static>> {
    let bytes = line.as_bytes();
    let mut spans = Vec::new();
    let mut pos = 0;
    while pos < bytes.len() {
        let start = pos;
        let color = match bytes[pos] {
            b'"' => {
                pos += 1;
                while pos < bytes.len() {
                    match bytes[pos] {
                        b'\\' => pos = (pos + 2).min(bytes.len()),
                        b'"' => {
                            pos += 1;
                            break;
                        }
                        _ => pos += 1,
                    }
                }
                if line[pos..].trim_start().starts_with(':') {
                    Color::Cyan
                } else {
                    Color::Green
                }
            }
            b'-' | b'0'..=b'9' => {
                pos += 1;
                while pos < bytes.len()
                    && matches!(bytes[pos], b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-')
                {
                    pos += 1;
                }
                Color::Yellow
            }
            b't' | b'f' | b'n' => {
                while pos < bytes.len() && bytes[pos].is_ascii_alphabetic() {
                    pos += 1;
                }
                Color::Magenta
            }
            _ => {
                pos += 1;
                Color::DarkGray
            }
        };
        spans.push(Span::styled(line[start..pos].to_string(), base.fg(color)));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_output_can_be_followed_by_a_recovery_note() {
        let lines = highlight(
            "{\n  \"ok\": true\n}\n\n[Recovered from local Claude transcript]",
            Style::default(),
        );
        assert!(
            lines[1]
                .iter()
                .any(|s| s.content == "true" && s.style.fg == Some(Color::Magenta))
        );
        assert_eq!(lines[4][0].style.fg, None);
        // A partial value, or trailing text on the same line, remains readable.
        let lines = highlight("{\"x\": 1} café", Style::default());
        assert_eq!(lines[0][0].style.fg, None);
        assert_eq!(lines[0][0].content, "{\"x\": 1} café");
    }

    #[test]
    fn mixed_logs_and_diff_color_only_the_patch_and_preserve_text() {
        let text = "Tests 2123 passed\ndiff --git a/a.ts b/a.ts\nindex abc..def 100644\n--- a/a.ts\n+++ b/a.ts\n@@ -1,2 +1,2 @@\n same\n-old\n+new\n+ordinary log after the hunk\nDuration 17.06s";
        let lines = highlight(text, Style::default());
        assert_eq!(lines[0][0].style.fg, None);
        assert_eq!(lines[3][0].style.fg, Some(Color::Cyan));
        assert_eq!(lines[5][0].style.fg, Some(Color::Cyan));
        assert_eq!(lines[7][0].style.fg, Some(Color::Red));
        assert_eq!(lines[8][0].style.fg, Some(Color::Green));
        assert_eq!(lines[9][0].style.fg, None);
        assert_eq!(
            lines
                .iter()
                .map(|spans| spans.iter().map(|s| s.content.as_ref()).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n"),
            text
        );
    }

    #[test]
    fn json_handles_escaped_quotes_unicode_and_types_without_altering_text() {
        let text = r#"{"command": "echo \\\"héj🎵\\\"", "timeout": 1800000, "enabled": true, "value": null}"#;
        let lines = highlight(text, Style::default());
        assert_eq!(
            lines[0]
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>(),
            text
        );
        let color = |token| {
            lines[0]
                .iter()
                .find(|s| s.content == token)
                .unwrap()
                .style
                .fg
        };
        assert_eq!(color("\"command\""), Some(Color::Cyan));
        assert_eq!(color("1800000"), Some(Color::Yellow));
        assert_eq!(color("true"), Some(Color::Magenta));
        assert_eq!(color("null"), Some(Color::Magenta));
    }

    #[test]
    fn ordinary_plus_and_minus_logs_stay_plain_and_new_files_are_highlighted() {
        let lines = highlight(
            "+ready\n-error\n--- /dev/null\n+++ b/new\n@@ -0,0 +1 @@\n+hello",
            Style::default(),
        );
        assert_eq!(lines[0][0].style.fg, None);
        assert_eq!(lines[1][0].style.fg, None);
        assert_eq!(lines[5][0].style.fg, Some(Color::Green));
    }
}
