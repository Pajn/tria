//! Markdown spans with their destinations, retained until the chat is laid out.

use ratatui::{
    style::{Color, Modifier, Style},
    text,
};
use std::{
    ops::{Deref, DerefMut},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tui_markdown::StyleSheet;

#[derive(Clone, Debug, PartialEq)]
pub struct Span {
    pub span: text::Span<'static>,
    pub url: Option<String>,
    pub hidden: bool,
}
impl Deref for Span {
    type Target = text::Span<'static>;
    fn deref(&self) -> &Self::Target {
        &self.span
    }
}
impl DerefMut for Span {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.span
    }
}
impl From<text::Span<'static>> for Span {
    fn from(span: text::Span<'static>) -> Self {
        Self {
            span,
            url: None,
            hidden: false,
        }
    }
}
impl Span {
    pub fn styled(content: impl Into<std::borrow::Cow<'static, str>>, style: Style) -> Self {
        text::Span::styled(content, style).into()
    }
    pub fn raw(content: impl Into<std::borrow::Cow<'static, str>>) -> Self {
        text::Span::raw(content).into()
    }
    pub fn piece(&self, content: impl Into<std::borrow::Cow<'static, str>>) -> Self {
        Self {
            span: text::Span::styled(content, self.style),
            url: self.url.clone(),
            hidden: self.hidden,
        }
    }
}
#[derive(Clone, Debug, Default)]
pub struct Line {
    pub spans: Vec<Span>,
    pub style: Style,
    pub alignment: Option<ratatui::layout::Alignment>,
}
impl From<Vec<Span>> for Line {
    fn from(spans: Vec<Span>) -> Self {
        Self {
            spans,
            ..Self::default()
        }
    }
}
impl From<text::Line<'static>> for Line {
    fn from(line: text::Line<'static>) -> Self {
        Self {
            spans: line.spans.into_iter().map(Into::into).collect(),
            style: line.style,
            alignment: line.alignment,
        }
    }
}
impl std::fmt::Display for Line {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for span in &self.spans {
            f.write_str(&span.content)?;
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Default)]
pub struct Text {
    pub lines: Vec<Line>,
}
impl From<text::Text<'static>> for Text {
    fn from(text: text::Text<'static>) -> Self {
        Self {
            lines: text.lines.into_iter().map(Into::into).collect(),
        }
    }
}
/// A destination over bytes of one rendered text line, before chat wrapping.
#[derive(Clone, Debug)]
pub struct Link {
    pub line: usize,
    pub start: usize,
    pub end: usize,
    pub url: String,
}
impl Text {
    pub fn into_parts(self) -> (text::Text<'static>, Vec<Link>) {
        let mut links: Vec<Link> = Vec::new();
        let lines = self
            .lines
            .into_iter()
            .enumerate()
            .map(|(line, linked)| {
                let mut byte = 0;
                let spans = linked
                    .spans
                    .into_iter()
                    .filter(|s| !s.hidden)
                    .map(|part| {
                        let end = byte + part.content.len();
                        if let Some(url) = part.url.filter(|_| end > byte) {
                            if let Some(last) = links.last_mut().filter(|last| {
                                last.line == line && last.end == byte && last.url == url
                            }) {
                                last.end = end;
                            } else {
                                links.push(Link {
                                    line,
                                    start: byte,
                                    end,
                                    url,
                                });
                            }
                        }
                        byte = end;
                        part.span
                    })
                    .collect();
                text::Line {
                    spans,
                    style: linked.style,
                    alignment: linked.alignment,
                }
            })
            .collect::<Vec<_>>();
        (text::Text::from(lines), links)
    }
}

// tui-markdown offers styles but no link metadata. Give each link a temporary background
// while it renders, then immediately replace that tag with a destination on the span.
// None of these colours reaches the terminal. The renderer consults link() twice per
// link: on entering the label and when appending its destination.
#[derive(Clone, Default)]
struct Links {
    calls: Arc<AtomicUsize>,
}
fn marker(index: usize) -> Color {
    Color::Rgb(1, (index >> 8) as u8, index as u8)
}
fn index(style: Style) -> Option<usize> {
    match style.bg {
        Some(Color::Rgb(1, hi, lo)) if style.add_modifier.contains(Modifier::UNDERLINED) => {
            Some((hi as usize) << 8 | lo as usize)
        }
        _ => None,
    }
}
impl StyleSheet for Links {
    fn link(&self) -> Style {
        let call = self.calls.fetch_add(1, Ordering::Relaxed);
        Style::default()
            .fg(Color::Blue)
            .bg(marker(call / 2))
            .add_modifier(Modifier::UNDERLINED)
    }
    fn code(&self) -> Style {
        let calls = self.calls.load(Ordering::Relaxed);
        let style = tui_markdown::DefaultStyleSheet.code();
        if calls % 2 == 1 {
            style
                .bg(marker(calls / 2))
                .add_modifier(Modifier::UNDERLINED)
        } else {
            style
        }
    }
}

pub fn render(source: &str) -> Text {
    let options = tui_markdown::Options::new(Links::default());
    let rendered = tui_markdown::from_str_with_options(source, &options);
    let mut text = Text {
        lines: rendered
            .lines
            .into_iter()
            .map(|line| Line {
                spans: line
                    .spans
                    .into_iter()
                    .map(|span| {
                        text::Span {
                            content: span.content.into_owned().into(),
                            style: span.style,
                        }
                        .into()
                    })
                    .collect(),
                style: line.style,
                alignment: line.alignment,
            })
            .collect(),
    };
    let mut destinations = std::collections::HashMap::new();
    for line in text.lines.iter_mut().rev() {
        for at in (1..line.spans.len().saturating_sub(1)).rev() {
            let Some(id) = index(line.spans[at].style) else {
                continue;
            };
            if destinations.contains_key(&id) {
                continue;
            }
            if line.spans[at - 1].content == " (" && line.spans[at + 1].content == ")" {
                destinations.insert(id, line.spans[at].content.to_string());
                for span in &mut line.spans[at - 1..=at + 1] {
                    span.hidden = true;
                }
            }
        }
    }
    for line in &mut text.lines {
        let ids: Vec<_> = line.spans.iter().map(|span| index(span.style)).collect();
        for at in 0..line.spans.len() {
            let id = ids[at].or_else(|| {
                if at > 0 && at + 1 < line.spans.len() && line.spans[at].content == " " {
                    let before = ids[at - 1]?;
                    (Some(before) == ids[at + 1]).then_some(before)
                } else {
                    None
                }
            });
            if let Some(id) = id {
                let span = &mut line.spans[at];
                span.url = destinations.get(&id).cloned();
                span.style.bg = (span.style.fg == Some(Color::White)).then_some(Color::Black);
            }
        }
    }
    // Tables measured the appended destinations. Rebuild their cells before discarding
    // those spans, so borders and padding are measured from the labels alone.
    crate::table::fit_linked(&mut text, u16::MAX);
    for line in &mut text.lines {
        line.spans.retain(|span| !span.hidden);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(source: &str) -> (Vec<String>, Vec<(String, String)>) {
        let (text, links) = render(source).into_parts();
        let lines: Vec<_> = text.lines.iter().map(ToString::to_string).collect();
        let labels = links
            .into_iter()
            .map(|link| (lines[link.line][link.start..link.end].to_string(), link.url))
            .collect();
        (lines, labels)
    }

    #[test]
    fn labels_keep_their_own_destinations_and_formatting() {
        let source = "[**same**](https://one.example) and [same](https://two.example)";
        let (lines, links) = labels(source);
        assert_eq!(lines, ["same and same"]);
        assert_eq!(
            links,
            [
                ("same".into(), "https://one.example".into()),
                ("same".into(), "https://two.example".into())
            ]
        );
        let text = render(source).into_parts().0;
        assert!(
            text.lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert!(text.lines[0].spans.iter().all(|s| s.style.bg.is_none()));
    }

    #[test]
    fn reference_autolinks_code_math_and_breaks_keep_their_targets() {
        let (lines, links) = labels(
            "[ref][r] <https://auto.example> [`code` and $math$\nlabel](https://label.example)\n\n[r]: https://reference.example",
        );
        assert_eq!(lines, ["ref https://auto.example code and $math$ label"]);
        assert_eq!(
            links,
            [
                ("ref".into(), "https://reference.example".into()),
                ("https://auto.example".into(), "https://auto.example".into()),
                (
                    "code and $math$ label".into(),
                    "https://label.example".into()
                )
            ]
        );
    }

    #[test]
    fn literal_markdown_and_bare_urls_stay_visible() {
        let (lines, links) = labels("`[literal](https://example.com)` https://bare.example");
        assert!(lines[0].contains("[literal](https://example.com)"));
        assert!(lines[0].contains("https://bare.example"));
        assert!(links.is_empty());
    }

    #[test]
    fn table_borders_are_measured_from_labels_and_targets_survive_reflow() {
        let source =
            "| Link |\n| --- |\n| [a long **label**](https://example.com/a/very/long/path) |";
        let mut text = render(source);
        crate::table::fit_linked(&mut text, 12);
        let (text, links) = text.into_parts();
        let lines: Vec<_> = text.lines.iter().map(ToString::to_string).collect();
        assert!(
            lines
                .iter()
                .all(|line| unicode_width::UnicodeWidthStr::width(line.as_str()) <= 12),
            "{lines:?}"
        );
        assert!(lines.iter().all(|line| !line.contains("https://")));
        let labels: Vec<_> = links
            .iter()
            .map(|link| lines[link.line][link.start..link.end].to_string())
            .collect();
        assert_eq!(labels, ["a long", "label"]);
        assert!(
            links
                .iter()
                .all(|link| link.url == "https://example.com/a/very/long/path")
        );
    }
}
