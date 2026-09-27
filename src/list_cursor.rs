//! Where the cursor is in one of the lists that stand over the screen for a moment — the
//! terminals, the worktrees, the subagents — and which of it is in view.
//!
//! The rows are the app's, lent per call as the sidebar's are: what each one is, to be
//! found again, and how many lines it is drawn in. What is selected is the row's key rather
//! than its place, so a list that moves under it — a terminal exiting and going to the foot
//! of the live ones — leaves it on the same thing; and when that has gone, it goes to
//! whatever is now nearest where it was.
//!
//! Unlike the sidebar, these lists keep no scroll of their own: what is in view follows the
//! cursor, which is held at the foot of the view once it has gone past it, and is worked
//! out afresh from where the cursor is whenever the list is drawn.

use std::ops::Range;

pub use crate::sidebar_view::Row;

/// The cursor in a list.
#[derive(Debug)]
pub struct ListCursor<K> {
    /// The row selected, by what it is. Nothing until something is, which is the top.
    selected: Option<K>,
    /// Where the selection was last put, for the row nearest it when it has gone.
    at: usize,
}

impl<K> Default for ListCursor<K> {
    fn default() -> Self {
        Self {
            selected: None,
            at: 0,
        }
    }
}

impl<K: Clone + PartialEq> ListCursor<K> {
    /// A cursor on the row at `at`, or on the top of a list that has none there.
    pub fn on(rows: &[Row<K>], at: usize) -> Self {
        let mut cursor = Self::default();
        cursor.put(rows, |_, _| at);
        cursor
    }

    /// The row selected in `rows`: the one it was on, wherever that has gone, else the one
    /// nearest where it was. Nothing only when there are no rows.
    pub fn selection(&self, rows: &[Row<K>]) -> Option<usize> {
        let last = rows.len().checked_sub(1)?;
        let found = self
            .selected
            .as_ref()
            .and_then(|key| rows.iter().position(|row| &row.key == key));
        Some(found.unwrap_or(self.at.min(last)))
    }

    /// Put the cursor on the row `to` says, from the one it is on and the last there is.
    fn put(&mut self, rows: &[Row<K>], to: impl FnOnce(usize, usize) -> usize) {
        let Some(last) = rows.len().checked_sub(1) else {
            return;
        };
        let from = self.selection(rows).unwrap_or(0);
        self.at = to(from, last).min(last);
        self.selected = Some(rows[self.at].key.clone());
    }

    /// `j` and `k`: `by` rows down or up, no further than the list goes.
    pub fn move_by(&mut self, rows: &[Row<K>], by: isize) {
        self.put(rows, |at, _| at.saturating_add_signed(by));
    }

    /// `g`: the first row.
    pub fn top(&mut self, rows: &[Row<K>]) {
        self.put(rows, |_, _| 0);
    }

    /// `G`: the last row.
    pub fn bottom(&mut self, rows: &[Row<K>]) {
        self.put(rows, |_, last| last);
    }

    /// The rows in view in `lines` lines: from the top down to the cursor while it is in
    /// the first screenful, and after that as many as fit above the cursor with the cursor
    /// on the last of them. The row under the cursor is in view however little room there
    /// is. Whatever is still below the cursor fills out the rest.
    pub fn window(&self, rows: &[Row<K>], lines: usize) -> Range<usize> {
        let Some(at) = self.selection(rows) else {
            return 0..0;
        };
        let mut first = at;
        let mut used = rows[at].height;
        while first > 0 && used + rows[first - 1].height <= lines {
            first -= 1;
            used += rows[first].height;
        }
        let mut end = at + 1;
        while end < rows.len() && used + rows[end].height <= lines {
            used += rows[end].height;
            end += 1;
        }
        first..end
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rows of these names, three lines each, as the popups draw them.
    fn rows(names: &[&'static str]) -> Vec<Row<&'static str>> {
        names.iter().map(|&key| Row { key, height: 3 }).collect()
    }

    fn ten() -> Vec<Row<&'static str>> {
        rows(&["r0", "r1", "r2", "r3", "r4", "r5", "r6", "r7", "r8", "r9"])
    }

    fn selected<'a>(cursor: &ListCursor<&'a str>, rows: &[Row<&'a str>]) -> Option<&'a str> {
        cursor.selection(rows).map(|at| rows[at].key)
    }

    #[test]
    fn the_cursor_starts_at_the_top_and_stays_on_the_list() {
        let rows = ten();
        let mut cursor = ListCursor::default();
        assert_eq!(selected(&cursor, &rows), Some("r0"));
        cursor.move_by(&rows, -1);
        assert_eq!(selected(&cursor, &rows), Some("r0"));
        cursor.move_by(&rows, 4);
        assert_eq!(selected(&cursor, &rows), Some("r4"));
        cursor.bottom(&rows);
        cursor.move_by(&rows, 1);
        assert_eq!(selected(&cursor, &rows), Some("r9"));
        cursor.top(&rows);
        assert_eq!(selected(&cursor, &rows), Some("r0"));
        assert_eq!(
            cursor.selection(&[]),
            None,
            "an empty list has nothing selected"
        );
    }

    #[test]
    fn a_cursor_can_start_anywhere_on_the_list() {
        let rows = ten();
        assert_eq!(selected(&ListCursor::on(&rows, 7), &rows), Some("r7"));
        assert_eq!(selected(&ListCursor::on(&rows, 70), &rows), Some("r9"));
        assert_eq!(
            ListCursor::on(&[] as &[Row<&str>], 3).selection(&ten()),
            Some(0)
        );
    }

    /// In the first screenful the view is the top of the list; past it, the cursor is
    /// held at the foot of the view, moving up as well as down; and the view is never
    /// less than the row under the cursor.
    #[test]
    fn the_view_follows_the_cursor_at_its_foot() {
        let rows = ten();
        let mut cursor = ListCursor::default();
        // Four rows of three lines, and two lines over.
        assert_eq!(cursor.window(&rows, 14), 0..4);
        cursor.move_by(&rows, 3);
        assert_eq!(cursor.window(&rows, 14), 0..4);
        cursor.move_by(&rows, 1);
        assert_eq!(
            cursor.window(&rows, 14),
            1..5,
            "one past the foot is one down"
        );
        cursor.bottom(&rows);
        assert_eq!(cursor.window(&rows, 14), 6..10);
        cursor.move_by(&rows, -1);
        assert_eq!(
            cursor.window(&rows, 14),
            5..9,
            "held at the foot on the way up too"
        );
        assert_eq!(
            cursor.window(&rows, 1),
            8..9,
            "the cursor's row however little room"
        );
        assert_eq!(cursor.window(&rows, 100), 0..10);
    }

    /// The list moving under the cursor, as the terminals do when one exits, leaves it on
    /// the row it was on.
    #[test]
    fn the_cursor_follows_its_row_when_the_list_moves() {
        let mut cursor = ListCursor::default();
        cursor.move_by(&rows(&["a", "b", "c"]), 1);
        let moved = rows(&["a", "c", "b"]);
        assert_eq!(selected(&cursor, &moved), Some("b"));
        cursor.move_by(&moved, -1);
        assert_eq!(selected(&cursor, &moved), Some("c"));
    }

    /// When the row the cursor was on has gone, it is on the row nearest where it was, and
    /// moves on from there.
    #[test]
    fn a_cursor_whose_row_has_gone_goes_to_the_nearest() {
        let mut cursor = ListCursor::default();
        cursor.move_by(&rows(&["a", "b", "c"]), 1);
        let fewer = rows(&["a", "c"]);
        assert_eq!(selected(&cursor, &fewer), Some("c"));
        cursor.move_by(&fewer, -1);
        assert_eq!(selected(&cursor, &fewer), Some("a"));
    }

    /// A list that shrinks under the cursor leaves it on its last row, and the view with it.
    #[test]
    fn a_list_that_shrinks_keeps_the_cursor_on_it() {
        let mut cursor = ListCursor::default();
        cursor.bottom(&ten());
        let fewer = rows(&["r0", "r1", "r2", "r3", "r4"]);
        assert_eq!(selected(&cursor, &fewer), Some("r4"));
        assert_eq!(cursor.window(&fewer, 12), 1..5);
        cursor.move_by(&fewer, -1);
        assert_eq!(selected(&cursor, &fewer), Some("r3"));
    }
}
