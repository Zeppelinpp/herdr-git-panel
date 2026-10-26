//! Commit history owns expansion, identity and projection as one state machine.
//! Callers pass Git rows/details and receive actions; display indices stay local.
use crate::{
    git::{FileStat, GraphRow},
    graph::{self, Item},
    theme::NAVIGATION,
};
use ratatui::{
    layout::{Constraint, Rect},
    widgets::{Row, Table, TableState},
    Frame,
};
use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Identity {
    Commit(String),
    File(String, String),
}
impl Identity {
    fn sha(&self) -> &str {
        match self {
            Self::Commit(sha) | Self::File(sha, _) => sha,
        }
    }
}
/// Opaque hit token produced by the current render, never a Git row index.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RowHit(usize);

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Action {
    ReadCommit(String),
    OpenFile { sha: String, path: String },
}
struct Details {
    summary: String,
    files: Vec<FileStat>,
}
struct Projection {
    width: usize,
    valid: bool,
    table: Table<'static>,
    items: Vec<Item>,
    selectable: Vec<usize>,
    commits: Vec<Option<usize>>,
}
impl Default for Projection {
    fn default() -> Self {
        Self {
            width: 80,
            valid: false,
            table: Table::default(),
            items: Vec::new(),
            selectable: Vec::new(),
            commits: Vec::new(),
        }
    }
}
pub(crate) struct History {
    rows: Vec<GraphRow>,
    commits: HashMap<String, usize>,
    details: HashMap<String, Details>,
    folded: bool,
    state: TableState,
    projection: Projection,
    restore: Option<Identity>,
}
impl Default for History {
    fn default() -> Self {
        Self {
            rows: Vec::new(),
            commits: HashMap::new(),
            details: HashMap::new(),
            folded: true,
            state: TableState::default().with_selected(0),
            projection: Projection::default(),
            restore: None,
        }
    }
}
impl History {
    pub(crate) fn count(&self) -> usize {
        self.commits.len()
    }
    pub(crate) fn folded(&self) -> bool {
        self.folded
    }
    pub(crate) fn toggle(&mut self) {
        self.folded = !self.folded;
    }

    /// Replace transport rows without retaining a second copy in the panel.
    /// Reconciliation is O(rows + expanded commits); folded history stays lazy.
    pub(crate) fn replace(&mut self, rows: Vec<GraphRow>) -> bool {
        if self.rows == rows {
            return false;
        }
        let selected = self.selection();
        self.commits = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| !row.sha.is_empty())
            .map(|(index, row)| (row.sha.clone(), index))
            .collect();
        self.details.retain(|sha, _| self.commits.contains_key(sha));
        self.rows = rows;
        self.invalidate(selected);
        true
    }
    pub(crate) fn expand(&mut self, sha: String, summary: String, files: Vec<FileStat>) {
        if !self.commits.contains_key(&sha) {
            return;
        }
        let selected = self
            .selection()
            .or_else(|| Some(Identity::Commit(sha.clone())));
        self.details.insert(sha, Details { summary, files });
        self.invalidate(selected);
    }
    pub(crate) fn selection(&self) -> Option<Identity> {
        self.restore.clone().or_else(|| {
            self.state
                .selected()
                .and_then(|index| self.projection.items.get(index))
                .and_then(|item| self.identity(*item))
        })
    }
    /// Select an exact identity for a deferred action. Missing files are rejected
    /// rather than opening a different file; refresh fallback is separate.
    pub(crate) fn select(&mut self, identity: &Identity) -> bool {
        let Some(item) = self.item(identity) else {
            return false;
        };
        self.restore = Some(identity.clone());
        if self.projection.valid {
            self.state.select(self.projected_index(item));
            self.restore = None;
        }
        true
    }
    pub(crate) fn select_row(&mut self, hit: RowHit) {
        if self.folded {
            return;
        }
        let index = hit.0;
        self.prepare(self.projection.width);
        if self
            .projection
            .items
            .get(index)
            .is_some_and(|item| item.selectable())
        {
            self.state.select(Some(index));
        }
    }
    pub(crate) fn navigate(&mut self, delta: isize) {
        if self.folded {
            return;
        }
        self.prepare(self.projection.width);
        let indices = &self.projection.selectable;
        if indices.is_empty() {
            return;
        }
        let position = indices
            .binary_search(&self.state.selected().unwrap_or(0))
            .unwrap_or_else(|index| index.min(indices.len() - 1));
        let next = position.saturating_add_signed(delta).min(indices.len() - 1);
        self.state.select(Some(indices[next]));
    }
    pub(crate) fn activate(&mut self) -> Option<Action> {
        if self.folded {
            self.folded = false;
            return None;
        }
        self.prepare(self.projection.width);
        match self.selection()? {
            Identity::File(sha, path) => Some(Action::OpenFile { sha, path }),
            Identity::Commit(sha) => {
                if self.details.remove(&sha).is_some() {
                    self.invalidate(Some(Identity::Commit(sha)));
                    None
                } else {
                    Some(Action::ReadCommit(sha))
                }
            }
        }
    }
    /// Draw cached rows and emit only actionable hit rectangles. Table state,
    /// viewport offsets and projected row identities never escape this module.
    pub(crate) fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        active: bool,
        mut hit: impl FnMut(Rect, RowHit),
    ) {
        if self.folded {
            return;
        }
        self.prepare(area.width as usize);
        if active {
            frame.render_stateful_widget(&self.projection.table, area, &mut self.state);
        } else {
            let mut display = self.state.clone();
            *display.selected_mut() = None;
            frame.render_stateful_widget(&self.projection.table, area, &mut display);
            *self.state.offset_mut() = display.offset();
        }
        for y in 0..area.height {
            let index = self.state.offset() + y as usize;
            if self
                .projection
                .items
                .get(index)
                .is_some_and(|item| item.selectable())
            {
                hit(Rect::new(area.x, area.y + y, area.width, 1), RowHit(index));
            }
        }
    }
    fn identity(&self, item: Item) -> Option<Identity> {
        match item {
            Item::Commit(index) => Some(Identity::Commit(self.rows.get(index)?.sha.clone())),
            Item::File(commit, index) => {
                let sha = &self.rows.get(commit)?.sha;
                Some(Identity::File(
                    sha.clone(),
                    self.details.get(sha)?.files.get(index)?.path.clone(),
                ))
            }
            _ => None,
        }
    }
    fn item(&self, identity: &Identity) -> Option<Item> {
        let commit = *self.commits.get(identity.sha())?;
        match identity {
            Identity::Commit(_) => Some(Item::Commit(commit)),
            Identity::File(sha, path) => Some(Item::File(
                commit,
                self.details
                    .get(sha)?
                    .files
                    .iter()
                    .position(|file| file.path == *path)?,
            )),
        }
    }
    fn projected_index(&self, item: Item) -> Option<usize> {
        match item {
            Item::Commit(index) => self.projection.commits.get(index).copied().flatten(),
            _ => self
                .projection
                .items
                .iter()
                .position(|candidate| *candidate == item),
        }
    }
    fn invalidate(&mut self, selected: Option<Identity>) {
        self.restore = selected;
        self.projection.valid = false;
    }
    fn prepare(&mut self, width: usize) {
        if self.projection.valid && self.projection.width == width {
            return;
        }
        let selected = self.selection();
        let expanded = self
            .details
            .iter()
            .map(|(sha, details)| graph::Expanded {
                sha,
                summary: &details.summary,
                files: &details.files,
            })
            .collect::<Vec<_>>();
        let projected = graph::project(&self.rows, &expanded, width);
        let mut items = Vec::with_capacity(projected.len());
        let mut selectable = Vec::new();
        let mut commits = vec![None; self.rows.len()];
        let mut rows = Vec::with_capacity(projected.len());
        for (index, row) in projected.into_iter().enumerate() {
            if row.item.selectable() {
                selectable.push(index);
            }
            if let Item::Commit(raw) = row.item {
                commits[raw] = Some(index);
            }
            items.push(row.item);
            rows.push(Row::new(vec![row.line]));
        }
        self.projection = Projection {
            width,
            valid: true,
            items,
            selectable,
            commits,
            table: Table::new(rows, [Constraint::Percentage(100)])
                .column_spacing(0)
                .row_highlight_style(NAVIGATION),
        };
        let selected = selected
            .as_ref()
            .and_then(|identity| self.item(identity))
            // A file removed from expansion falls back to its owning commit.
            .or_else(|| {
                selected
                    .as_ref()
                    .and_then(|identity| self.commits.get(identity.sha()))
                    .copied()
                    .map(Item::Commit)
            })
            .and_then(|item| self.projected_index(item))
            .or_else(|| self.projection.selectable.first().copied());
        self.state.select(selected);
        self.restore = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    fn rows() -> Vec<GraphRow> {
        vec![
            GraphRow {
                sha: "aaa".into(),
                lanes: "* ".into(),
                subject: "First".into(),
                ..Default::default()
            },
            GraphRow {
                lanes: "| ".into(),
                ..Default::default()
            },
            GraphRow {
                sha: "bbb".into(),
                lanes: "* ".into(),
                subject: "Second".into(),
                ..Default::default()
            },
            GraphRow {
                sha: "ccc".into(),
                lanes: "* ".into(),
                subject: "Third".into(),
                ..Default::default()
            },
        ]
    }
    fn file(path: &str) -> FileStat {
        FileStat {
            path: path.into(),
            added: "2".into(),
            deleted: "1".into(),
        }
    }
    fn history() -> History {
        let mut history = History::default();
        history.replace(rows());
        history.toggle();
        history
    }
    fn draw(
        history: &mut History,
        width: u16,
        height: u16,
        active: bool,
    ) -> (Vec<String>, Vec<(Rect, RowHit)>) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut hits = Vec::new();
        terminal
            .draw(|frame| {
                history.render(frame, frame.area(), active, |rect, index| {
                    hits.push((rect, index))
                })
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let lines = (0..height)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();
        (lines, hits)
    }
    #[test]
    fn multiple_expansions_have_independent_actions_and_navigation_reuses_projection() {
        let mut history = history();
        assert!(history.select(&Identity::Commit("aaa".into())));
        history.expand(
            "aaa".into(),
            "1 file changed".into(),
            vec![file("first.rs")],
        );
        history.navigate(1);
        assert_eq!(
            history.activate(),
            Some(Action::OpenFile {
                sha: "aaa".into(),
                path: "first.rs".into()
            })
        );
        history.navigate(1);
        assert_eq!(history.activate(), Some(Action::ReadCommit("bbb".into())));
        history.expand(
            "bbb".into(),
            "1 file changed".into(),
            vec![file("second.rs")],
        );
        let (lines, _) = draw(&mut history, 60, 20, true);
        assert!(lines.iter().any(|line| line.contains("first.rs")));
        assert!(lines.iter().any(|line| line.contains("second.rs")));
        let projections = graph::projection_count();
        for _ in 0..100 {
            history.navigate(1);
            assert_eq!(
                history.selection(),
                Some(Identity::File("bbb".into(), "second.rs".into()))
            );
            history.navigate(-1);
        }
        assert_eq!(
            graph::projection_count(),
            projections,
            "j/k changes selection only"
        );
        assert_eq!(history.activate(), None); // collapse bbb, retain aaa
        let (lines, _) = draw(&mut history, 60, 20, true);
        assert!(lines.iter().any(|line| line.contains("first.rs")));
        assert!(!lines.iter().any(|line| line.contains("second.rs")));
        assert_eq!(history.selection(), Some(Identity::Commit("bbb".into())));
        history.toggle();
        history.toggle();
        assert!(history.select(&Identity::File("aaa".into(), "first.rs".into())));
    }
    #[test]
    fn row_replacement_restores_file_identity_and_does_not_project_while_folded() {
        let mut history = history();
        history.expand(
            "bbb".into(),
            "1 file changed".into(),
            vec![file("second.rs")],
        );
        assert!(history.select(&Identity::File("bbb".into(), "second.rs".into())));
        draw(&mut history, 60, 20, true);
        history.toggle();
        let mut changed = rows();
        changed.insert(
            0,
            GraphRow {
                sha: "new".into(),
                lanes: "* ".into(),
                ..Default::default()
            },
        );
        let projections = graph::projection_count();
        assert!(history.replace(changed.clone()));
        assert!(!history.replace(changed));
        draw(&mut history, 42, 10, false);
        assert_eq!(graph::projection_count(), projections);
        history.toggle();
        draw(&mut history, 42, 10, true);
        assert_eq!(graph::projection_count(), projections + 1);
        assert_eq!(
            history.activate(),
            Some(Action::OpenFile {
                sha: "bbb".into(),
                path: "second.rs".into()
            })
        );
        assert_eq!(history.count(), 4);
    }
    #[test]
    fn disappearing_file_falls_back_to_owning_commit_and_stale_actions_are_rejected() {
        let mut history = history();
        history.expand("bbb".into(), "1 file changed".into(), vec![file("gone.rs")]);
        let removed = Identity::File("bbb".into(), "gone.rs".into());
        assert!(history.select(&removed));
        draw(&mut history, 60, 20, true);
        history.expand("bbb".into(), "empty".into(), vec![]);
        draw(&mut history, 60, 20, true);
        assert_eq!(history.selection(), Some(Identity::Commit("bbb".into())));
        assert!(
            !history.select(&removed),
            "deferred diff never opens a replacement file"
        );
        history.replace(vec![rows().remove(0)]);
        draw(&mut history, 60, 20, true);
        assert_eq!(history.selection(), Some(Identity::Commit("aaa".into())));
        history.expand("bbb".into(), "stale result".into(), vec![file("stale.rs")]);
        let (lines, _) = draw(&mut history, 60, 20, true);
        assert!(!lines.iter().any(|line| line.contains("stale.rs")));
    }
    #[test]
    fn mouse_rows_skip_metadata_and_resolve_the_owning_commit() {
        let mut history = history();
        history.expand(
            "aaa".into(),
            "1 file changed".into(),
            vec![file("first.rs")],
        );
        history.expand(
            "bbb".into(),
            "1 file changed".into(),
            vec![file("second.rs")],
        );
        let (lines, hits) = draw(&mut history, 60, 20, false);
        assert_eq!(
            hits.len(),
            5,
            "three commits and two files; summaries and rails are not clickable"
        );
        let row = lines
            .iter()
            .position(|line| line.contains("second.rs"))
            .unwrap();
        let (_, index) = hits
            .iter()
            .find(|(rect, _)| rect.y as usize == row)
            .unwrap();
        history.select_row(*index);
        assert_eq!(
            history.activate(),
            Some(Action::OpenFile {
                sha: "bbb".into(),
                path: "second.rs".into()
            })
        );
    }
}
