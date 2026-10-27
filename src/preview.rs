//! Selected-file preview with one active read and one replaceable desired request.
//! A short selection deadline coalesces keyboard navigation. The event loop
//! sleeps until that deadline or a completion; idle previews have no timer.
use crate::{
    diff_view::Viewer,
    git::{ChangeKind, FileStat, Repo, WorkingFile},
    host::DiffData,
};
use anyhow::{anyhow, Result};
use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    widgets::{Block, Paragraph, Wrap},
    Frame,
};
use std::{
    sync::mpsc::{self, Receiver, SyncSender},
    thread,
    time::{Duration, Instant},
};

const SELECTION_SETTLE: Duration = Duration::from_millis(75);
// Border, title, metadata, two possible notices, footer and at least one code row.
pub(crate) const MIN_HEIGHT: u16 = 8;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Request {
    Working { kind: ChangeKind, path: String },
    Branch { base: String, path: String },
    Commit { sha: String, path: String },
}

impl Request {
    pub(crate) fn read(&self, repo: &Repo) -> Result<DiffData> {
        match self {
            Self::Working { kind, path } => repo.working_diff_data(&WorkingFile {
                stat: FileStat {
                    path: path.clone(),
                    added: String::new(),
                    deleted: String::new(),
                },
                kind: *kind,
            }),
            Self::Branch { base, path } => repo.diff_data(base, "", path),
            Self::Commit { sha, path } => repo.diff_data("", sha, path),
        }
    }
}

struct Running {
    revision: u64,
    result: Receiver<Result<Viewer>>,
}

pub(crate) struct Preview {
    repo: Repo,
    desired: Option<Request>,
    revision: u64,
    settled: Option<u64>,
    deadline: Option<Instant>,
    running: Option<Running>,
    viewer: Option<Viewer>,
    pending_navigation_edge: Option<bool>,
    error: Option<String>,
    wake: Option<SyncSender<()>>,
}

impl Preview {
    pub(crate) fn new(repo: Repo) -> Self {
        Self {
            repo,
            desired: None,
            revision: 0,
            settled: None,
            deadline: None,
            running: None,
            viewer: None,
            pending_navigation_edge: None,
            error: None,
            wake: None,
        }
    }

    pub(crate) fn select(
        &mut self,
        request: Option<Request>,
        wake: Option<SyncSender<()>>,
    ) -> bool {
        self.wake = wake;
        if self.desired == request {
            return false;
        }
        self.desired = request;
        self.revision = self.revision.wrapping_add(1);
        self.viewer = None;
        self.pending_navigation_edge = None;
        self.error = None;
        self.deadline = self
            .desired
            .as_ref()
            .map(|_| Instant::now() + SELECTION_SETTLE);
        true
    }

    /// Keep the current document visible while its replacement is prepared.
    pub(crate) fn invalidate(&mut self, wake: Option<SyncSender<()>>) {
        self.wake = wake;
        if self.desired.is_some() {
            self.revision = self.revision.wrapping_add(1);
            self.error = None;
            self.deadline = Some(Instant::now() + SELECTION_SETTLE);
        }
    }

    /// Only pending selections need a deadline. An active read wakes the UI on
    /// completion, so an elapsed queued deadline must not cause a busy loop.
    pub(crate) fn wait(&self, now: Instant) -> Option<Duration> {
        if self.viewer.as_ref().is_some_and(Viewer::review_active) {
            return None;
        }
        if self.running.is_some() || self.desired.is_none() || self.settled == Some(self.revision) {
            return None;
        }
        self.deadline
            .map(|deadline| deadline.saturating_duration_since(now))
    }

    fn start_latest(&mut self, now: Instant) {
        if self.viewer.as_ref().is_some_and(Viewer::review_active) {
            return;
        }
        if self.running.is_some() || self.settled == Some(self.revision) {
            return;
        }
        if self.deadline.is_none_or(|deadline| deadline > now) {
            return;
        }
        let Some(request) = self.desired.clone() else {
            return;
        };
        self.deadline = None;
        let repo = self.repo.clone();
        let wake = self.wake.clone();
        let (sender, result) = mpsc::channel();
        self.running = Some(Running {
            revision: self.revision,
            result,
        });
        thread::spawn(move || {
            let prepared = request
                .read(&repo)
                .map(|data| Viewer::new(data, wake.clone()));
            let _ = sender.send(prepared);
            if let Some(wake) = wake {
                let _ = wake.try_send(());
            }
        });
    }

    pub(crate) fn poll(&mut self) -> bool {
        let mut changed = self.viewer.as_mut().is_some_and(Viewer::poll);
        if self.viewer.as_ref().is_some_and(Viewer::review_active) {
            return changed;
        }
        self.start_latest(Instant::now());
        let Some(running) = &self.running else {
            return changed;
        };
        let result = match running.result.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return changed,
            Err(mpsc::TryRecvError::Disconnected) => Err(anyhow!("Diff worker stopped")),
        };
        let revision = running.revision;
        self.running = None;
        if revision == self.revision && self.desired.is_some() {
            self.settled = Some(revision);
            match result {
                Ok(viewer) => {
                    if let Some(current) = &mut self.viewer {
                        current.reconcile(viewer);
                    } else {
                        self.viewer = Some(viewer);
                    }
                    if let Some(forward) = self.pending_navigation_edge.take() {
                        if let Some(viewer) = &mut self.viewer {
                            viewer.enter_navigation_edge(forward);
                        }
                    }
                    self.error = None;
                }
                Err(error) => self.error = Some(format!("{error:#}")),
            }
            changed = true;
        }
        self.start_latest(Instant::now());
        changed
    }

    pub(crate) fn available(&self) -> bool {
        self.desired.is_some()
    }

    /// Use the preview's identity, including an expanded history commit, rather
    /// than inferring the popup source from a file list that may have moved.
    pub(crate) fn request(&self) -> Option<Request> {
        self.desired.clone()
    }

    pub(crate) fn at_navigation_edge(&self, forward: bool) -> bool {
        self.viewer
            .as_ref()
            .is_none_or(|viewer| viewer.at_navigation_edge(forward))
    }

    pub(crate) fn enter_navigation_edge(&mut self, forward: bool) {
        if let Some(viewer) = &mut self.viewer {
            viewer.enter_navigation_edge(forward);
        } else if self.desired.is_some() {
            self.pending_navigation_edge = Some(forward);
        }
    }

    /// `true` asks the panel to return focus to its selected file.
    pub(crate) fn key(&mut self, key: KeyEvent) -> bool {
        if key.code == KeyCode::Char('r')
            && !self.viewer.as_ref().is_some_and(Viewer::review_active)
        {
            self.invalidate(self.wake.clone());
            if self.desired.is_some() {
                self.deadline = Some(Instant::now());
                self.start_latest(Instant::now());
            }
            return false;
        }
        self.viewer.as_mut().map_or_else(
            || matches!(key.code, KeyCode::Esc | KeyCode::Char('q')),
            |viewer| viewer.key(key),
        )
    }

    pub(crate) fn captures_input(&self) -> bool {
        self.viewer.as_ref().is_some_and(Viewer::captures_input)
    }

    pub(crate) fn hide_review(&mut self) {
        if let Some(viewer) = &mut self.viewer {
            viewer.hide_review();
        }
    }

    pub(crate) fn review_active(&self) -> bool {
        self.viewer.as_ref().is_some_and(Viewer::review_active)
    }

    pub(crate) fn mouse_captured(&self) -> bool {
        self.viewer.as_ref().is_some_and(Viewer::mouse_captured)
    }

    pub(crate) fn mouse(&mut self, mouse: MouseEvent) {
        if let Some(viewer) = &mut self.viewer {
            viewer.mouse(mouse);
        }
    }

    pub(crate) fn render(&mut self, frame: &mut Frame, area: Rect, focused: bool) {
        let title = if focused {
            "Diff · Shift+Enter popup · Esc files"
        } else {
            "Diff · Enter focus"
        };
        let block = Block::bordered()
            .title(title)
            .border_style(Style::new().fg(if focused { Color::Blue } else { Color::Gray }));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if area.height < MIN_HEIGHT {
            let choosing = self.viewer.as_ref().is_some_and(Viewer::captures_input);
            if let Some(viewer) = &mut self.viewer {
                viewer.hide_review();
            }
            let message = if choosing {
                "Enlarge panel to choose an agent · Esc cancel"
            } else {
                "Enlarge panel to read diff · o popup"
            };
            frame.render_widget(
                Paragraph::new(message).style(Style::new().fg(Color::Gray)),
                inner,
            );
            return;
        }
        let notice = self.viewer.as_ref().and_then(|_| {
            self.error.as_deref().or_else(|| {
                (self.running.is_some() || self.deadline.is_some()).then_some("Refreshing diff…")
            })
        });
        let rows = Layout::vertical([
            Constraint::Min(0),
            Constraint::Length(u16::from(notice.is_some())),
        ])
        .split(inner);
        if let Some(viewer) = &mut self.viewer {
            viewer.render_embedded(frame, rows[0], focused);
        } else {
            let message = if let Some(error) = &self.error {
                format!("Could not read diff: {error}\nPress r to retry")
            } else if self.desired.is_some() {
                "Loading diff…".into()
            } else {
                "Select a changed file to preview its diff".into()
            };
            frame.render_widget(
                Paragraph::new(message)
                    .style(Style::new().fg(Color::Gray))
                    .wrap(Wrap { trim: false }),
                rows[0],
            );
        }
        if let Some(notice) = notice {
            frame.render_widget(
                Paragraph::new(notice).style(Style::new().fg(Color::Yellow)),
                rows[1],
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};
    use std::{
        fs,
        path::Path,
        process::Command,
        time::{Duration, Instant},
    };

    #[test]
    fn preview_navigation_can_pass_loading_content_and_respects_review_boundaries() {
        let mut preview = Preview::new(Repo {
            path: std::env::temp_dir(),
        });
        assert!(preview.at_navigation_edge(false));
        assert!(preview.at_navigation_edge(true));

        preview.viewer = Some(fake_viewer("change"));
        assert!(preview.at_navigation_edge(false));
        assert!(!preview.at_navigation_edge(true));
        preview.key(KeyEvent::from(KeyCode::End));
        assert!(preview.at_navigation_edge(true));
        preview.key(KeyEvent::from(KeyCode::Char('v')));
        assert!(!preview.at_navigation_edge(true));
        assert!(!preview.at_navigation_edge(false));
        preview.key(KeyEvent::from(KeyCode::Esc));
        assert!(preview.at_navigation_edge(true));
    }

    #[test]
    fn navigation_entry_waits_for_the_selected_document_and_resets_on_selection_change() {
        let mut preview = Preview::new(Repo {
            path: std::env::temp_dir(),
        });
        preview.desired = Some(request("a"));
        preview.revision = 1;
        let (sender, result) = mpsc::channel();
        preview.running = Some(Running {
            revision: 1,
            result,
        });
        preview.enter_navigation_edge(false);
        assert_eq!(preview.pending_navigation_edge, Some(false));
        sender.send(Ok(fake_viewer("ready"))).ok().unwrap();
        preview.poll();
        assert!(preview.at_navigation_edge(true));
        assert!(preview.pending_navigation_edge.is_none());
        preview.enter_navigation_edge(true);
        assert!(preview.at_navigation_edge(false));

        preview.select(Some(request("b")), None);
        preview.enter_navigation_edge(false);
        assert_eq!(preview.pending_navigation_edge, Some(false));
        preview.select(Some(request("c")), None);
        assert!(preview.pending_navigation_edge.is_none());
    }

    fn git(path: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn fixture() -> (tempfile::TempDir, Repo) {
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "-b", "main"]);
        git(root.path(), &["config", "user.name", "Preview Test"]);
        git(
            root.path(),
            &["config", "user.email", "preview@example.invalid"],
        );
        for name in ["a", "b", "c"] {
            fs::write(root.path().join(name), format!("old_{name}\n")).unwrap();
        }
        git(root.path(), &["add", "."]);
        git(root.path(), &["commit", "-m", "Base"]);
        for name in ["a", "b", "c"] {
            fs::write(root.path().join(name), format!("new_{name}\n")).unwrap();
        }
        let repo = Repo::discover(root.path()).unwrap();
        (root, repo)
    }

    fn request(path: &str) -> Request {
        Request::Working {
            kind: ChangeKind::Unstaged,
            path: path.into(),
        }
    }

    fn fake_viewer(label: &str) -> Viewer {
        Viewer::new(
            DiffData {
                title: label.into(),
                patch: format!("@@ -1 +1 @@\n-old\n+{label}\n"),
                source: None,
            },
            None,
        )
    }

    fn screen(preview: &mut Preview) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal
            .draw(|frame| preview.render(frame, frame.area(), true))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    fn settle(preview: &mut Preview) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while (preview.running.is_some() || preview.wait(Instant::now()).is_some())
            && Instant::now() < deadline
        {
            preview.poll();
            thread::sleep(Duration::from_millis(2));
        }
        assert!(preview.running.is_none(), "preview worker did not finish");
        assert!(
            preview.wait(Instant::now()).is_none(),
            "selection did not settle"
        );
    }

    #[test]
    fn rapid_selection_discards_stale_result_and_reads_only_the_latest_request() {
        let (_root, repo) = fixture();
        let mut preview = Preview::new(repo);
        preview.desired = Some(request("a"));
        preview.revision = 1;
        preview.viewer = Some(fake_viewer("old_a_view"));
        let (sender, result) = mpsc::channel();
        preview.running = Some(Running {
            revision: 1,
            result,
        });

        preview.select(Some(request("b")), None);
        preview.select(Some(request("c")), None);
        // Keep this assertion independent of terminal rendering speed or CI
        // scheduling; the test advances the deadline explicitly below.
        preview.deadline = Some(Instant::now() + Duration::from_secs(60));
        assert!(preview.viewer.is_none());
        assert!(screen(&mut preview).contains("Loading diff"));
        sender.send(Ok(fake_viewer("stale_a_view"))).ok().unwrap();
        preview.poll();
        assert!(
            preview.running.is_none(),
            "completion must respect the settle deadline"
        );
        preview.start_latest(preview.deadline.unwrap());
        assert_eq!(preview.running.as_ref().unwrap().revision, 3);
        assert_eq!(preview.desired, Some(request("c")));
        assert!(preview.viewer.is_none());
        settle(&mut preview);
        let rendered = screen(&mut preview);
        assert!(rendered.contains("new_c"));
        assert!(!rendered.contains("stale_a_view"));
        assert_eq!(preview.settled, Some(3));
        preview.select(Some(request("c")), None);
        assert!(
            preview.running.is_none(),
            "same selection must not reread Git"
        );
    }

    #[test]
    fn invalidation_supersedes_an_inflight_read_even_for_the_same_file() {
        let (root, repo) = fixture();
        let mut preview = Preview::new(repo);
        preview.desired = Some(request("a"));
        preview.revision = 1;
        preview.viewer = Some(fake_viewer("kept_while_refreshing"));
        let (sender, result) = mpsc::channel();
        preview.running = Some(Running {
            revision: 1,
            result,
        });
        fs::write(root.path().join("a"), "latest_a\n").unwrap();
        preview.invalidate(None);
        assert!(screen(&mut preview).contains("kept_while_refreshing"));
        sender
            .send(Ok(fake_viewer("obsolete_refresh")))
            .ok()
            .unwrap();
        preview.poll();
        assert!(screen(&mut preview).contains("kept_while_refreshing"));
        settle(&mut preview);
        let rendered = screen(&mut preview);
        assert!(rendered.contains("latest_a"));
        assert!(!rendered.contains("obsolete_refresh"));
    }

    #[test]
    fn clearing_selection_prevents_an_inflight_result_from_reappearing() {
        let (_root, repo) = fixture();
        let mut preview = Preview::new(repo);
        preview.desired = Some(request("a"));
        preview.revision = 1;
        let (sender, result) = mpsc::channel();
        preview.running = Some(Running {
            revision: 1,
            result,
        });
        preview.select(None, None);
        sender
            .send(Ok(fake_viewer("must_not_reappear")))
            .ok()
            .unwrap();
        preview.poll();
        assert!(!preview.available());
        assert!(preview.viewer.is_none());
        assert!(preview.running.is_none());
        assert!(preview.wait(Instant::now()).is_none());
        assert!(!screen(&mut preview).contains("must_not_reappear"));
    }

    #[test]
    fn failed_reads_settle_until_explicit_retry_and_completion_wakes_the_panel() {
        let (root, repo) = fixture();
        let mut preview = Preview::new(repo);
        let (wake, events) = mpsc::sync_channel(1);
        preview.select(Some(request("a")), Some(wake.clone()));
        preview.start_latest(preview.deadline.unwrap());
        events.recv_timeout(Duration::from_secs(3)).unwrap();
        settle(&mut preview);
        fs::remove_file(root.path().join("a")).unwrap();
        // Git still provides a deletion diff for a removed tracked path.
        preview.invalidate(Some(wake));
        settle(&mut preview);
        assert!(preview.error.is_none());

        let (sender, result) = mpsc::channel();
        preview.revision += 1;
        preview.running = Some(Running {
            revision: preview.revision,
            result,
        });
        sender
            .send(Err(anyhow!("fixture read failure")))
            .ok()
            .unwrap();
        assert!(preview.poll());
        assert!(preview
            .error
            .as_ref()
            .unwrap()
            .contains("fixture read failure"));
        assert!(preview.running.is_none());
        assert!(!preview.poll());
        preview.invalidate(None);
        settle(&mut preview);
        assert!(preview.error.is_none());
    }

    #[test]
    fn selection_deadline_coalesces_navigation_and_disappears_when_idle() {
        let (_root, repo) = fixture();
        let mut preview = Preview::new(repo);
        assert!(preview.wait(Instant::now()).is_none());
        for file in ["a", "b", "c"] {
            assert!(preview.select(Some(request(file)), None));
            preview.start_latest(preview.deadline.unwrap() - Duration::from_millis(1));
            assert!(preview.running.is_none());
        }
        let deadline = preview.deadline.unwrap();
        let before = deadline - Duration::from_millis(1);
        assert_eq!(preview.wait(before), Some(Duration::from_millis(1)));
        preview.start_latest(before);
        assert!(preview.running.is_none());
        assert!(!preview.select(Some(request("c")), None));
        assert_eq!(
            preview.deadline,
            Some(deadline),
            "unchanged selection must not postpone loading"
        );
        preview.start_latest(deadline);
        assert_eq!(preview.desired, Some(request("c")));
        assert!(preview.running.is_some());
        assert!(
            preview.wait(deadline).is_none(),
            "running read wakes on completion"
        );
        settle(&mut preview);
        assert!(screen(&mut preview).contains("new_c"));
        assert!(preview.deadline.is_none());
        assert!(preview.wait(Instant::now()).is_none());
    }

    #[test]
    fn explicit_retry_bypasses_the_selection_delay() {
        let (_root, repo) = fixture();
        let mut preview = Preview::new(repo);
        preview.select(Some(request("a")), None);
        assert!(preview.running.is_none());
        assert!(!preview.key(KeyEvent::from(KeyCode::Char('r'))));
        assert!(preview.running.is_some());
        assert!(preview.deadline.is_none());
        settle(&mut preview);
        assert!(screen(&mut preview).contains("new_a"));
    }

    #[test]
    fn selecting_lines_defers_a_refresh_until_the_selection_is_released() {
        let (root, repo) = fixture();
        let mut preview = Preview::new(repo);
        preview.select(Some(request("a")), None);
        settle(&mut preview);
        preview.key(KeyEvent::from(KeyCode::Char('v')));
        fs::write(root.path().join("a"), "changed_during_review\n").unwrap();
        preview.invalidate(None);
        preview.start_latest(preview.deadline.unwrap());
        assert!(preview.running.is_none());
        assert!(preview.wait(Instant::now()).is_none());
        assert!(screen(&mut preview).contains("new_a"));
        assert!(!preview.key(KeyEvent::from(KeyCode::Esc)));
        preview.start_latest(preview.deadline.unwrap());
        settle(&mut preview);
        assert!(screen(&mut preview).contains("changed_during_review"));
    }
}
