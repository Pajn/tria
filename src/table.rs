//! Tables narrowed to the width they are drawn in.
//!
//! `tui_markdown` lays a table out at the width of its widest cells, and a table wider than
//! the view is then wrapped like any other line: every row breaks across the border and the
//! box falls apart. Here each such table is laid out again after rendering. Its columns
//! shrink until it fits, and the text wraps inside its cells, which grow taller instead.

use ratatui::{
    style::Style,
    text::{Line, Span, Text},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const VERTICAL: &str = "│";

/// A column is not narrowed past its longest word where there is room for that, so a short
/// column keeps its words whole; this bounds what counts as a word for that.
const WORD_FLOOR: usize = 12;

/// Lay out again every table in `text` that is wider than `width`.
pub fn fit(text: &mut Text<'static>, width: u16) {
    let width = width as usize;
    let lines = std::mem::take(&mut text.lines);
    let mut out = Vec::with_capacity(lines.len());
    let mut lines = lines.into_iter();
    while let Some(line) = lines.next() {
        let Some(top) = border_at(&line, '┌') else {
            out.push(line);
            continue;
        };
        let mut table = vec![line];
        for next in lines.by_ref() {
            let last = border_at(&next, '└').is_some();
            table.push(next);
            if last {
                break;
            }
        }
        match Table::read(&table, top) {
            Some(parsed) if parsed.width() > width => out.extend(parsed.draw(width, table)),
            _ => out.extend(table),
        }
    }
    text.lines = out;
}

/// The span a border line opening with `corner` is drawn in: a table's borders are each one
/// span, after whatever a list or quote puts before them.
fn border_at(line: &Line, corner: char) -> Option<usize> {
    line.spans.iter().position(|span| {
        let mut chars = span.content.chars();
        chars.next() == Some(corner)
            && chars.clone().count() >= 2
            && chars.all(|c| matches!(c, '─' | '┬' | '┼' | '┴' | '┐' | '┤' | '┘'))
    })
}

#[derive(Clone, Copy, PartialEq)]
enum Align {
    Left,
    Center,
    Right,
}

struct Cell {
    spans: Vec<Span<'static>>,
    /// What its padding is drawn in, the header's style or a cell's.
    pad: Style,
}

enum Row {
    /// A line across the table, with the glyphs it is drawn in.
    Border {
        at: usize,
        glyphs: [char; 3],
    },
    Cells {
        at: usize,
        cells: Vec<Cell>,
    },
}

struct Table {
    widths: Vec<usize>,
    aligns: Vec<Align>,
    rows: Vec<Row>,
    /// How much a list or quote puts before each line.
    indent: usize,
    border: Style,
}

impl Table {
    /// Read a table back out of the lines it was rendered to: the top border gives the
    /// columns' widths, and each row is cut at them.
    fn read(lines: &[Line<'static>], top: usize) -> Option<Table> {
        let first = &lines[0].spans[top];
        let widths: Vec<usize> = first
            .content
            .trim_start_matches('┌')
            .trim_end_matches('┐')
            .split('┬')
            .map(|segment| segment.chars().count().checked_sub(2))
            .collect::<Option<_>>()?;
        let indent = lines[0].spans[..top].iter().map(Span::width).sum();
        let mut aligns: Vec<Option<Align>> = vec![None; widths.len()];
        let mut rows = Vec::with_capacity(lines.len());
        for line in lines {
            if let Some(at) = ['┌', '├', '└']
                .into_iter()
                .find_map(|corner| border_at(line, corner))
            {
                let content = &line.spans[at].content;
                let mut chars = content.chars();
                let left = chars.next()?;
                let right = chars.next_back()?;
                let middle = chars.find(|c| *c != '─').unwrap_or('─');
                rows.push(Row::Border {
                    at,
                    glyphs: [left, middle, right],
                });
                continue;
            }
            let at = line.spans.iter().position(|s| s.content == VERTICAL)?;
            let mut spans = line.spans[at + 1..].iter();
            let mut cells = Vec::with_capacity(widths.len());
            for (column, &width) in widths.iter().enumerate() {
                // Each cell is its padding, what it says, its padding, and a border, over
                // exactly the width of the column and a space either side.
                let mut taken = Vec::new();
                let mut reach = 0;
                while reach < width + 2 {
                    let span = spans.next()?;
                    reach += span.width();
                    taken.push(span.clone());
                }
                if reach != width + 2 || spans.next()?.content != VERTICAL || taken.len() < 2 {
                    return None;
                }
                let right = taken.pop()?;
                let left = taken.remove(0);
                let (before, after) = (left.width() - 1, right.width() - 1);
                if aligns[column].is_none() && before + after > 0 {
                    aligns[column] = Some(match (before, after) {
                        (0, _) => Align::Left,
                        (_, 0) => Align::Right,
                        _ => Align::Center,
                    });
                }
                cells.push(Cell {
                    spans: taken,
                    pad: left.style,
                });
            }
            rows.push(Row::Cells { at, cells });
        }
        let border = lines[0].spans[top].style;
        Some(Table {
            aligns: aligns
                .into_iter()
                .map(|a| a.unwrap_or(Align::Left))
                .collect(),
            widths,
            rows,
            indent,
            border,
        })
    }

    fn width(&self) -> usize {
        self.indent + outline(&self.widths)
    }

    /// The table drawn again in `width`, from the lines it was read from, which still hold
    /// what goes before each of them.
    fn draw(&self, width: usize, lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
        let Some(widths) = self.narrowed(width.saturating_sub(self.indent)) else {
            return lines;
        };
        let mut out = Vec::with_capacity(lines.len());
        for (row, line) in self.rows.iter().zip(lines) {
            match row {
                Row::Border { at, glyphs } => {
                    let mut spans = line.spans[..*at].to_vec();
                    spans.push(Span::styled(rule(&widths, *glyphs), self.border));
                    out.push(Line::from(spans));
                }
                Row::Cells { at, cells } => {
                    let before = &line.spans[..*at];
                    let wrapped: Vec<Vec<Vec<Span<'static>>>> = cells
                        .iter()
                        .zip(&widths)
                        .map(|(cell, &width)| wrap(&cell.spans, width))
                        .collect();
                    let height = wrapped.iter().map(Vec::len).max().unwrap_or(1).max(1);
                    for n in 0..height {
                        let mut spans = before.to_vec();
                        spans.push(Span::styled(VERTICAL, self.border));
                        for (column, cell) in cells.iter().enumerate() {
                            let content = wrapped[column].get(n).cloned().unwrap_or_default();
                            let used: usize = content.iter().map(Span::width).sum();
                            let room = widths[column].saturating_sub(used);
                            let (left, right) = match self.aligns[column] {
                                Align::Left => (0, room),
                                Align::Right => (room, 0),
                                Align::Center => (room / 2, room - room / 2),
                            };
                            spans.push(Span::styled(" ".repeat(left + 1), cell.pad));
                            spans.extend(content);
                            spans.push(Span::styled(" ".repeat(right + 1), cell.pad));
                            spans.push(Span::styled(VERTICAL, self.border));
                        }
                        out.push(Line::from(spans));
                    }
                }
            }
        }
        out
    }

    /// The columns' widths for a table that has `room`, or `None` where not even a column
    /// of one fits. Each column keeps its longest word where it can, and gives up the same
    /// share of whatever it has past that.
    fn narrowed(&self, room: usize) -> Option<Vec<usize>> {
        let count = self.widths.len();
        let budget = room.checked_sub(outline(&vec![0; count]))?;
        if budget < count {
            return None;
        }
        let mut floors: Vec<usize> = (0..count)
            .map(|column| self.longest_word(column).clamp(1, WORD_FLOOR))
            .zip(&self.widths)
            .map(|(word, &width)| word.min(width))
            .collect();
        if floors.iter().sum::<usize>() > budget {
            floors = vec![1; count];
        }
        let slack: Vec<usize> = self
            .widths
            .iter()
            .zip(&floors)
            .map(|(w, f)| w - f)
            .collect();
        let total: usize = slack.iter().sum();
        let spare = budget - floors.iter().sum::<usize>();
        let mut widths: Vec<usize> = floors
            .iter()
            .zip(&slack)
            .map(|(floor, slack)| floor + slack * spare / total.max(1))
            .collect();
        // What rounding down left over goes to the columns it was taken from most.
        let mut left = budget.saturating_sub(widths.iter().sum());
        let mut order: Vec<usize> = (0..count).collect();
        order.sort_by_key(|&c| std::cmp::Reverse((slack[c] * spare) % total.max(1)));
        for column in order.into_iter().cycle().take(count * 2) {
            if left == 0 {
                break;
            }
            if widths[column] < self.widths[column] {
                widths[column] += 1;
                left -= 1;
            }
        }
        Some(widths)
    }

    fn longest_word(&self, column: usize) -> usize {
        self.rows
            .iter()
            .filter_map(|row| match row {
                Row::Cells { cells, .. } => cells.get(column),
                Row::Border { .. } => None,
            })
            .flat_map(|cell| words(&cell.spans))
            .map(|word| word.iter().map(|(text, _)| text.width()).sum::<usize>())
            .max()
            .unwrap_or(0)
    }
}

/// How wide a table's borders and padding make it beyond what its columns say.
fn outline(widths: &[usize]) -> usize {
    widths.iter().map(|w| w + 3).sum::<usize>() + 1
}

fn rule(widths: &[usize], [left, middle, right]: [char; 3]) -> String {
    let mut out = String::from(left);
    for (n, width) in widths.iter().enumerate() {
        out.extend(std::iter::repeat_n('─', width + 2));
        if n + 1 < widths.len() {
            out.push(middle);
        }
    }
    out.push(right);
    out
}

type Word = Vec<(String, Style)>;

/// What a cell says, cut into words, each in the pieces of the spans it came from so the
/// styles and the spans' own boundaries survive.
fn words(spans: &[Span<'static>]) -> Vec<Word> {
    let mut out = Vec::new();
    let mut word: Word = Vec::new();
    for span in spans {
        let mut piece = String::new();
        for c in span.content.chars() {
            if c.is_whitespace() {
                if !piece.is_empty() {
                    word.push((std::mem::take(&mut piece), span.style));
                }
                if !word.is_empty() {
                    out.push(std::mem::take(&mut word));
                }
            } else {
                piece.push(c);
            }
        }
        if !piece.is_empty() {
            word.push((piece, span.style));
        }
    }
    if !word.is_empty() {
        out.push(word);
    }
    out
}

/// A cell's text wrapped at `width`, a line of spans each. Words go whole where they fit
/// on a line of their own, and are broken where they do not.
fn wrap(spans: &[Span<'static>], width: usize) -> Vec<Vec<Span<'static>>> {
    let mut lines: Vec<Vec<Span<'static>>> = Vec::new();
    let mut line: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    for word in words(spans) {
        let size: usize = word.iter().map(|(text, _)| text.width()).sum();
        if used > 0 && used + 1 + size > width {
            lines.push(std::mem::take(&mut line));
            used = 0;
        }
        if used > 0 {
            let style = line.last().map(|s| s.style).unwrap_or_default();
            line.push(Span::styled(" ", style));
            used += 1;
        }
        for (text, style) in word {
            if used + text.width() <= width {
                used += text.width();
                line.push(Span::styled(text, style));
                continue;
            }
            let mut piece = String::new();
            for c in text.chars() {
                let size = c.width().unwrap_or(0);
                if used + size > width && used > 0 {
                    if !piece.is_empty() {
                        line.push(Span::styled(std::mem::take(&mut piece), style));
                    }
                    lines.push(std::mem::take(&mut line));
                    used = 0;
                }
                piece.push(c);
                used += size;
            }
            if !piece.is_empty() {
                line.push(Span::styled(piece, style));
            }
        }
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fitted(source: &str, width: u16) -> Vec<String> {
        let mut text = crate::timeline::markdown(source);
        fit(&mut text, width);
        text.lines.iter().map(|l| l.to_string()).collect()
    }

    fn widths(lines: &[String]) -> Vec<usize> {
        lines.iter().map(|l| l.width()).collect()
    }

    const WIDE: &str = "| Name | What it does |\n| --- | --- |\n| fit | Lays out again every table that is wider than the view it is drawn in |\n";

    #[test]
    fn a_table_that_fits_is_left_as_it_was() {
        let before: Vec<String> = crate::timeline::markdown(WIDE)
            .lines
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert_eq!(fitted(WIDE, 200), before);
    }

    #[test]
    fn a_wide_table_shrinks_to_fit_and_wraps_inside_its_cells() {
        let lines = fitted(WIDE, 40);
        assert!(widths(&lines).iter().all(|w| *w == 40), "{lines:#?}");
        assert_eq!(lines[0], "┌──────┬───────────────────────────────┐");
        assert_eq!(lines[1], "│ Name │ What it does                  │");
        assert_eq!(lines[3], "│ fit  │ Lays out again every table    │");
        assert_eq!(lines[4], "│      │ that is wider than the view   │");
        assert_eq!(lines[5], "│      │ it is drawn in                │");
        assert_eq!(lines[6], "└──────┴───────────────────────────────┘");
    }

    #[test]
    fn every_wide_column_gives_up_the_same_share() {
        let source = "| a | b |\n| - | - |\n| one two three four five six seven eight | one two three four |\n";
        let lines = fitted(source, 40);
        assert!(widths(&lines).iter().all(|w| *w <= 40), "{lines:#?}");
        let columns: Vec<usize> = lines[0]
            .trim_matches(['┌', '┐'])
            .split('┬')
            .map(|s| s.chars().count() - 2)
            .collect();
        // 39 and 18 wide with room for 33: past their five-letter floors, 34 and 13 to give
        // and 23 to keep, so each keeps about half of what it had past its floor.
        assert_eq!(columns, vec![22, 11]);
    }

    #[test]
    fn a_word_longer_than_its_column_is_broken() {
        let source =
            "| x | y |\n| - | - |\n| a | https://example.com/a/very/long/path/that/goes/on |\n";
        let lines = fitted(source, 30);
        assert!(widths(&lines).iter().all(|w| *w == 30), "{lines:#?}");
        let joined: String = lines[3..lines.len() - 1]
            .iter()
            .map(|l| l.split('│').nth(2).unwrap().trim())
            .collect();
        assert_eq!(joined, "https://example.com/a/very/long/path/that/goes/on");
    }

    #[test]
    fn alignment_and_styles_survive() {
        let source =
            "| n | said |\n| --: | --- |\n| 12 | **bold words** and more words here |\n| 3 | x |\n";
        let mut text = crate::timeline::markdown(source);
        fit(&mut text, 24);
        let lines: Vec<String> = text.lines.iter().map(|l| l.to_string()).collect();
        assert!(widths(&lines).iter().all(|w| *w == 24), "{lines:#?}");
        assert!(lines.iter().any(|l| l.starts_with("│  3 │")), "{lines:#?}");
        let bold = text
            .lines
            .iter()
            .flat_map(|l| &l.spans)
            .find(|s| s.content == "bold")
            .unwrap();
        assert!(
            bold.style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)
        );
    }

    #[test]
    fn a_table_in_a_list_keeps_its_indent() {
        let source = format!("- item\n\n  {}", WIDE.replace('\n', "\n  "));
        let lines = fitted(&source, 40);
        let table: Vec<&String> = lines.iter().filter(|l| l.contains('│')).collect();
        assert!(!table.is_empty());
        assert!(widths(&lines).iter().all(|w| *w <= 40), "{lines:#?}");
        assert!(table.iter().all(|l| l.starts_with("  │")), "{lines:#?}");
    }

    #[test]
    fn a_view_too_narrow_for_any_column_leaves_the_table_alone() {
        let before: Vec<String> = crate::timeline::markdown(WIDE)
            .lines
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert_eq!(fitted(WIDE, 6), before);
    }
}
