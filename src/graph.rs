//! Compact commit rows modeled on the supplied Orca panel reference.
//! Git owns topology/color identity; this module only projects it into cells.
use crate::git::{FileStat, GraphRow};
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};
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
fn track(row: &GraphRow, index: usize) -> Style {
    let color = row
        .lane_colors
        .get(index)
        .copied()
        .flatten()
        .map(usize::from)
        .unwrap_or(index / 2)
        % TRACKS.len();
    Style::new().fg(TRACKS[color])
}
fn lanes(row: &GraphRow, continuation: bool) -> Vec<Span<'static>> {
    row.lanes
        .chars()
        .enumerate()
        .map(|(i, c)| {
            let glyph = if continuation {
                match c {
                    '*' if row.parents.is_empty() => ' ',
                    '*' | '|' | '/' | '\\' => '│',
                    _ => ' ',
                }
            } else {
                match c {
                    '*' if row.parents.len() > 1 => '○',
                    '*' => '●',
                    '|' => '│',
                    '/' => '╱',
                    '\\' => '╲',
                    '_' | '-' => '─',
                    _ => c,
                }
            };
            Span::styled(glyph.to_string(), track(row, i))
        })
        .collect()
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
fn commit_line(row: &GraphRow, expanded: bool, width: usize) -> Line<'static> {
    let mut spans = lanes(row, false);
    let prefix = UnicodeWidthStr::width(row.lanes.as_str());
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
fn detail_line(row: &GraphRow, text: &str, width: usize) -> Line<'static> {
    let mut spans = lanes(row, true);
    spans.push(Span::raw("   "));
    let available = width.saturating_sub(UnicodeWidthStr::width(row.lanes.as_str()) + 3);
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
    for (index, row) in rows.iter().enumerate() {
        if row.sha.is_empty() {
            output.push(DisplayRow {
                item: Item::Connector,
                line: Line::from(lanes(row, false)),
            });
            continue;
        }
        let detail = expanded.get(row.sha.as_str());
        output.push(DisplayRow {
            item: Item::Commit(index),
            line: commit_line(row, detail.is_some(), width),
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
                line: detail_line(row, &meta, width),
            });
            for (file_index, file) in detail.files.iter().enumerate() {
                let mut spans = lanes(row, true);
                spans.push(Span::raw("   "));
                let stats = if file.added == "-" {
                    "binary".to_string()
                } else {
                    format!("+{} -{}", file.added, file.deleted)
                };
                let budget = width.saturating_sub(UnicodeWidthStr::width(row.lanes.as_str()) + 3);
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
                    line: detail_line(row, "No file changes", width),
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
        for width in [26, 42, 60, 90] {
            let line = commit_line(&row, false, width);
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
            lanes: "|/  ".into(),
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
        assert_eq!(original.last().unwrap().item, Item::Connector);
        assert!(expanded[0].item.selectable());
        assert!(!expanded[1].item.selectable());
    }
    #[test]
    fn straight_history_uses_metadata_rows_as_continuous_rails() {
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
        assert_eq!(rows.len(), 4);
        assert!(!rows.iter().any(|row| row.item == Item::Connector));
        assert!(rows[1].line.to_string().starts_with("│ "));
    }
}
