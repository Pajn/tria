//! The sidebar's list as it is read: the tab it is on, the row selected there, how far it
//! is scrolled, and where each of the other tabs was left to come back to.
//!
//! The rows are the app's, lent per call as the chat's blocks are: what each one is, to be
//! found again, and how many lines it is drawn in. The view knows nothing of threads or pull
//! requests. What is selected is the row's key rather than its place in the list, so a list
//! that moves under it — a thread started above it, or settled out of the section it was in
//! — leaves it on the same thing; and when that thing has gone from the list, it goes to
//! whatever is now nearest where it was.
//!
//! Anything that moves the list first settles it on the rows it is lent, so a key or a click
//! reads the list as it is now rather than as it was when it was last drawn. Painting only
//! reads: it is told the same offset and selection the next key will find.

/// A row as the view needs it: what it is, and how many lines it is drawn in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row<K> {
    pub key: K,
    pub height: usize,
}

/// What the sidebar lists: every thread, or the open thread's pull requests or subagents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    #[default]
    Threads,
    PullRequests,
    Agents,
}

impl Tab {
    pub const ALL: [Tab; 3] = [Self::Threads, Self::PullRequests, Self::Agents];

    fn index(self) -> usize {
        Self::ALL.iter().position(|tab| *tab == self).unwrap_or(0)
    }
}

/// Where a tab was left: what was selected, where it was then, and how far it was scrolled.
#[derive(Debug, Clone)]
struct Place<K> {
    selected: Option<K>,
    at: usize,
    offset: usize,
}

impl<K> Default for Place<K> {
    fn default() -> Self {
        Self {
            selected: None,
            at: 0,
            offset: 0,
        }
    }
}

/// The sidebar's list and where the reader is in it.
#[derive(Debug)]
pub struct SidebarView<K> {
    tab: Tab,
    /// The row selected, by what it is. Nothing until something is, which is the top.
    selected: Option<K>,
    /// Where the selection was last found, for the row nearest it when it has gone.
    at: usize,
    /// First row in view, as far as it has been settled.
    offset: usize,
    /// Set by moving the selection, so the list scrolls to show it when it next settles.
    /// Kept while the list is not drawn, as there is no room yet to show it in.
    reveal: bool,
    /// Where each tab was left. The one the list is on is kept here only while it is away.
    places: [Place<K>; 3],
    /// Lines the list is drawn in, told by the frame that draws it; 0 while it is not drawn.
    height: usize,
}

impl<K> Default for SidebarView<K> {
    fn default() -> Self {
        Self {
            tab: Tab::default(),
            selected: None,
            at: 0,
            offset: 0,
            reveal: false,
            places: Default::default(),
            height: 0,
        }
    }
}

impl<K: Clone + PartialEq> SidebarView<K> {
    // ── Laying out ─────────────────────────────────────────────────────

    /// Be told how many lines the list is drawn in, by the frame that draws it, or that it
    /// is not drawn at all.
    pub fn resize(&mut self, height: u16) {
        self.height = height as usize;
    }

    /// Settle the list on `rows`: the selection found again, or the row nearest where it
    /// was, and the list scrolled no further than it goes and, when the selection has just
    /// moved, far enough to show it.
    pub fn settle(&mut self, rows: &[Row<K>]) {
        self.offset = self.offset(rows);
        if let Some(at) = self.selection(rows) {
            self.at = at;
            self.selected = Some(rows[at].key.clone());
        }
        self.reveal &= self.height == 0;
    }

    // ── Where the reader is ────────────────────────────────────────────

    pub fn tab(&self) -> Tab {
        self.tab
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

    /// The first of `rows` in view.
    pub fn offset(&self, rows: &[Row<K>]) -> usize {
        let offset = self.offset.min(self.max_offset(rows));
        match self.selection(rows) {
            Some(at) if self.reveal && self.height > 0 => self.offset_showing(rows, at, offset),
            _ => offset,
        }
    }

    /// The furthest the list can be scrolled: the first row that still leaves every row
    /// after it room to be drawn.
    fn max_offset(&self, rows: &[Row<K>]) -> usize {
        let mut used = 0;
        for (index, row) in rows.iter().enumerate().rev() {
            used += row.height;
            if used > self.height {
                return index + 1;
            }
        }
        0
    }

    /// An offset from `offset` that has row `at` on screen, moving the list as little as
    /// it takes.
    fn offset_showing(&self, rows: &[Row<K>], at: usize, offset: usize) -> usize {
        let mut offset = offset.min(at);
        while offset < at {
            let used: usize = rows[offset..=at].iter().map(|row| row.height).sum();
            if used <= self.height {
                break;
            }
            offset += 1;
        }
        offset
    }

    // ── Tabs ───────────────────────────────────────────────────────────

    /// Go to `tab`, where it was left, keeping where this one was for coming back. Says
    /// whether it is another tab. Nothing is brought into view: which row is marked on
    /// the tab depends on where the keys are, and that is the app's to say.
    pub fn show(&mut self, rows: &[Row<K>], tab: Tab) -> bool {
        if tab == self.tab {
            return false;
        }
        self.settle(rows);
        let leaving = Place {
            selected: self.selected.take(),
            at: self.at,
            offset: self.offset,
        };
        self.places[self.tab.index()] = leaving;
        let place = std::mem::take(&mut self.places[tab.index()]);
        self.selected = place.selected;
        self.at = place.at;
        self.offset = place.offset;
        self.tab = tab;
        true
    }

    /// `H` and `L`: the tab `by` to the left or right, round from one end to the other,
    /// with its selection brought into view.
    pub fn turn(&mut self, rows: &[Row<K>], by: isize) -> bool {
        let count = Tab::ALL.len() as isize;
        let next = (self.tab.index() as isize + by).rem_euclid(count) as usize;
        let turned = self.show(rows, Tab::ALL[next]);
        self.reveal();
        turned
    }

    /// Put `tab` back at its top, as a list that is about something else now starts.
    pub fn start_over(&mut self, tab: Tab) {
        if tab == self.tab {
            self.selected = None;
            self.at = 0;
            self.offset = 0;
            self.reveal = false;
        } else {
            self.places[tab.index()] = Place::default();
        }
    }

    // ── Motions ────────────────────────────────────────────────────────

    /// Bring the selection into view when the list next settles.
    pub fn reveal(&mut self) {
        self.reveal = true;
    }

    /// Select `key` on `tab`: at once, and brought into view, when the list is on it, and
    /// otherwise for when it comes back.
    pub fn select(&mut self, tab: Tab, key: K) {
        if tab == self.tab {
            self.selected = Some(key);
            self.reveal = true;
        } else {
            self.places[tab.index()].selected = Some(key);
        }
    }

    /// Select the row at `at` and bring it into view.
    fn select_row(&mut self, rows: &[Row<K>], at: impl FnOnce(usize, usize) -> usize) {
        self.settle(rows);
        let Some(last) = rows.len().checked_sub(1) else {
            return;
        };
        self.at = at(self.at, last).min(last);
        self.selected = Some(rows[self.at].key.clone());
        self.reveal = true;
    }

    /// `j` and `k`: `by` rows down or up, no further than the list goes.
    pub fn move_by(&mut self, rows: &[Row<K>], by: isize) {
        self.select_row(rows, |at, _| at.saturating_add_signed(by));
    }

    /// `gg`: the first row.
    pub fn top(&mut self, rows: &[Row<K>]) {
        self.select_row(rows, |_, _| 0);
    }

    /// `G`: the last row.
    pub fn bottom(&mut self, rows: &[Row<K>]) {
        self.select_row(rows, |_, last| last);
    }

    /// The wheel: scroll `by` lines' worth of rows, no further than the list goes, and
    /// leave the selection where it is, on the screen or not.
    pub fn scroll(&mut self, rows: &[Row<K>], by: isize) {
        self.settle(rows);
        self.offset = self
            .offset
            .saturating_add_signed(by)
            .min(self.max_offset(rows));
    }

    /// Scroll just enough to have row `at` in view, leaving the selection where it is: what
    /// is marked while the keys are elsewhere is what is open, not what is selected.
    pub fn show_row(&mut self, rows: &[Row<K>], at: usize) {
        self.settle(rows);
        if at < rows.len() {
            self.offset = self.offset_showing(rows, at, self.offset);
        }
    }

    /// A click `line` lines down the list: select the row drawn there, and say which it is.
    pub fn select_at(&mut self, rows: &[Row<K>], line: usize) -> Option<usize> {
        self.settle(rows);
        let mut bottom = 0;
        let at = rows
            .iter()
            .enumerate()
            .skip(self.offset)
            .find_map(|(at, row)| {
                bottom += row.height;
                (line < bottom).then_some(at)
            })?;
        self.at = at;
        self.selected = Some(rows[at].key.clone());
        Some(at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rows of these names, each `height` lines tall.
    fn rows(names: &[&'static str], height: usize) -> Vec<Row<&'static str>> {
        names.iter().map(|&key| Row { key, height }).collect()
    }

    /// Ten one-line rows, `r0` to `r9`, four of which are in view.
    fn ten() -> (SidebarView<&'static str>, Vec<Row<&'static str>>) {
        let mut view = SidebarView::default();
        view.resize(4);
        let rows = rows(
            &["r0", "r1", "r2", "r3", "r4", "r5", "r6", "r7", "r8", "r9"],
            1,
        );
        (view, rows)
    }

    fn selected<'a>(view: &SidebarView<&'a str>, rows: &[Row<&'a str>]) -> Option<&'a str> {
        view.selection(rows).map(|at| rows[at].key)
    }

    #[test]
    fn the_selection_starts_at_the_top_and_stays_on_the_list() {
        let (mut view, rows) = ten();
        assert_eq!(selected(&view, &rows), Some("r0"));
        view.move_by(&rows, -1);
        assert_eq!(selected(&view, &rows), Some("r0"));
        view.move_by(&rows, 3);
        assert_eq!(selected(&view, &rows), Some("r3"));
        view.bottom(&rows);
        assert_eq!(selected(&view, &rows), Some("r9"));
        view.move_by(&rows, 1);
        assert_eq!(selected(&view, &rows), Some("r9"));
        view.top(&rows);
        assert_eq!(selected(&view, &rows), Some("r0"));
        assert_eq!(
            view.selection(&[]),
            None,
            "an empty list has nothing selected"
        );
    }

    /// The list moves only when the selection would leave it, and then by as little as
    /// keeps it in view, either way.
    #[test]
    fn the_list_moves_as_little_as_it_takes_to_show_the_selection() {
        let (mut view, rows) = ten();
        view.move_by(&rows, 3);
        assert_eq!(view.offset(&rows), 0, "still in view");
        view.move_by(&rows, 1);
        assert_eq!(
            view.offset(&rows),
            1,
            "one row past the foot is one row down"
        );
        view.bottom(&rows);
        assert_eq!(view.offset(&rows), 6);
        view.move_by(&rows, -2);
        assert_eq!(view.offset(&rows), 6, "moving up inside the view leaves it");
        view.move_by(&rows, -2);
        assert_eq!(view.offset(&rows), 5);
    }

    /// A row of two lines is shown whole: at the foot of the list, the list moves far
    /// enough for its second line too.
    #[test]
    fn a_two_line_row_is_brought_into_view_whole() {
        let mut view = SidebarView::default();
        view.resize(5);
        let mut rows = rows(&["a", "b", "c", "d"], 2);
        rows.insert(
            0,
            Row {
                key: "head",
                height: 1,
            },
        );
        // head, a, b fit in five lines; c is the fourth row and needs lines six and seven.
        view.move_by(&rows, 2);
        assert_eq!(view.offset(&rows), 0);
        view.move_by(&rows, 1);
        assert_eq!(view.offset(&rows), 2, "c's second line is on the screen");
        view.bottom(&rows);
        assert_eq!(view.offset(&rows), 3, "no further than the list goes");
    }

    /// The wheel scrolls the list and leaves the selection, which may go off the screen,
    /// and goes no further than the list does.
    #[test]
    fn the_wheel_scrolls_the_list_and_not_the_selection() {
        let (mut view, rows) = ten();
        view.move_by(&rows, 1);
        view.scroll(&rows, 3);
        assert_eq!(view.offset(&rows), 3);
        assert_eq!(selected(&view, &rows), Some("r1"));
        view.scroll(&rows, 30);
        assert_eq!(view.offset(&rows), 6);
        view.scroll(&rows, -30);
        assert_eq!(view.offset(&rows), 0);
    }

    /// A click lands on the row drawn where it is, counting the lines each row takes from
    /// the first in view.
    #[test]
    fn a_click_selects_the_row_drawn_there() {
        let mut view = SidebarView::default();
        view.resize(4);
        let mut rows = rows(&["a", "b", "c", "d"], 2);
        rows.insert(
            0,
            Row {
                key: "head",
                height: 1,
            },
        );
        view.scroll(&rows, 1);
        assert_eq!(view.select_at(&rows, 0), Some(1));
        assert_eq!(view.select_at(&rows, 3), Some(2), "b's second line");
        assert_eq!(selected(&view, &rows), Some("b"));
        view.scroll(&rows, 10);
        assert_eq!(view.select_at(&rows, 4), None, "under the last row");
        assert_eq!(
            selected(&view, &rows),
            Some("b"),
            "and nothing was selected"
        );
    }

    /// The list moving under the selection, as the threads do when one starts or settles,
    /// leaves it on the row it was on.
    #[test]
    fn the_selection_follows_its_row_when_the_list_moves() {
        let mut view = SidebarView::default();
        view.resize(10);
        let before = rows(&["head", "b", "a"], 1);
        view.move_by(&before, 2);
        let after = rows(&["head", "c", "b", "a"], 1);
        assert_eq!(selected(&view, &after), Some("a"));
        view.move_by(&after, -1);
        assert_eq!(selected(&view, &after), Some("b"));
    }

    /// When the row selected has gone, the selection is the row nearest where it was, and
    /// that is what it stays on as the list moves after.
    #[test]
    fn a_selection_whose_row_has_gone_goes_to_the_nearest() {
        let mut view = SidebarView::default();
        view.resize(10);
        let before = rows(&["head", "a", "b", "c"], 1);
        view.move_by(&before, 2);
        let after = rows(&["head", "a", "c"], 1);
        assert_eq!(selected(&view, &after), Some("c"));
        view.settle(&after);
        let moved = rows(&["head", "c", "a"], 1);
        assert_eq!(selected(&view, &moved), Some("c"), "c, now it is c");
    }

    /// A list that shrinks under the selection, as one does when a section is folded, has
    /// its last row selected and is scrolled no further than it now goes — and the next
    /// key starts from there, not from where the list used to end.
    #[test]
    fn a_list_that_shrinks_keeps_the_selection_and_the_scroll_on_it() {
        let (mut view, rows) = ten();
        view.bottom(&rows);
        let fewer = super::tests::rows(&["r0", "r1", "r2", "r3", "r4"], 1);
        assert_eq!(selected(&view, &fewer), Some("r4"));
        assert_eq!(view.offset(&fewer), 1);
        view.move_by(&fewer, -1);
        assert_eq!(selected(&view, &fewer), Some("r3"));
    }

    /// Each tab is left where it was and found there again, and `H` and `L` go round.
    #[test]
    fn tabs_keep_their_place() {
        let (mut view, threads) = ten();
        let prs = rows(&["pr1", "pr2"], 1);
        let agents = rows(&["none yet"], 1);
        view.bottom(&threads);
        assert!(view.turn(&threads, 1));
        assert_eq!(view.tab(), Tab::PullRequests);
        assert_eq!(
            selected(&view, &prs),
            Some("pr1"),
            "a tab not yet seen is at its top"
        );
        view.move_by(&prs, 1);
        view.turn(&prs, 1);
        assert_eq!(view.tab(), Tab::Agents);
        view.turn(&agents, 1);
        assert_eq!(view.tab(), Tab::Threads, "round from the last to the first");
        assert_eq!(selected(&view, &threads), Some("r9"));
        assert_eq!(view.offset(&threads), 6);
        view.turn(&threads, -1);
        assert_eq!(view.tab(), Tab::Agents, "and back the other way");
        view.turn(&agents, -1);
        assert_eq!(selected(&view, &prs), Some("pr2"), "back where it was left");
        assert!(!view.show(&prs, Tab::PullRequests), "already there");
    }

    /// A list that is about another thread now starts at its top, whether or not it is
    /// the one showing.
    #[test]
    fn another_thread_starts_its_lists_at_the_top() {
        let (mut view, threads) = ten();
        let prs = rows(&["pr1", "pr2", "pr3", "pr4", "pr5", "pr6"], 1);
        view.show(&threads, Tab::PullRequests);
        view.bottom(&prs);
        view.start_over(Tab::PullRequests);
        assert_eq!(selected(&view, &prs), Some("pr1"));
        assert_eq!(view.offset(&prs), 0);

        view.show(&prs, Tab::Agents);
        view.show(&[], Tab::PullRequests);
        view.bottom(&prs);
        view.show(&prs, Tab::Agents);
        view.start_over(Tab::PullRequests);
        view.show(&[], Tab::PullRequests);
        assert_eq!(selected(&view, &prs), Some("pr1"));
        assert_eq!(view.offset(&prs), 0);
    }

    /// A row selected on a tab the list is not on waits there, and is brought into view
    /// on the way back.
    #[test]
    fn a_row_selected_on_another_tab_is_there_when_it_is_shown() {
        let (mut view, threads) = ten();
        view.show(&threads, Tab::PullRequests);
        view.select(Tab::Threads, "r8");
        view.turn(&[], -1);
        assert_eq!(selected(&view, &threads), Some("r8"));
        assert_eq!(view.offset(&threads), 5);
    }

    /// What is open can be brought into view without being selected, for while the keys
    /// are elsewhere and it is what is marked.
    #[test]
    fn a_row_can_be_shown_without_being_selected() {
        let (mut view, rows) = ten();
        view.show_row(&rows, 7);
        assert_eq!(view.offset(&rows), 4);
        assert_eq!(selected(&view, &rows), Some("r0"));
    }

    /// Bringing the selection into view is done once, when the list settles: a list that
    /// moves the selection out of view after that is left where it is.
    #[test]
    fn the_selection_is_shown_once_and_not_followed() {
        let (mut view, rows) = ten();
        view.select(Tab::Threads, "r2");
        view.settle(&rows);
        assert_eq!(view.offset(&rows), 0);
        let moved = super::tests::rows(
            &["n0", "n1", "n2", "n3", "n4", "r0", "r1", "r2", "r3", "r4"],
            1,
        );
        assert_eq!(selected(&view, &moved), Some("r2"));
        assert_eq!(view.offset(&moved), 0, "the list does not follow it down");
        view.settle(&moved);
        assert_eq!(view.offset(&moved), 0);
    }

    /// A list that is not drawn has no room to show the selection in, so it is shown
    /// once there is.
    #[test]
    fn a_list_not_drawn_shows_the_selection_once_it_is() {
        let (mut view, rows) = ten();
        view.resize(0);
        view.bottom(&rows);
        view.settle(&rows);
        assert_eq!(view.offset(&rows), 0);
        view.resize(4);
        assert_eq!(view.offset(&rows), 6);
    }

    /// Keys and clicks that come before the next frame find the list where the last of
    /// them left it: a click after `G` lands on the rows `G` scrolled to, and the wheel
    /// goes on from there.
    #[test]
    fn the_list_is_settled_for_the_next_key_without_a_frame() {
        let (mut view, rows) = ten();
        view.bottom(&rows);
        assert_eq!(view.select_at(&rows, 0), Some(6));
        view.scroll(&rows, -1);
        assert_eq!(view.offset(&rows), 5);
    }
}
