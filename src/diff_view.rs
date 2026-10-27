//! Shared diff interaction for the Herdr popup and the standalone dialog.
use crate::{
    diff::{Document, ExpandResult, PreparedContext, MAX_REVIEW_BYTES},
    git::DiffSource,
    host::DiffData,
    review::{Delivery, Progress},
    theme::NAVIGATION,
};
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::Line,
    widgets::Paragraph,
    Frame,
};
use std::{
    hash::{DefaultHasher, Hash, Hasher},
    sync::mpsc::{self, Receiver, SyncSender},
    thread,
};

enum Loaded {
    Context(PreparedContext),
    Refresh(Box<ReadyDiff>),
}

struct ReadyDiff {
    title: String,
    doc: Document,
    source: Option<DiffSource>,
    patch_identity: (usize, u64),
}

impl ReadyDiff {
    fn new(data: DiffData) -> Self {
        Self {
            patch_identity: patch_identity(&data.patch),
            title: data.title,
            doc: Document::new(&data.patch),
            source: data.source,
        }
    }
}

fn patch_identity(patch: &str) -> (usize, u64) {
    let mut hash = DefaultHasher::new();
    patch.hash(&mut hash);
    (patch.len(), hash.finish())
}

#[derive(Clone, Copy)]
struct Expansion {
    row: usize,
    bottom: bool,
    all: bool,
}

pub(crate) struct Viewer {
    title: String,
    doc: Document,
    source: Option<DiffSource>,
    top: usize,
    cursor: usize,
    horizontal: usize,
    body: Rect,
    status: String,
    loading: Option<Receiver<Result<Loaded>>>,
    pending: Option<Expansion>,
    wake: Option<SyncSender<()>>,
    patch_identity: (usize, u64),
    selection: Option<usize>,
    drag_start: Option<usize>,
    delivery: Option<Delivery>,
}

impl Viewer {
    pub(crate) fn new(data: DiffData, wake: Option<SyncSender<()>>) -> Self {
        Self {
            patch_identity: patch_identity(&data.patch),
            title: data.title,
            doc: Document::new(&data.patch),
            source: data.source,
            top: 0,
            cursor: 0,
            horizontal: 0,
            body: Rect::default(),
            status: String::new(),
            loading: None,
            pending: None,
            wake,
            selection: None,
            drag_start: None,
            delivery: None,
        }
    }

    pub(crate) fn review_active(&self) -> bool {
        self.selection.is_some() || self.delivery.is_some() || self.drag_start.is_some()
    }

    pub(crate) fn captures_input(&self) -> bool {
        self.delivery.is_some()
    }

    pub(crate) fn hide_review(&mut self) {
        if let Some(delivery) = &mut self.delivery {
            delivery.hide();
        }
    }

    pub(crate) fn mouse_captured(&self) -> bool {
        self.drag_start.is_some()
    }

    fn selected_payload(&self) -> std::result::Result<String, String> {
        let snippet = self
            .doc
            .selected_patch(self.selection.unwrap_or(self.cursor), self.cursor)?;
        let source = self.source.as_ref().ok_or_else(|| {
            "Reopen this diff from the file list to include source information".to_string()
        })?;
        let payload = format!(
            "Selected diff lines\n{}\n\n{snippet}",
            source.review_context()
        );
        if payload.len() > MAX_REVIEW_BYTES {
            return Err("Selection exceeds 128 KiB; select fewer lines".into());
        }
        Ok(payload)
    }

    fn open_delivery(&mut self) {
        match self.selected_payload() {
            Ok(payload) => {
                self.status.clear();
                self.delivery = Some(Delivery::new(payload, self.wake.clone()));
            }
            Err(error) => self.status = error,
        }
    }

    /// Unchanged content keeps its loaded context and folding state. A new
    /// snapshot restores source coordinates without reusing obsolete text.
    pub(crate) fn reconcile(&mut self, mut next: Self) {
        let same_content = match (&self.source, &next.source) {
            (Some(a), Some(b)) => a.same_content(b),
            (None, None) => true,
            _ => false,
        };
        if same_content && self.patch_identity == next.patch_identity {
            self.title = next.title;
            self.source = next.source;
            self.wake = next.wake;
            // A context read may have captured the previous worktree stamp.
            self.loading = None;
            self.pending = None;
            self.status.clear();
            return;
        }
        let screen_row = self.cursor.saturating_sub(self.top);
        next.cursor = self
            .doc
            .source_line(self.cursor)
            .and_then(|(old, new)| next.doc.row_for_source(old, new))
            .unwrap_or(0);
        next.top = next.cursor.saturating_sub(screen_row);
        next.horizontal = self.horizontal;
        next.body = self.body;
        next.clamp();
        *self = next;
    }

    fn start(&mut self, refresh: bool) {
        if self.loading.is_some() {
            return;
        }
        let Some(source) = self.source.clone() else {
            self.status = "Context unavailable for this diff; reopen it from the panel".into();
            self.pending = None;
            return;
        };
        let (tx, rx) = mpsc::channel();
        let wake = self.wake.clone();
        self.loading = Some(rx);
        self.status = if refresh {
            "Refreshing diff…"
        } else {
            "Loading context…"
        }
        .into();
        thread::spawn(move || {
            let result = if refresh {
                source
                    .reload()
                    .map(|data| Loaded::Refresh(Box::new(ReadyDiff::new(data))))
            } else {
                source
                    .load_context()
                    .map(|text| Loaded::Context(Document::prepare_context(text)))
            };
            let _ = tx.send(result);
            if let Some(wake) = wake {
                let _ = wake.try_send(());
            }
        });
    }

    /// Worker completion wakes the same event loop as terminal input.
    pub(crate) fn poll(&mut self) -> bool {
        let progress = self
            .delivery
            .as_mut()
            .map(Delivery::poll)
            .unwrap_or(Progress::Idle);
        let changed = !matches!(progress, Progress::Idle);
        if let Progress::Sent(label) = progress {
            self.delivery = None;
            self.selection = None;
            self.status = format!("Pasted to {label}. Submit with Enter in that pane.");
        }
        // Keep the selected snapshot stable while choosing or addressing an
        // agent. Queued context results attach once the review is finished.
        if self.review_active() {
            return changed;
        }
        let delivery_notice = changed.then(|| self.status.clone());
        let Some(rx) = &self.loading else {
            return changed;
        };
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return changed,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.loading = None;
                self.pending = None;
                self.status = "Context worker stopped; press r to refresh".into();
                return true;
            }
        };
        self.loading = None;
        self.status.clear();
        match result {
            Ok(Loaded::Context(context)) => match self.doc.provide_prepared(context) {
                Ok(()) => {
                    if let Some(request) = self.pending.take() {
                        // Navigation during loading must not pull the reader back.
                        if self.cursor == request.row {
                            self.expand(request.bottom, request.all);
                        }
                    }
                }
                Err(error) => {
                    self.failed(error);
                }
            },
            Ok(Loaded::Refresh(data)) => {
                let position = self.doc.source_line(self.cursor);
                let screen_row = self.cursor.saturating_sub(self.top);
                self.title = data.title;
                self.doc = data.doc;
                self.source = data.source;
                self.patch_identity = data.patch_identity;
                self.pending = None;
                self.cursor = position
                    .and_then(|(old, new)| self.doc.row_for_source(old, new))
                    .unwrap_or(0);
                self.top = self.cursor.saturating_sub(screen_row);
            }
            Err(error) => {
                self.failed(format!("{error:#}"));
            }
        }
        self.clamp();
        if let Some(notice) = delivery_notice {
            self.status = notice;
        }
        true
    }

    fn failed(&mut self, message: String) {
        self.pending = None;
        self.status = message;
        if !self.status.contains("press r") {
            self.status.push_str("; press r to refresh");
        }
    }

    fn clamp(&mut self) {
        let height = usize::from(self.body.height).max(1);
        self.horizontal = self
            .horizontal
            .min(self.doc.horizontal_limit(self.body.width.saturating_sub(1)));
        self.cursor = self.cursor.min(self.doc.len().saturating_sub(1));
        self.top = self.top.min(self.doc.len().saturating_sub(height));
        if self.cursor < self.top {
            self.top = self.cursor;
        } else if self.cursor >= self.top + height {
            self.top = self.cursor + 1 - height;
        }
    }

    fn anchor(&mut self, row: usize) {
        let screen_row = self.cursor.saturating_sub(self.top);
        self.cursor = row;
        self.top = row.saturating_sub(screen_row);
        self.clamp();
    }

    fn expand(&mut self, bottom: bool, all: bool) {
        match self.doc.toggle(self.cursor, bottom, all) {
            ExpandResult::Changed { anchor } => self.anchor(anchor),
            ExpandResult::NeedsContext => {
                self.pending = Some(Expansion {
                    row: self.cursor,
                    bottom,
                    all,
                });
                self.start(false);
            }
            ExpandResult::Unchanged => (),
        }
    }

    pub(crate) fn key(&mut self, key: KeyEvent) -> bool {
        if let Some(delivery) = &mut self.delivery {
            if delivery.key(key) {
                self.delivery = None;
            }
            return false;
        }
        self.drag_start = None;
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                if self.selection.take().is_none() {
                    return true;
                }
                self.drag_start = None;
            }
            KeyCode::Char('v') => {
                self.selection = if self.selection.is_some() {
                    None
                } else {
                    Some(self.cursor)
                };
                self.drag_start = None;
            }
            KeyCode::Char('p') => self.open_delivery(),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return true,
            KeyCode::Down | KeyCode::Char('j') => self.cursor = self.cursor.saturating_add(1),
            KeyCode::Up | KeyCode::Char('k') => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::PageDown => {
                self.cursor = self
                    .cursor
                    .saturating_add(usize::from(self.body.height).max(1));
            }
            KeyCode::PageUp => {
                self.cursor = self
                    .cursor
                    .saturating_sub(usize::from(self.body.height).max(1));
            }
            KeyCode::Right => self.horizontal = self.horizontal.saturating_add(4),
            KeyCode::Left => self.horizontal = self.horizontal.saturating_sub(4),
            KeyCode::Home => {
                self.cursor = 0;
                self.horizontal = 0;
            }
            KeyCode::End => self.cursor = self.doc.len().saturating_sub(1),
            KeyCode::Enter if !self.review_active() => self.expand(
                key.modifiers.contains(KeyModifiers::SHIFT)
                    || self.doc.expand_from_bottom(self.cursor),
                false,
            ),
            KeyCode::Char('t') if !self.review_active() => self.expand(false, false),
            KeyCode::Char('b') if !self.review_active() => self.expand(true, false),
            KeyCode::Char('a') if !self.review_active() => self.expand(false, true),
            KeyCode::Char(' ') => {
                self.selection = None;
                if let Some(anchor) = self.doc.collapse(self.cursor) {
                    self.anchor(anchor);
                }
            }
            KeyCode::Char('[') | KeyCode::Char(']') => {
                if let Some(row) = self
                    .doc
                    .next_change(self.cursor, key.code == KeyCode::Char(']'))
                {
                    self.cursor = row;
                }
            }
            KeyCode::Char('r') => {
                self.selection = None;
                self.drag_start = None;
                self.pending = None;
                self.start(true);
            }
            _ => (),
        }
        self.clamp();
        false
    }

    pub(crate) fn mouse(&mut self, mouse: MouseEvent) {
        if let Some(delivery) = &mut self.delivery {
            delivery.mouse(mouse);
            return;
        }
        if mouse.kind == MouseEventKind::Up(MouseButton::Left) {
            if self.drag_start.is_some_and(|start| {
                self.selection.is_some()
                    || start != self.top + usize::from(mouse.row.saturating_sub(self.body.y))
            }) {
                self.extend_drag(mouse.row);
            }
            self.drag_start = None;
            return;
        }
        if mouse.kind == MouseEventKind::Drag(MouseButton::Left) && self.drag_start.is_some() {
            self.extend_drag(mouse.row);
            return;
        }
        if !self.body.contains((mouse.column, mouse.row).into()) {
            return;
        }
        match mouse.kind {
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let max = self.doc.len().saturating_sub(usize::from(self.body.height));
                self.top = if mouse.kind == MouseEventKind::ScrollDown {
                    self.top.saturating_add(3).min(max)
                } else {
                    self.top.saturating_sub(3)
                };
                self.cursor = self.cursor.clamp(
                    self.top,
                    self.top + usize::from(self.body.height).saturating_sub(1),
                );
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let previous = self.cursor;
                self.cursor = self.top + usize::from(mouse.row - self.body.y);
                if !self.doc.is_fold(self.cursor) {
                    if mouse.modifiers.contains(KeyModifiers::SHIFT) {
                        self.selection.get_or_insert(previous);
                    } else {
                        self.selection = None;
                    }
                    self.drag_start = Some(self.selection.unwrap_or(self.cursor));
                }
                let bottom = mouse.column >= self.body.x + self.body.width / 2;
                if self.doc.is_fold(self.cursor) && !self.review_active() {
                    self.expand(bottom, mouse.modifiers.contains(KeyModifiers::SHIFT));
                }
            }
            _ => (),
        }
        self.clamp();
    }

    fn extend_drag(&mut self, row: u16) {
        if self.body.height == 0 {
            return;
        }
        self.selection = self.drag_start;
        if row < self.body.y {
            self.top = self.top.saturating_sub(1);
        } else if row >= self.body.bottom() {
            self.top = self
                .top
                .saturating_add(1)
                .min(self.doc.len().saturating_sub(usize::from(self.body.height)));
        }
        self.cursor =
            self.top + usize::from(row.saturating_sub(self.body.y).min(self.body.height - 1));
        self.clamp();
    }

    pub(crate) fn render(&mut self, frame: &mut Frame, area: Rect) {
        self.render_mode(frame, area, true, false);
    }

    pub(crate) fn render_embedded(&mut self, frame: &mut Frame, area: Rect, focused: bool) {
        self.render_mode(frame, area, focused, true);
    }

    fn render_mode(&mut self, frame: &mut Frame, area: Rect, focused: bool, embedded: bool) {
        let rows = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(if self.doc.metadata().is_empty() { 0 } else { 1 }),
            Constraint::Min(1),
            Constraint::Length(if self.status.is_empty() { 0 } else { 1 }),
            Constraint::Length(1),
        ])
        .split(area);
        self.body = rows[2];
        self.clamp();
        let title: String = self
            .title
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        frame.render_widget(
            Paragraph::new(title).style(Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD)),
            rows[0],
        );
        frame.render_widget(
            Paragraph::new(self.doc.metadata()).style(Style::new().fg(Color::Yellow)),
            rows[1],
        );
        for screen in 0..self.body.height {
            let index = self.top + usize::from(screen);
            if index >= self.doc.len() {
                break;
            }
            let mut line = self
                .doc
                .line(index, self.horizontal, self.body.width.saturating_sub(1));
            if focused
                && self.selection.is_some_and(|anchor| {
                    (anchor.min(self.cursor)..=anchor.max(self.cursor)).contains(&index)
                })
            {
                line = line.style(NAVIGATION);
                for span in &mut line.spans {
                    span.style = span.style.patch(NAVIGATION);
                }
            }
            if focused && index == self.cursor {
                // A gutter cursor leaves diff backgrounds and token colors intact.
                line.spans.insert(
                    0,
                    ratatui::text::Span::styled("›", Style::new().fg(Color::Blue)),
                );
            } else {
                line.spans.insert(0, ratatui::text::Span::raw(" "));
            }
            let style = line.style;
            frame.render_widget(
                Paragraph::new(line).style(style),
                Rect::new(self.body.x, self.body.y + screen, self.body.width, 1),
            );
        }
        frame.render_widget(
            Paragraph::new(self.status.as_str()).style(Style::new().fg(Color::Yellow)),
            rows[3],
        );
        let hint = if self.selection.is_some() {
            "↑↓ extend selection  p send to agent  v/Esc cancel selection"
        } else if self.doc.is_empty() {
            "r refresh  q close"
        } else if area.width >= 100 {
            "↑↓ move  ←→ pan  v select  p agent  Enter expand  t/b edges  a all  Space fold  [ ] changes  r refresh  q close"
        } else {
            "v select  p agent  Enter expand  a all  Space fold  r refresh  q close"
        };
        let hint = if embedded {
            hint.replace("q close", "Esc files")
        } else {
            hint.to_string()
        };
        frame.render_widget(
            Paragraph::new(Line::raw(hint)).style(Style::new().fg(Color::Gray)),
            rows[4],
        );
        if let Some(delivery) = &mut self.delivery {
            delivery.render(frame, area);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};
    use std::time::Instant;

    fn data(patch: String) -> DiffData {
        DiffData {
            title: "unstaged · 中文.rs".into(),
            patch,
            source: None,
        }
    }

    fn fixture() -> Viewer {
        let patch = "@@ -26,11 +26,11 @@\n line26\n line27\n line28\n line29\n line30\n-old31\n+new31\n line32\n line33\n line34\n line35\n line36\n";
        Viewer::new(data(patch.into()), None)
    }

    fn context() -> String {
        (1..=100)
            .map(|i| {
                if i == 31 {
                    "new31\n".into()
                } else {
                    format!("line{i}\n")
                }
            })
            .collect()
    }

    fn draw(viewer: &mut Viewer, terminal: &mut Terminal<TestBackend>) -> String {
        terminal.draw(|f| viewer.render(f, f.area())).unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn keyboard_and_mouse_expand_collapse_and_keep_source_coordinates() {
        let mut viewer = fixture();
        viewer.doc.provide_context(context()).unwrap();
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        let screen = draw(&mut viewer, &mut terminal);
        assert!(screen.contains('中') && screen.contains('文') && screen.contains(".rs"));
        assert!(screen.contains("25 unchanged"));
        assert!(screen.contains("line28"));
        viewer.key(KeyEvent::from(KeyCode::Char('t')));
        assert_eq!(
            viewer.doc.source_line(viewer.cursor),
            Some((Some(1), Some(1)))
        );
        viewer.key(KeyEvent::from(KeyCode::Char(' ')));
        assert!(viewer.doc.is_fold(viewer.cursor));
        draw(&mut viewer, &mut terminal);
        viewer.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: viewer.body.right() - 1,
            row: viewer.body.y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(
            viewer.doc.source_line(viewer.cursor),
            Some((Some(11), Some(11)))
        );
        viewer.key(KeyEvent::from(KeyCode::Char(' ')));
        viewer.key(KeyEvent::from(KeyCode::Char(']')));
        assert_eq!(
            viewer.doc.source_line(viewer.cursor),
            Some((Some(31), None))
        );
    }

    #[test]
    fn late_context_does_not_move_a_reader_who_navigated_during_loading() {
        let mut viewer = fixture();
        let (tx, rx) = mpsc::channel();
        viewer.loading = Some(rx);
        viewer.pending = Some(Expansion {
            row: 0,
            bottom: false,
            all: false,
        });
        viewer.key(KeyEvent::from(KeyCode::Char(']')));
        let selected = viewer.doc.source_line(viewer.cursor);
        tx.send(Ok(Loaded::Context(Document::prepare_context(context()))))
            .unwrap();
        assert!(viewer.poll());
        assert_eq!(viewer.doc.source_line(viewer.cursor), selected);
        assert!(viewer.doc.is_fold(0));
        assert!(!viewer.poll());
        assert!(viewer.key(KeyEvent::from(KeyCode::Esc)));
    }

    #[test]
    fn default_expansion_reveals_adjacent_patch_context_before_loading() {
        let mut viewer = fixture();
        viewer.cursor = viewer.doc.row_for_source(None, Some(26)).unwrap();
        viewer.key(KeyEvent::from(KeyCode::Char(' ')));
        viewer.key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(
            viewer.doc.source_line(viewer.cursor),
            Some((Some(26), Some(26)))
        );
        assert!(viewer.loading.is_none());
        assert!(viewer.status.is_empty());
        viewer.key(KeyEvent::from(KeyCode::Char(' ')));
        assert!(viewer.doc.is_fold(viewer.cursor));
    }

    #[test]
    fn refresh_preserves_source_location_and_reports_failed_context() {
        let mut viewer = fixture();
        viewer.doc.provide_context(context()).unwrap();
        viewer.key(KeyEvent::from(KeyCode::Char('a')));
        viewer.cursor = 24;
        let selected = viewer.doc.source_line(viewer.cursor);
        let (tx, rx) = mpsc::channel();
        viewer.loading = Some(rx);
        tx.send(Ok(Loaded::Refresh(Box::new(ReadyDiff::new(data(
            "@@ -26,11 +26,11 @@\n line26\n line27\n line28\n line29\n line30\n-old31\n+new31\n line32\n line33\n line34\n line35\n line36\n".into(),
        )))))).unwrap();
        viewer.poll();
        assert!(viewer.doc.is_fold(viewer.cursor));
        let (old, new) = selected.unwrap();
        assert_eq!(viewer.doc.row_for_source(old, new), Some(viewer.cursor));
        let (tx, rx) = mpsc::channel();
        viewer.loading = Some(rx);
        tx.send(Ok(Loaded::Context(Document::prepare_context(
            "changed\n".into(),
        ))))
        .unwrap();
        viewer.poll();
        assert!(viewer.status.contains("refresh"));
    }

    fn inserted_patch(count: usize) -> String {
        let mut patch = format!("--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1,{count} @@\n");
        for i in 0..count {
            patch.push_str(&format!("+line {i:06} let timeout = 5000;\n"));
        }
        patch
    }

    #[test]
    fn large_offsets_resize_and_page_navigation_stay_inside_content() {
        let mut viewer = Viewer::new(data(inserted_patch(70_000)), None);
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        draw(&mut viewer, &mut terminal);
        viewer.key(KeyEvent::from(KeyCode::End));
        let screen = draw(&mut viewer, &mut terminal);
        assert!(viewer.cursor > usize::from(u16::MAX));
        assert!(screen.contains("069999"));
        for _ in 0..3 {
            viewer.key(KeyEvent::from(KeyCode::PageDown));
        }
        assert_eq!(viewer.cursor, 69_999);
        assert_eq!(
            viewer.top + usize::from(viewer.body.height),
            viewer.doc.len()
        );
        terminal.backend_mut().resize(80, 30);
        draw(&mut viewer, &mut terminal);
        assert_eq!(
            viewer.top + usize::from(viewer.body.height),
            viewer.doc.len()
        );
        viewer.key(KeyEvent::from(KeyCode::PageUp));
        assert_eq!(viewer.cursor, 69_999 - usize::from(viewer.body.height));
    }

    #[test]
    fn horizontal_scroll_is_bounded_and_reclamped_after_resize() {
        let mut viewer = Viewer::new(data(inserted_patch(2)), None);
        let mut terminal = Terminal::new(TestBackend::new(20, 10)).unwrap();
        draw(&mut viewer, &mut terminal);
        for _ in 0..100 {
            viewer.key(KeyEvent::from(KeyCode::Right));
        }
        assert!(viewer.horizontal > 0);
        assert_eq!(viewer.horizontal, viewer.doc.horizontal_limit(19));
        terminal.backend_mut().resize(120, 10);
        draw(&mut viewer, &mut terminal);
        assert_eq!(viewer.horizontal, 0);
    }

    #[test]
    fn unchanged_preview_refresh_keeps_loaded_context_and_manual_expansion() {
        let mut viewer = fixture();
        viewer.doc.provide_context(context()).unwrap();
        viewer.key(KeyEvent::from(KeyCode::Char('a')));
        let rows = viewer.doc.len();
        viewer.cursor = 14;
        let position = viewer.doc.source_line(viewer.cursor);
        viewer.reconcile(fixture());
        assert_eq!(viewer.doc.len(), rows);
        assert_eq!(viewer.doc.source_line(viewer.cursor), position);
        assert_eq!(viewer.cursor, 14);
    }

    #[test]
    fn visual_selection_tracks_both_directions_and_escape_cancels_before_closing() {
        let mut viewer = fixture();
        viewer.cursor = viewer.doc.row_for_source(Some(31), None).unwrap();
        let start = viewer.cursor;
        viewer.key(KeyEvent::from(KeyCode::Char('v')));
        viewer.key(KeyEvent::from(KeyCode::Down));
        viewer.horizontal = 200;
        let text = viewer
            .doc
            .selected_patch(viewer.selection.unwrap(), viewer.cursor)
            .unwrap();
        assert!(text.contains("-old31") && text.contains("+new31"));
        viewer.key(KeyEvent::from(KeyCode::Up));
        viewer.key(KeyEvent::from(KeyCode::Up));
        assert_eq!(viewer.selection, Some(start));
        assert!(viewer.review_active());
        assert!(!viewer.key(KeyEvent::from(KeyCode::Esc)));
        assert!(!viewer.review_active());
        assert!(viewer.key(KeyEvent::from(KeyCode::Esc)));
    }

    #[test]
    fn mouse_drag_and_shift_click_select_source_rows_without_expanding_them() {
        let mut viewer = fixture();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        draw(&mut viewer, &mut terminal);
        let start = viewer.doc.row_for_source(Some(31), None).unwrap();
        let y = viewer.body.y + (start - viewer.top) as u16;
        let mouse = |kind, row, modifiers| MouseEvent {
            kind,
            column: 30,
            row,
            modifiers,
        };
        viewer.mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            y,
            KeyModifiers::NONE,
        ));
        assert!(viewer.mouse_captured());
        viewer.mouse(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            y + 1,
            KeyModifiers::NONE,
        ));
        viewer.mouse(mouse(
            MouseEventKind::Up(MouseButton::Left),
            y + 2,
            KeyModifiers::NONE,
        ));
        assert_eq!(viewer.selection, Some(start));
        assert_eq!(viewer.cursor, start + 2);
        assert!(!viewer.mouse_captured());
        viewer.mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            y + 2,
            KeyModifiers::SHIFT,
        ));
        viewer.mouse(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            y + 3,
            KeyModifiers::SHIFT,
        ));
        assert_eq!(viewer.selection, Some(start));
        let text = viewer.doc.selected_patch(start, viewer.cursor).unwrap();
        assert!(text.contains("+new31") && text.contains("line33"));
    }

    #[test]
    fn same_content_rewrite_keeps_folds_and_replaces_the_worktree_stamp() {
        use crate::git::{ChangeKind, FileStat, Repo, WorkingFile};
        let root = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let result = std::process::Command::new("git")
                .arg("-C")
                .arg(root.path())
                .args(args)
                .output()
                .unwrap();
            assert!(result.status.success());
        };
        git(&["init", "-b", "main"]);
        git(&["config", "user.name", "Refresh Test"]);
        git(&["config", "user.email", "refresh@example.invalid"]);
        let original: String = (1..=100).map(|i| format!("line{i}\n")).collect();
        std::fs::write(root.path().join("file.txt"), original).unwrap();
        git(&["add", "."]);
        git(&["commit", "-m", "Base"]);
        std::fs::write(root.path().join("file.txt"), context()).unwrap();
        let repo = Repo::discover(root.path()).unwrap();
        let file = WorkingFile {
            kind: ChangeKind::Unstaged,
            stat: FileStat {
                path: "file.txt".into(),
                added: "1".into(),
                deleted: "1".into(),
            },
        };
        let mut viewer = Viewer::new(repo.working_diff_data(&file).unwrap(), None);
        viewer.doc.provide_context(context()).unwrap();
        viewer.key(KeyEvent::from(KeyCode::Char('a')));
        let rows = viewer.doc.len();
        let old_source = viewer.source.clone().unwrap();
        std::fs::write(root.path().join("file.txt"), context()).unwrap();
        let next = Viewer::new(repo.working_diff_data(&file).unwrap(), None);
        assert_ne!(
            viewer.source, next.source,
            "the rewrite must change the stamp"
        );
        assert!(old_source.load_context().is_err());
        viewer.reconcile(next);
        assert_eq!(viewer.doc.len(), rows);
        assert_eq!(viewer.source.unwrap().load_context().unwrap(), context());
    }

    #[test]
    fn mode_changes_remain_visible_alongside_the_code_diff() {
        let mut viewer = Viewer::new(
            data("old mode 100644\nnew mode 100755\n@@ -1 +1 @@\n-old\n+new\n".into()),
            None,
        );
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        let screen = draw(&mut viewer, &mut terminal);
        assert!(screen.contains("old mode 100644"));
        assert!(screen.contains("new mode 100755"));
        assert!(screen.contains("new"));
    }

    /// Run with --release --ignored --nocapture; timings are observations,
    /// not thresholds that depend on the machine executing the test suite.
    #[test]
    #[ignore]
    fn diff_viewport_benchmark() {
        for count in [1_000, 10_000, 100_000] {
            let patch = inserted_patch(count);
            let start = Instant::now();
            let mut viewer = Viewer::new(data(patch), None);
            let build = start.elapsed();
            let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
            draw(&mut viewer, &mut terminal);
            let start = Instant::now();
            for _ in 0..300 {
                viewer.key(KeyEvent::from(KeyCode::Down));
                terminal.draw(|f| viewer.render(f, f.area())).unwrap();
            }
            eprintln!(
                "{count} changed lines: build={build:?}, 300 frames={:?}",
                start.elapsed()
            );
        }
    }
}
