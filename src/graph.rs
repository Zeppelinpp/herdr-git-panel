//! Compact commit rows modeled on the Orca and lazygit panel references.
//! Commit parents own topology; Git supplies node color identity.
use crate::git::{FileStat, GraphRow};
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};
use std::collections::HashSet;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[cfg(test)]
thread_local! { static PROJECTIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
#[cfg(test)]
pub(crate) fn projection_count() -> usize {
    PROJECTIONS.with(|n| n.get())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Item {
    Commit(usize),
    File(usize, usize),
    Connector,
    Detail,
}
impl Item {
    pub fn selectable(self) -> bool {
        matches!(self, Self::Commit(_) | Self::File(_, _))
    }
}
pub struct DisplayRow {
    pub item: Item,
    pub line: Line<'static>,
}
pub struct Expanded<'a> {
    pub sha: &'a str,
    pub summary: &'a str,
    pub files: &'a [FileStat],
}

const TRACKS: [Color; 6] = [
    Color::Blue,
    Color::Green,
    Color::Magenta,
    Color::Cyan,
    Color::Yellow,
    Color::LightMagenta,
];
fn visible(value: &str) -> String {
    value
        .chars()
        .flat_map(|c| {
            if c.is_control() {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}
pub fn truncate(value: &str, width: usize) -> String {
    if UnicodeWidthStr::width(value) <= width {
        return value.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut result = String::new();
    let mut used = 0;
    for c in value.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > width - 1 {
            break;
        }
        result.push(c);
        used += w;
    }
    result.push('…');
    result
}
// Pipes connect commit parents. Each projected cell records its incoming and
// outgoing directions so joins use rounded corners instead of diagonal glyphs.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum PipeKind {
    Terminates,
    Starts,
    Continues,
}
#[derive(Clone)]
struct Pipe {
    to_hash: String,
    from_pos: usize,
    to_pos: usize,
    kind: PipeKind,
    color: usize,
}
impl Pipe {
    fn left(&self) -> usize {
        self.from_pos.min(self.to_pos)
    }
    fn right(&self) -> usize {
        self.from_pos.max(self.to_pos)
    }
}

const EMPTY_TREE_HASH: &str = "\0empty-tree";

fn node_color(row: &GraphRow) -> usize {
    row.lanes
        .chars()
        .position(|c| c == '*')
        .and_then(|column| row.lane_colors.get(column).copied().flatten())
        .map(usize::from)
        .unwrap_or(0)
        % TRACKS.len()
}
fn track(color: usize) -> Style {
    Style::new().fg(TRACKS[color % TRACKS.len()])
}
fn traverse(from: usize, to: usize, taken: &mut HashSet<usize>, traversed: &mut HashSet<usize>) {
    for position in from.min(to)..=from.max(to) {
        traversed.insert(position);
    }
    taken.insert(to);
}
fn next_pipes(previous: &[Pipe], row: &GraphRow) -> Vec<Pipe> {
    let max_pos = previous.iter().map(|pipe| pipe.to_pos).max().unwrap_or(0);
    let current = previous
        .iter()
        .filter(|pipe| pipe.kind != PipeKind::Terminates)
        .cloned()
        .collect::<Vec<_>>();
    let position = current
        .iter()
        .find(|pipe| pipe.to_hash == row.sha)
        .map_or(max_pos + 1, |pipe| pipe.to_pos);
    let color = node_color(row);
    let first_parent = row
        .parents
        .first()
        .cloned()
        .unwrap_or_else(|| EMPTY_TREE_HASH.to_string());
    let mut next = vec![Pipe {
        to_hash: first_parent,
        from_pos: position,
        to_pos: position,
        kind: PipeKind::Starts,
        color,
    }];
    let mut taken = HashSet::new();
    let mut traversed = HashSet::new();
    let continuing = current
        .iter()
        .filter(|pipe| pipe.to_hash != row.sha)
        .map(|pipe| pipe.to_pos)
        .collect::<HashSet<_>>();

    for pipe in &current {
        if pipe.to_hash == row.sha {
            next.push(Pipe {
                from_pos: pipe.to_pos,
                to_pos: position,
                kind: PipeKind::Terminates,
                ..pipe.clone()
            });
            traverse(pipe.to_pos, position, &mut taken, &mut traversed);
        } else if pipe.to_pos < position {
            let available = (0..)
                .find(|position| !traversed.contains(position))
                .expect("an unoccupied graph position");
            next.push(Pipe {
                from_pos: pipe.to_pos,
                to_pos: available,
                kind: PipeKind::Continues,
                ..pipe.clone()
            });
            traverse(pipe.to_pos, available, &mut taken, &mut traversed);
        }
    }
    for parent in row.parents.iter().skip(1) {
        let available = (0..)
            .find(|position| !taken.contains(position) && !continuing.contains(position))
            .expect("an untraversed graph position");
        next.push(Pipe {
            to_hash: parent.clone(),
            from_pos: position,
            to_pos: available,
            kind: PipeKind::Starts,
            color,
        });
        taken.insert(available);
    }
    for pipe in &current {
        if pipe.to_hash == row.sha || pipe.to_pos <= position {
            continue;
        }
        let mut last = pipe.to_pos;
        for candidate in (position..pipe.to_pos).rev() {
            if taken.contains(&candidate) || traversed.contains(&candidate) {
                break;
            }
            last = candidate;
        }
        next.push(Pipe {
            from_pos: pipe.to_pos,
            to_pos: last,
            kind: PipeKind::Continues,
            ..pipe.clone()
        });
        traverse(pipe.to_pos, last, &mut taken, &mut traversed);
    }
    next.sort_by_key(|pipe| (pipe.to_pos, pipe.kind));
    next
}
fn pipe_sets(rows: &[GraphRow]) -> Vec<Vec<Pipe>> {
    let mut commits = rows.iter().filter(|row| !row.sha.is_empty());
    let Some(first) = commits.next() else {
        return Vec::new();
    };
    let mut output = Vec::new();
    let mut pipes = vec![Pipe {
        to_hash: first.sha.clone(),
        from_pos: 0,
        to_pos: 0,
        kind: PipeKind::Starts,
        color: node_color(first),
    }];
    for row in std::iter::once(first).chain(commits) {
        pipes = next_pipes(&pipes, row);
        output.push(pipes.clone());
    }
    output
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CellKind {
    Connection,
    Commit,
    Merge,
}
struct Cell {
    up: bool,
    down: bool,
    left: bool,
    right: bool,
    kind: CellKind,
    color: usize,
    right_color: Option<usize>,
}
impl Cell {
    fn new() -> Self {
        Self {
            up: false,
            down: false,
            left: false,
            right: false,
            kind: CellKind::Connection,
            color: 0,
            right_color: None,
        }
    }
    fn set_up(&mut self, color: usize) {
        self.up = true;
        self.color = color;
    }
    fn set_down(&mut self, color: usize) {
        self.down = true;
        self.color = color;
    }
    fn set_left(&mut self, color: usize) {
        self.left = true;
        if !self.up && !self.down {
            self.color = color;
        }
    }
    fn set_right(&mut self, color: usize, override_color: bool) {
        self.right = true;
        if self.right_color.is_none() || override_color {
            self.right_color = Some(color);
        }
    }
}
struct Geometry {
    cells: Vec<Cell>,
}
impl Geometry {
    fn new(pipes: &[Pipe], row: &GraphRow) -> Self {
        let max_pos = pipes.iter().map(|pipe| pipe.right()).max().unwrap_or(0);
        let commit_pos = pipes
            .iter()
            .find_map(|pipe| match pipe.kind {
                PipeKind::Starts => Some(pipe.from_pos),
                PipeKind::Terminates => Some(pipe.to_pos),
                PipeKind::Continues => None,
            })
            .unwrap_or(0);
        let merge = pipes
            .iter()
            .filter(|pipe| pipe.kind == PipeKind::Starts)
            .count()
            > 1;
        let mut cells = (0..=max_pos).map(|_| Cell::new()).collect::<Vec<_>>();
        let mut render = |pipe: &Pipe, override_right: bool| {
            let left = pipe.left();
            let right = pipe.right();
            if left != right {
                for cell in &mut cells[left + 1..right] {
                    cell.set_left(pipe.color);
                    cell.set_right(pipe.color, override_right);
                }
                cells[left].set_right(pipe.color, override_right);
                cells[right].set_left(pipe.color);
            }
            if matches!(pipe.kind, PipeKind::Starts | PipeKind::Continues)
                && pipe.to_hash != EMPTY_TREE_HASH
            {
                cells[pipe.to_pos].set_down(pipe.color);
            }
            if matches!(pipe.kind, PipeKind::Terminates | PipeKind::Continues) {
                cells[pipe.from_pos].set_up(pipe.color);
            }
        };
        for pipe in pipes.iter().filter(|pipe| pipe.kind == PipeKind::Starts) {
            render(pipe, true);
        }
        for pipe in pipes.iter().filter(|pipe| {
            pipe.kind != PipeKind::Starts
                && !(pipe.kind == PipeKind::Terminates
                    && pipe.from_pos == commit_pos
                    && pipe.to_pos == commit_pos)
        }) {
            render(pipe, false);
        }
        cells[commit_pos].kind = if merge {
            CellKind::Merge
        } else {
            CellKind::Commit
        };
        cells[commit_pos].color = node_color(row);
        Self { cells }
    }
    fn width(&self) -> usize {
        self.cells.len() * 2
    }
    fn spans(&self, continuation: bool) -> Vec<Span<'static>> {
        let mut spans = Vec::with_capacity(self.cells.len() * 2);
        for cell in &self.cells {
            let (first, second) = if continuation {
                (if cell.down { '│' } else { ' ' }, ' ')
            } else {
                let (connection, second) = box_drawing(cell.up, cell.down, cell.left, cell.right);
                let first = match cell.kind {
                    CellKind::Connection => connection,
                    CellKind::Commit => '●',
                    CellKind::Merge => '○',
                };
                (first, second)
            };
            spans.push(Span::styled(first.to_string(), track(cell.color)));
            spans.push(Span::styled(
                second.to_string(),
                track(cell.right_color.unwrap_or(cell.color)),
            ));
        }
        spans
    }
}
fn box_drawing(up: bool, down: bool, left: bool, right: bool) -> (char, char) {
    match (up, down, left, right) {
        (true, true, true, true) => ('│', '─'),
        (true, true, true, false) => ('│', ' '),
        (true, true, false, true) => ('│', '─'),
        (true, true, false, false) => ('│', ' '),
        (true, false, true, true) => ('┴', '─'),
        (true, false, true, false) => ('╯', ' '),
        (true, false, false, true) => ('╰', '─'),
        (true, false, false, false) => ('╵', ' '),
        (false, true, true, true) => ('┬', '─'),
        (false, true, true, false) => ('╮', ' '),
        (false, true, false, true) => ('╭', '─'),
        (false, true, false, false) => ('╷', ' '),
        (false, false, true, true) => ('─', '─'),
        (false, false, true, false) => ('─', ' '),
        (false, false, false, true) => ('╶', '─'),
        (false, false, false, false) => (' ', ' '),
    }
}
fn label(row: &GraphRow, width: usize) -> Option<String> {
    if row.refs.is_empty() || width < 26 {
        return None;
    }
    let refs = row.refs.split(',').map(str::trim).collect::<Vec<_>>();
    let first = refs[0].trim_start_matches("HEAD -> ");
    let suffix = if refs.len() > 1 {
        format!(" +{}", refs.len() - 1)
    } else {
        String::new()
    };
    let available = (width * 2 / 5).max(10).min(width.saturating_sub(14));
    let raw = format!("{first}{suffix}");
    Some(format!("({})", truncate(&raw, available.saturating_sub(2))))
}
fn commit_line(row: &GraphRow, geometry: &Geometry, expanded: bool, width: usize) -> Line<'static> {
    let mut spans = geometry.spans(false);
    let prefix = geometry.width();
    let available = width.saturating_sub(prefix + 3);
    spans.push(Span::styled(
        if expanded { " ▾ " } else { " › " },
        Style::new().fg(Color::Gray),
    ));
    let subject = visible(&row.subject);
    let badge = label(row, available);
    let reserved = badge
        .as_ref()
        .map_or(0, |s| UnicodeWidthStr::width(s.as_str()) + 2);
    let subject = truncate(&subject, available.saturating_sub(reserved));
    let subject_width = UnicodeWidthStr::width(subject.as_str());
    spans.push(Span::raw(subject));
    if let Some(badge) = badge {
        let badge_width = UnicodeWidthStr::width(badge.as_str());
        let gap = available.saturating_sub(subject_width + badge_width);
        spans.push(Span::raw(" ".repeat(gap)));
        spans.push(Span::styled(badge, Style::new().fg(Color::Gray)));
    }
    Line::from(spans)
}
fn detail_line(geometry: &Geometry, text: &str, width: usize) -> Line<'static> {
    let mut spans = geometry.spans(true);
    spans.push(Span::raw("   "));
    let available = width.saturating_sub(geometry.width() + 3);
    spans.push(Span::styled(
        truncate(text, available),
        Style::new().fg(Color::Gray),
    ));
    Line::from(spans)
}
pub fn project(rows: &[GraphRow], expanded: &[Expanded<'_>], width: usize) -> Vec<DisplayRow> {
    #[cfg(test)]
    PROJECTIONS.with(|n| n.set(n.get() + 1));
    let expanded = expanded
        .iter()
        .map(|d| (d.sha, d))
        .collect::<std::collections::HashMap<_, _>>();
    let mut output = Vec::new();
    let mut pipe_sets = pipe_sets(rows).into_iter();
    for (index, row) in rows.iter().enumerate() {
        if row.sha.is_empty() {
            continue;
        }
        let pipes = pipe_sets.next().expect("one pipe set per commit");
        let geometry = Geometry::new(&pipes, row);
        let detail = expanded.get(row.sha.as_str());
        output.push(DisplayRow {
            item: Item::Commit(index),
            line: commit_line(row, &geometry, detail.is_some(), width),
        });
        output.push(DisplayRow {
            item: Item::Detail,
            line: detail_line(
                row,
                &format!("{} · {}", visible(&row.author), row.date),
                width,
            ),
        });
        if let Some(detail) = detail {
            let totals = detail
                .summary
                .lines()
                .last()
                .unwrap_or("No file changes")
                .trim();
            let meta = format!(
                "{} · {}{}",
                row.short,
                totals,
                if row.parents.len() > 1 {
                    " · first parent"
                } else {
                    ""
                }
            );
            output.push(DisplayRow {
                item: Item::Detail,
                line: detail_line(&geometry, &meta, width),
            });
            for (file_index, file) in detail.files.iter().enumerate() {
                let mut spans = geometry.spans(true);
                spans.push(Span::raw("   "));
                let stats = if file.added == "-" {
                    "binary".to_string()
                } else {
                    format!("+{} -{}", file.added, file.deleted)
                };
                let budget = width.saturating_sub(geometry.width() + 3);
                let path = truncate(&visible(&file.path), budget.saturating_sub(stats.len() + 2));
                spans.push(Span::raw(path));
                spans.push(Span::raw("  "));
                if file.added == "-" {
                    spans.push(Span::styled("binary", Style::new().fg(Color::Gray)));
                } else {
                    spans.push(Span::styled(
                        format!("+{}", file.added),
                        Style::new().fg(Color::Green),
                    ));
                    spans.push(Span::styled(
                        format!(" -{}", file.deleted),
                        Style::new().fg(Color::Red),
                    ));
                }
                output.push(DisplayRow {
                    item: Item::File(index, file_index),
                    line: Line::from(spans),
                });
            }
            if detail.files.is_empty() {
                output.push(DisplayRow {
                    item: Item::Detail,
                    line: detail_line(&geometry, "No file changes", width),
                });
            }
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample() -> GraphRow {
        GraphRow {
            lanes: "| * ".into(),
            lane_colors: vec![Some(0), None, Some(4), None],
            sha: "123".into(),
            short: "1234567".into(),
            subject: "合并分支并更新用户输入与个人助理功能".into(),
            author: "Alice".into(),
            date: "2025-10-29 | 10:38".into(),
            refs: "origin/feature-long-branch".into(),
            parents: vec!["a".into(), "b".into()],
        }
    }
    #[test]
    fn unicode_nodes_and_labels_fit_the_available_cells() {
        let row = sample();
        let pipes = pipe_sets(std::slice::from_ref(&row));
        let geometry = Geometry::new(&pipes[0], &row);
        for width in [26, 42, 60, 90] {
            let line = commit_line(&row, &geometry, false, width);
            assert!(line.width() <= width);
            let plain = line.to_string();
            assert!(plain.contains('○'));
            assert!(plain.contains('›'));
            assert!(!plain.contains("1234567"));
            if width >= 42 {
                assert!(plain.contains("origin/"));
            }
        }
        assert_eq!(truncate("你好世界", 5), "你好…");
        assert_eq!(truncate("abc", 0), "");
    }
    #[test]
    fn expanded_rows_carry_rail_colors_and_file_identity() {
        let row = sample();
        let files = vec![FileStat {
            path: "src/feature.rs".into(),
            added: "12".into(),
            deleted: "3".into(),
        }];
        let rows = project(
            &[row],
            &[Expanded {
                sha: "123",
                summary: "1 file changed",
                files: &files,
            }],
            60,
        );
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[3].item, Item::File(0, 0));
        assert!(rows[0].line.to_string().contains('▾'));
        assert_eq!(
            rows[1].line.to_string(),
            "│ │    Alice · 2025-10-29 | 10:38"
        );
        assert!(rows[2].line.to_string().starts_with("│ │"));
        assert_eq!(rows[3].line.spans[2].style.fg, Some(Color::Yellow));
        assert!(rows[3].line.to_string().contains("+12 -3"));
    }
    #[test]
    fn details_do_not_change_following_graph_geometry() {
        let row = sample();
        let next = GraphRow {
            lanes: "* ".into(),
            sha: "a".into(),
            parents: vec!["z".into()],
            ..Default::default()
        };
        let original = project(&[row.clone(), next.clone()], &[], 60);
        let expanded = project(
            &[row, next],
            &[Expanded {
                sha: "123",
                summary: "empty",
                files: &[],
            }],
            60,
        );
        assert_eq!(original.last().unwrap().line, expanded.last().unwrap().line);
        assert_eq!(original.last().unwrap().item, Item::Commit(1));
        assert!(expanded[0].item.selectable());
        assert!(!expanded[1].item.selectable());
    }
    #[test]
    fn merge_history_uses_rounded_connectors_without_extra_rows() {
        let commits = [
            ("a", vec!["b", "c"]),
            ("c", vec!["b"]),
            ("b", vec!["d"]),
            ("d", vec![]),
        ];
        let rows = commits
            .into_iter()
            .map(|(sha, parents)| GraphRow {
                lanes: "* ".into(),
                sha: sha.into(),
                parents: parents.into_iter().map(str::to_string).collect(),
                ..Default::default()
            })
            .collect::<Vec<_>>();
        let projected = project(&rows, &[], 42);
        let lines = projected
            .iter()
            .map(|row| row.line.to_string())
            .collect::<Vec<_>>();
        assert_eq!(projected.len(), 4);
        assert!(lines[0].starts_with("○─╮"), "{}", lines[0]);
        assert!(lines[1].starts_with("│ ●"), "{}", lines[1]);
        assert!(lines[2].starts_with("●─╯"), "{}", lines[2]);
        assert!(lines[3].starts_with('●'), "{}", lines[3]);
        assert!(!projected.iter().any(|row| row.item == Item::Connector));
    }
    #[test]
    fn straight_history_does_not_insert_connector_rows() {
        let first = GraphRow {
            lanes: "* ".into(),
            sha: "a".into(),
            parents: vec!["b".into()],
            ..Default::default()
        };
        let second = GraphRow {
            lanes: "* ".into(),
            sha: "b".into(),
            ..Default::default()
        };
        let rows = project(&[first, second], &[], 42);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| matches!(row.item, Item::Commit(_))));
    }
}
