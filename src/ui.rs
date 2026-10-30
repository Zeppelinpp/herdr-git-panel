use crate::{
    diff_view::Viewer,
    git::{matching_branches, ChangeKind, DiscardPlan, FileStat, Mr, Repo, Snapshot},
    history::{Action as HistoryAction, History, Identity as GraphSelection, RowHit},
    host::{self, Config, DiffData},
    preview::{Preview, Request as PreviewRequest, MIN_HEIGHT as MIN_PREVIEW_HEIGHT},
    theme::{Theme, INPUT_CURSOR, NAVIGATION},
    watch::{Changes, RepoWatch},
};
use anyhow::{bail, Result};
use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
        MouseButton, MouseEvent, MouseEventKind, PopKeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    },
    execute,
};
use ratatui::{
    backend::Backend,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap},
    DefaultTerminal, Frame, Terminal,
};
use std::{
    collections::HashMap,
    env, io,
    path::PathBuf,
    process::{Command, Stdio},
    sync::mpsc::{self, Receiver, Sender, SyncSender},
    thread::{self, JoinHandle},
    time::Instant,
};
use tui_textarea::TextArea;

const NORMAL: Style = Style::new();
const BOLD: Style = Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD);
const DIM: Style = Style::new().fg(Color::Gray).add_modifier(Modifier::DIM);
const SELECTED: Style = Style::new().add_modifier(Modifier::REVERSED);
const ACTION_LABELS: [&str; 3] = ["[Commit]", "[Push]", "[Create PR/MR]"];

struct ActionBarLayout {
    buttons: [Rect; 3],
    link: Rect,
    height: u16,
}

impl ActionBarLayout {
    fn new(area: Rect, labels: [&str; 3], link: Option<&str>) -> Self {
        let width = |text: &str| {
            u16::try_from(unicode_width::UnicodeWidthStr::width(text)).unwrap_or(u16::MAX)
        };
        let widths = labels.map(width);
        let gap = 3;
        let prefix = widths[0].saturating_add(widths[1]).saturating_add(gap * 2);
        let link_space = link.map_or(0, |text| width(text).saturating_add(1));
        let wrapped = area.width < prefix.saturating_add(widths[2]).saturating_add(link_space);
        let columns = Layout::horizontal([
            Constraint::Length(widths[0].saturating_add(gap)),
            Constraint::Length(widths[1].saturating_add(gap)),
            Constraint::Min(0),
        ])
        .split(Rect::new(area.x, area.y, area.width, 1));
        let last_row = if wrapped {
            Rect::new(area.x, area.y.saturating_add(1), area.width, 1)
        } else {
            columns[2]
        };
        let rows = [columns[0], columns[1], last_row];
        let buttons = std::array::from_fn(|i| {
            Rect::new(rows[i].x, rows[i].y, widths[i].min(rows[i].width), 1)
        });
        let link_offset = widths[2].saturating_add(1).min(last_row.width);
        let link = Rect::new(
            last_row.x.saturating_add(link_offset),
            last_row.y,
            last_row
                .width
                .saturating_sub(link_offset)
                .min(link.map_or(0, width)),
            1,
        );
        Self {
            buttons,
            link,
            height: if wrapped { 2 } else { 1 },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Base,
    Title,
    Description,
    Stage,
    Commit,
    Push,
    Mr,
    WorkingFiles,
    BranchFiles,
    Diff,
    HistoryToggle,
    Graph,
    More,
    Link,
}
const FOCI: [Focus; 14] = [
    Focus::Base,
    Focus::Title,
    Focus::Description,
    Focus::Stage,
    Focus::Commit,
    Focus::Push,
    Focus::Mr,
    Focus::Link,
    Focus::WorkingFiles,
    Focus::BranchFiles,
    Focus::Diff,
    Focus::HistoryToggle,
    Focus::Graph,
    Focus::More,
];
#[derive(Clone, Debug)]
enum Hit {
    Focus(Focus),
    BranchRow(usize),
    GraphRow(RowHit),
    FileRow(Focus, usize),
    ModalField(usize),
    Submit,
    Cancel,
    Refresh,
    ToggleGraph,
}
struct BranchDialog {
    query: TextArea<'static>,
    selected: usize,
    state: ListState,
}
struct MrDialog {
    fields: Vec<TextArea<'static>>,
    selected: usize,
}
enum Dialog {
    Branches(Box<BranchDialog>),
    Mr(MrDialog),
    Push(String, String),
    Diff(Box<Viewer>),
    Discard(Box<DiscardPlan>),
}
enum AutoData {
    Snapshot(Box<Snapshot>),
    Working(String, crate::git::WorkingTree),
}
enum Update {
    Automatic(Option<AutoData>),
    Snapshot(Box<Snapshot>),
    Details(String, String, Vec<FileStat>),
    Committed(String),
    Pushed(String),
    Done,
    Mr(String),
    Diff(DiffData),
    DiscardReady(Box<DiscardPlan>),
    FileAction(Box<FileActionResult>),
}
enum FileAction {
    ToggleStage(ChangeKind, String),
    PrepareDiscard(String),
    Discard(Box<DiscardPlan>),
}
struct FileActionResult {
    status: String,
    working: crate::git::WorkingTree,
    message: String,
    preferred: Option<(ChangeKind, String)>,
}
enum Activation {
    Focus(Focus),
    Graph(GraphSelection),
    Working(ChangeKind, String),
    Branch(String),
}
#[derive(Default)]
struct FileCache {
    valid: bool,
    working: List<'static>,
    branch: List<'static>,
}

pub struct Panel {
    repo: Repo,
    theme: Theme,
    cfg: Config,
    // Snapshot.graph moves into History when a complete snapshot is ingested.
    snapshot: Option<Snapshot>,
    title: TextArea<'static>,
    description: TextArea<'static>,
    focus: Focus,
    stage: bool,
    message: String,
    last_mr: String,
    dialog: Option<Dialog>,
    history: History,
    preview: Preview,
    preview_origin: Focus,
    preview_area: Rect,
    preview_refresh_pending: bool,
    file_cache: FileCache,
    working_files: ListState,
    branch_files: ListState,
    limit: usize,
    hits: Vec<(Rect, Hit)>,
    sender: Sender<Result<Update>>,
    receiver: Receiver<Result<Update>>,
    worker: Option<JoinHandle<()>>,
    refresh_notice: Option<String>,
    pending_refresh: bool,
    pending_activation: Option<Activation>,
    pending_file_action: Option<FileAction>,
    discard_visible: bool,
    file_action_running: bool,
    background_refresh: bool,
    wake: Option<SyncSender<()>>,
    watch_notice: Option<String>,
    dirty: bool,
}
fn textarea(value: &str) -> TextArea<'static> {
    let mut area = TextArea::from(value.lines().map(str::to_string).collect::<Vec<_>>());
    area.set_style(NORMAL);
    area.set_cursor_line_style(NORMAL);
    area.set_selection_style(SELECTED);
    area.set_placeholder_style(DIM);
    area
}
fn text(area: &TextArea<'_>) -> String {
    area.lines().join("\n")
}
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
fn trim_output(value: &str) -> String {
    value
        .lines()
        .filter(|s| !s.trim().is_empty())
        .take(3)
        .collect::<Vec<_>>()
        .join(" · ")
}
fn paint_input(frame: &mut Frame, area: Rect, input: &mut TextArea<'_>, title: &str, active: bool) {
    input.set_block(
        Block::bordered()
            .title(title.to_string())
            .border_style(if active {
                BOLD
            } else {
                NORMAL.fg(Color::DarkGray)
            }),
    );
    input.set_cursor_style(if active { INPUT_CURSOR } else { NORMAL });
    frame.render_widget(&*input, area);
}
fn footer(frame: &mut Frame, area: Rect, text: &str) {
    frame.render_widget(Paragraph::new(text.to_string()).style(DIM), area);
}
fn dialog_area(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width.saturating_sub(2));
    let h = height.min(area.height.saturating_sub(2));
    Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    )
}
fn move_list(state: &mut ListState, count: usize, delta: isize) {
    if count == 0 {
        state.select(None);
        return;
    }
    let i = state
        .selected()
        .unwrap_or(0)
        .saturating_add_signed(delta)
        .min(count - 1);
    state.select(Some(i));
}
pub fn key_matches(event: KeyEvent, binding: &str) -> bool {
    let mut mods = KeyModifiers::empty();
    let mut code = None;
    for token in binding.to_lowercase().split('+') {
        match token {
            "ctrl" | "control" => mods |= KeyModifiers::CONTROL,
            "alt" => mods |= KeyModifiers::ALT,
            "shift" => mods |= KeyModifiers::SHIFT,
            "cmd" | "super" => mods |= KeyModifiers::SUPER,
            "enter" => code = Some(KeyCode::Enter),
            "space" => code = Some(KeyCode::Char(' ')),
            "" => return false,
            key if key.len() == 1 => code = key.chars().next().map(KeyCode::Char),
            key if key.starts_with('f') => code = key[1..].parse().ok().map(KeyCode::F),
            _ => return false,
        }
    }
    let actual = if let KeyCode::Char(c) = event.code {
        KeyCode::Char(c.to_ascii_lowercase())
    } else {
        event.code
    };
    code == Some(actual) && event.modifiers == mods
}
impl Panel {
    pub fn new(repo: Repo, cfg: Config) -> Self {
        let (sender, receiver) = mpsc::channel();
        let limit = cfg.history_limit;
        let mut title = textarea("");
        title.set_placeholder_text("Commit title");
        let mut description = textarea("");
        description.set_placeholder_text("Description");
        let preview = Preview::new(repo.clone());
        Self {
            repo,
            theme: Theme::load().unwrap_or_default(),
            cfg,
            snapshot: None,
            title,
            description,
            focus: Focus::Base,
            stage: true,
            message: String::new(),
            last_mr: String::new(),
            dialog: None,
            history: History::default(),
            preview,
            preview_origin: Focus::WorkingFiles,
            preview_area: Rect::default(),
            preview_refresh_pending: false,
            file_cache: FileCache::default(),
            working_files: ListState::default().with_selected(Some(0)),
            branch_files: ListState::default().with_selected(Some(0)),
            limit,
            hits: Vec::new(),
            sender,
            receiver,
            worker: None,
            refresh_notice: None,
            pending_refresh: false,
            pending_activation: None,
            pending_file_action: None,
            discard_visible: false,
            file_action_running: false,
            background_refresh: false,
            wake: None,
            watch_notice: None,
            dirty: true,
        }
    }
    fn busy(&self) -> bool {
        self.worker.is_some()
    }
    fn work(&mut self, label: &str, job: impl FnOnce() -> Result<Update> + Send + 'static) {
        if self.busy() {
            return;
        }
        self.message = label.into();
        self.dirty = true;
        self.spawn(job);
    }
    fn spawn(&mut self, job: impl FnOnce() -> Result<Update> + Send + 'static) {
        let sender = self.sender.clone();
        let wake = self.wake.clone();
        self.worker = Some(thread::spawn(move || {
            let _ = sender.send(job());
            if let Some(wake) = wake {
                let _ = wake.try_send(());
            }
        }));
    }
    pub fn refresh(&mut self) {
        if self.busy() {
            self.pending_refresh = true;
            return;
        }
        self.pending_refresh = false;
        self.dirty = true;
        match Theme::load() {
            Ok(theme) => self.theme = theme,
            Err(error) => {
                self.message = format!("Could not read Herdr theme: {error:#}");
                return;
            }
        }
        let repo = self.repo.clone();
        let base = self
            .snapshot
            .as_ref()
            .map(|s| s.base.clone())
            .unwrap_or_default();
        let limit = self.limit;
        self.work("Reading local repository…", move || {
            Ok(Update::Snapshot(Box::new(repo.snapshot(&base, limit)?)))
        });
    }
    fn receive(&mut self) {
        self.dirty |= self.preview.poll();
        if self.preview_refresh_pending && !self.preview.review_active() {
            self.preview_refresh_pending = false;
            self.refresh_preview();
        }
        if let Some(Dialog::Diff(viewer)) = &mut self.dialog {
            self.dirty |= viewer.poll();
        }
        let Ok(result) = self.receiver.try_recv() else {
            return;
        };
        self.dirty |= !matches!(&result, Ok(Update::Automatic(_)));
        self.background_refresh = false;
        self.file_action_running = false;
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        match result {
            Err(e) => self.message = format!("{e:#}"),
            Ok(Update::Snapshot(snapshot)) => {
                self.replace_snapshot(*snapshot);
                self.refresh_preview();
                self.message = self.refresh_notice.take().unwrap_or_default();
            }
            Ok(Update::Automatic(data)) => match data {
                Some(AutoData::Snapshot(snapshot)) => {
                    self.replace_snapshot(*snapshot);
                    self.refresh_preview();
                }
                Some(AutoData::Working(status, working)) => {
                    self.replace_working(status, working);
                    if self.preview_origin == Focus::WorkingFiles {
                        self.refresh_preview();
                    } else {
                        self.sync_preview();
                    }
                }
                None => (),
            },
            Ok(Update::Details(sha, summary, files)) => {
                self.history.expand(sha, summary, files);
                self.sync_preview();
                self.message.clear();
            }
            Ok(Update::Committed(out)) => {
                self.title = textarea("");
                self.description = textarea("");
                self.message = trim_output(&out);
                self.refresh_after_operation();
            }
            Ok(Update::Pushed(out)) => {
                self.message = trim_output(&out);
                self.refresh_after_operation();
            }
            Ok(Update::Mr(url)) => {
                self.last_mr = url;
                self.message = "Merge request created".into();
            }
            Ok(Update::Diff(data)) => {
                if env::var_os("HERDR_SOCKET_PATH").is_some() {
                    self.work("Opening diff popup…", move || {
                        host::open_diff(&data)?;
                        Ok(Update::Done)
                    });
                } else {
                    self.message.clear();
                    self.dialog =
                        Some(Dialog::Diff(Box::new(Viewer::new(data, self.wake.clone()))));
                }
            }
            Ok(Update::Done) => self.message.clear(),
            Ok(Update::DiscardReady(plan)) => {
                self.message.clear();
                self.discard_visible = false;
                self.dialog = Some(Dialog::Discard(plan));
            }
            Ok(Update::FileAction(result)) => {
                let FileActionResult {
                    status,
                    working,
                    message,
                    preferred,
                } = *result;
                let selected_path = self.working_key().map(|(_, path)| path);
                self.replace_working(status, working);
                if let (Some((kind, path)), Some(snapshot)) = (preferred, &self.snapshot) {
                    let files = &snapshot.working.files;
                    if selected_path.as_ref() == Some(&path) {
                        if let Some(index) = files
                            .iter()
                            .position(|f| f.kind == kind && f.stat.path == path)
                            .or_else(|| files.iter().position(|f| f.stat.path == path))
                        {
                            self.working_files.select(Some(index));
                        }
                    }
                }
                self.message = message;
                if !self.sync_preview() {
                    self.preview.invalidate(self.wake.clone());
                }
            }
        }
        if !self.busy() {
            if let Some(action) = self.pending_file_action.take() {
                self.run_file_action(action);
            }
        }
        if !self.busy() {
            if let Some(action) = self.pending_activation.take() {
                if let Err(error) = self.resume_activation(action) {
                    self.message = error.to_string();
                }
            }
        }
        if self.pending_refresh && !self.busy() {
            self.refresh();
        }
    }
    fn auto_refresh(&mut self, changes: Changes) {
        debug_assert!(!self.busy());
        // Recover from a failed initial read using the next filesystem event.
        // A partial result has nowhere to apply until the first snapshot exists.
        let changes = if self.snapshot.is_none() {
            Changes::Repository
        } else {
            changes
        };
        let repo = self.repo.clone();
        let base = self
            .snapshot
            .as_ref()
            .map(|s| s.base.clone())
            .unwrap_or_default();
        let limit = self.limit;
        self.background_refresh = true;
        self.spawn(move || {
            if !changes.relevant(&repo)? {
                return Ok(Update::Automatic(None));
            }
            let data = match changes {
                Changes::Repository => AutoData::Snapshot(Box::new(repo.snapshot(&base, limit)?)),
                Changes::Worktree(_) => {
                    let (status, working) = repo.worktree_snapshot()?;
                    AutoData::Working(status, working)
                }
            };
            Ok(Update::Automatic(Some(data)))
        });
    }
    fn replace_snapshot(&mut self, mut snapshot: Snapshot) {
        let history_changed = self.history.replace(std::mem::take(&mut snapshot.graph));
        if self.snapshot.as_ref() == Some(&snapshot) && !history_changed {
            return;
        }
        let branch_path = self
            .snapshot
            .as_ref()
            .and_then(|old| old.files.get(self.branch_files.selected().unwrap_or(0)))
            .map(|file| file.path.clone());
        let working_key = self.working_key();
        let working_index = self.working_files.selected().unwrap_or(0);
        let branch_index = self.branch_files.selected().unwrap_or(0);
        self.snapshot = Some(snapshot);
        self.restore_working_selection(working_key, working_index);
        let files = &self.snapshot.as_ref().unwrap().files;
        let selected = branch_path
            .and_then(|path| files.iter().position(|file| file.path == path))
            .or_else(|| (!files.is_empty()).then(|| branch_index.min(files.len() - 1)));
        self.branch_files.select(selected);
        self.file_cache.valid = false;
        self.dirty = true;
    }
    fn working_key(&self) -> Option<(ChangeKind, String)> {
        self.snapshot
            .as_ref()?
            .working
            .files
            .get(self.working_files.selected()?)
            .map(|file| (file.kind, file.stat.path.clone()))
    }

    fn run_file_action(&mut self, action: FileAction) {
        if self.busy() {
            if self.pending_file_action.is_none()
                && (self.background_refresh || matches!(action, FileAction::Discard(_)))
            {
                self.pending_file_action = Some(action);
                self.pending_activation = None;
            }
            return;
        }
        let repo = self.repo.clone();
        let label = match &action {
            FileAction::ToggleStage(ChangeKind::Staged, _) => "Unstaging file…",
            FileAction::ToggleStage(_, _) => "Staging file…",
            FileAction::PrepareDiscard(_) => "Checking file changes…",
            FileAction::Discard(_) => "Discarding file changes…",
        };
        self.file_action_running = true;
        self.work(label, move || {
            let (outcome, preferred, success) = match action {
                FileAction::PrepareDiscard(path) => {
                    return Ok(Update::DiscardReady(Box::new(repo.prepare_discard(&path)?)))
                }
                FileAction::ToggleStage(kind, path) => {
                    let result = repo.toggle_stage(kind, &path);
                    let next = if kind == ChangeKind::Staged {
                        ChangeKind::Unstaged
                    } else {
                        ChangeKind::Staged
                    };
                    (
                        result,
                        Some((next, path)),
                        if next == ChangeKind::Staged {
                            "File staged"
                        } else {
                            "File unstaged"
                        },
                    )
                }
                FileAction::Discard(plan) => (repo.discard(&plan), None, "File changes discarded"),
            };
            let message = outcome
                .map(|_| success.to_string())
                .unwrap_or_else(|error| format!("{error:#}"));
            let (status, working) = repo.worktree_snapshot().map_err(|error| {
                anyhow::anyhow!("{message}; refresh failed: {error:#}. Press Ctrl+R to refresh")
            })?;
            Ok(Update::FileAction(Box::new(FileActionResult {
                status,
                working,
                message,
                preferred,
            })))
        });
    }

    fn sync_preview(&mut self) -> bool {
        self.preview_refresh_pending = false;
        let working = self
            .working_key()
            .map(|(kind, path)| PreviewRequest::Working { kind, path });
        let branch = self.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .files
                .get(self.branch_files.selected()?)
                .map(|file| PreviewRequest::Branch {
                    base: snapshot.base.clone(),
                    path: file.path.clone(),
                })
        });
        let request = match self.preview_origin {
            Focus::Graph => match self.history.selection() {
                Some(GraphSelection::File(sha, path)) => Some(PreviewRequest::Commit { sha, path }),
                _ => None,
            },
            Focus::BranchFiles if branch.is_some() => branch,
            _ if working.is_some() => {
                self.preview_origin = Focus::WorkingFiles;
                working
            }
            _ => {
                self.preview_origin = Focus::BranchFiles;
                branch
            }
        };
        self.preview.select(request, self.wake.clone())
    }

    fn refresh_preview(&mut self) {
        if self.preview.review_active() {
            self.preview_refresh_pending = true;
            return;
        }
        // Unchanged statistics can still contain different text after a save.
        if !self.sync_preview() {
            self.preview.invalidate(self.wake.clone());
        }
    }

    fn focus_preview(&mut self, origin: Focus) {
        self.preview_origin = origin;
        self.sync_preview();
        if self.can_focus(Focus::Diff) {
            self.focus = Focus::Diff;
        }
    }

    fn leave_preview(&mut self) {
        self.focus = self.preview_origin;
        if !self.can_focus(self.focus) {
            self.change_focus(false);
        }
    }
    fn restore_working_selection(&mut self, key: Option<(ChangeKind, String)>, old_index: usize) {
        let files = &self.snapshot.as_ref().unwrap().working.files;
        let selected = key
            .and_then(|(kind, path)| {
                files
                    .iter()
                    .position(|file| file.kind == kind && file.stat.path == path)
            })
            .or_else(|| (!files.is_empty()).then(|| old_index.min(files.len() - 1)));
        self.working_files.select(selected);
    }
    fn replace_working(&mut self, status: String, working: crate::git::WorkingTree) {
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        if snapshot.status == status && snapshot.working == working {
            return;
        }
        let key = self.working_key();
        let index = self.working_files.selected().unwrap_or(0);
        let snapshot = self.snapshot.as_mut().unwrap();
        snapshot.status = status;
        snapshot.working = working;
        self.restore_working_selection(key, index);
        self.file_cache.valid = false;
        self.dirty = true;
    }
    fn refresh_after_operation(&mut self) {
        self.refresh_notice = Some(self.message.clone());
        self.refresh();
    }
    fn choose_branch(&mut self, name: String) {
        if let Some(snapshot) = &mut self.snapshot {
            snapshot.base = name;
        }
        self.refresh();
    }
    fn change_focus(&mut self, backward: bool) {
        let index = FOCI.iter().position(|f| *f == self.focus).unwrap_or(0);
        for step in 1..=FOCI.len() {
            let next = if backward {
                (index + FOCI.len() - step) % FOCI.len()
            } else {
                (index + step) % FOCI.len()
            };
            let focus = FOCI[next];
            if !self.can_focus(focus) {
                continue;
            }
            self.focus = focus;
            if matches!(
                focus,
                Focus::WorkingFiles | Focus::BranchFiles | Focus::Graph
            ) {
                self.preview_origin = focus;
                self.sync_preview();
            }
            break;
        }
    }
    fn can_focus(&self, focus: Focus) -> bool {
        if self.busy()
            && !self.background_refresh
            && !self.file_action_running
            && focus != Focus::Diff
        {
            return false;
        }
        match focus {
            Focus::Title | Focus::Description | Focus::Stage => true,
            Focus::Base => self
                .snapshot
                .as_ref()
                .is_some_and(|s| !s.branches.is_empty()),
            Focus::Commit => {
                !text(&self.title).trim().is_empty()
                    && self.snapshot.as_ref().is_some_and(|s| {
                        !s.branch.is_empty()
                            && s.working
                                .files
                                .iter()
                                .any(|f| self.stage || f.kind == ChangeKind::Staged)
                    })
            }
            Focus::Push => self
                .snapshot
                .as_ref()
                .is_some_and(|s| !s.branch.is_empty() && s.push.is_some()),
            Focus::Mr => {
                self.snapshot
                    .as_ref()
                    .is_some_and(|s| !s.branch.is_empty() && !s.remotes.is_empty())
                    && self.history.count() > 0
            }
            Focus::WorkingFiles => self
                .snapshot
                .as_ref()
                .is_some_and(|s| !s.working.files.is_empty()),
            Focus::BranchFiles => self.snapshot.as_ref().is_some_and(|s| !s.files.is_empty()),
            Focus::Diff => {
                self.preview.available()
                    && (self.preview.captures_input()
                        || self.preview_area.height >= MIN_PREVIEW_HEIGHT)
            }
            Focus::HistoryToggle => self.history.count() > 0,
            Focus::Graph => !self.history.folded() && self.history.count() > 0,
            Focus::More => !self.history.folded() && self.history.count() >= self.limit,
            Focus::Link => !self.last_mr.is_empty(),
        }
    }
    fn toggle_history(&mut self) {
        self.history.toggle();
        self.focus = if self.history.folded() {
            Focus::HistoryToggle
        } else {
            Focus::Graph
        };
    }
    fn select_graph(&mut self) {
        match self.history.activate() {
            Some(HistoryAction::ReadCommit(sha)) => {
                let repo = self.repo.clone();
                self.work("Reading commit…", move || {
                    let (summary, files) = repo.stats("", &sha)?;
                    Ok(Update::Details(sha, summary, files))
                });
            }
            Some(HistoryAction::OpenFile { .. }) => self.focus_preview(Focus::Graph),
            None => (),
        }
    }
    fn open_file(&mut self, index: usize) {
        let data = self.snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .files
                .get(index)
                .map(|file| (snapshot.base.clone(), file.path.clone()))
        });
        if let Some((base, path)) = data {
            self.open_patch(base, String::new(), path);
        }
    }
    fn open_patch(&mut self, base: String, sha: String, path: String) {
        let request = if sha.is_empty() {
            PreviewRequest::Branch { base, path }
        } else {
            PreviewRequest::Commit { sha, path }
        };
        self.open_diff(request);
    }
    fn open_working_file(&mut self, index: usize) {
        let file = self
            .snapshot
            .as_ref()
            .and_then(|s| s.working.files.get(index))
            .cloned();
        let Some(file) = file else {
            return;
        };
        self.open_diff(PreviewRequest::Working {
            kind: file.kind,
            path: file.stat.path,
        });
    }

    fn open_diff(&mut self, request: PreviewRequest) {
        let repo = self.repo.clone();
        self.work("Reading diff…", move || {
            Ok(Update::Diff(request.read(&repo)?))
        });
    }
    fn capture_activation(&self) -> Option<Activation> {
        match self.focus {
            Focus::Graph => self.history.selection().map(Activation::Graph),
            Focus::WorkingFiles => self
                .working_key()
                .map(|(kind, path)| Activation::Working(kind, path)),
            Focus::BranchFiles => self
                .snapshot
                .as_ref()?
                .files
                .get(self.branch_files.selected()?)
                .map(|file| Activation::Branch(file.path.clone())),
            focus => Some(Activation::Focus(focus)),
        }
    }
    fn resume_activation(&mut self, action: Activation) -> Result<()> {
        let focus = match action {
            Activation::Focus(focus) => focus,
            Activation::Graph(identity) => {
                if !self.history.select(&identity) {
                    return Ok(());
                }
                Focus::Graph
            }
            Activation::Working(kind, path) => {
                let Some(index) = self.snapshot.as_ref().and_then(|s| {
                    s.working
                        .files
                        .iter()
                        .position(|file| file.kind == kind && file.stat.path == path)
                }) else {
                    return Ok(());
                };
                self.working_files.select(Some(index));
                Focus::WorkingFiles
            }
            Activation::Branch(path) => {
                let Some(index) = self
                    .snapshot
                    .as_ref()
                    .and_then(|s| s.files.iter().position(|file| file.path == path))
                else {
                    return Ok(());
                };
                self.branch_files.select(Some(index));
                Focus::BranchFiles
            }
        };
        if self.can_focus(focus) {
            self.focus = focus;
            self.activate()?;
        }
        Ok(())
    }
    fn activate(&mut self) -> Result<()> {
        if self.busy() {
            if self.background_refresh {
                self.pending_activation = self.capture_activation();
            }
            return Ok(());
        }
        match self.focus {
            Focus::Base => {
                self.dialog = Some(Dialog::Branches(Box::new(BranchDialog {
                    query: textarea(""),
                    selected: 0,
                    state: ListState::default().with_selected(Some(0)),
                })))
            }
            Focus::Stage => self.stage = !self.stage,
            Focus::Commit => {
                let repo = self.repo.clone();
                let title = text(&self.title);
                let body = text(&self.description);
                let stage = self.stage;
                self.work("Committing…", move || {
                    Ok(Update::Committed(repo.commit(&title, &body, stage)?))
                });
            }
            Focus::Push => {
                if let Some((remote, target)) = self.snapshot.as_ref().and_then(|s| s.push.clone())
                {
                    self.dialog = Some(Dialog::Push(remote, target));
                } else {
                    self.message = "Configure an upstream or origin for the selected branch".into();
                }
            }
            Focus::Mr => {
                let snapshot = self
                    .snapshot
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("Repository is loading"))?;
                let fallback = if snapshot.remotes.contains(&self.cfg.default_remote) {
                    self.cfg.default_remote.clone()
                } else {
                    snapshot.remotes.first().cloned().unwrap_or_default()
                };
                let (remote, target) = snapshot
                    .push
                    .clone()
                    .unwrap_or((fallback, snapshot.base.clone()));
                self.dialog = Some(Dialog::Mr(MrDialog {
                    fields: vec![
                        textarea(&remote),
                        textarea(&snapshot.branch),
                        textarea(&target),
                        textarea(&text(&self.title)),
                        textarea(&text(&self.description)),
                    ],
                    selected: 0,
                }));
            }
            Focus::Graph => self.select_graph(),
            Focus::HistoryToggle => self.toggle_history(),
            Focus::BranchFiles => self.focus_preview(Focus::BranchFiles),
            Focus::WorkingFiles => self.focus_preview(Focus::WorkingFiles),
            Focus::More => {
                self.limit += self.cfg.history_limit;
                self.refresh();
            }
            Focus::Link => open_url(&self.last_mr)?,
            _ => (),
        }
        Ok(())
    }
    fn navigate(&mut self, delta: isize) {
        match self.focus {
            Focus::Graph => {
                self.history.navigate(delta);
            }
            Focus::WorkingFiles => move_list(
                &mut self.working_files,
                self.snapshot.as_ref().map_or(0, |s| s.working.files.len()),
                delta,
            ),
            Focus::BranchFiles => move_list(
                &mut self.branch_files,
                self.snapshot.as_ref().map_or(0, |s| s.files.len()),
                delta,
            ),
            _ => (),
        }
        if matches!(
            self.focus,
            Focus::WorkingFiles | Focus::BranchFiles | Focus::Graph
        ) {
            self.preview_origin = self.focus;
            self.sync_preview();
        }
    }

    fn jump_navigation_focus(&mut self, forward: bool) {
        let Some(current) = FOCI.iter().position(|focus| *focus == self.focus) else {
            return;
        };
        for distance in 1..FOCI.len() {
            let index = if forward {
                current
                    .checked_add(distance)
                    .filter(|index| *index < FOCI.len())
            } else {
                current.checked_sub(distance)
            };
            let Some(index) = index else {
                break;
            };
            let focus = FOCI[index];
            if matches!(focus, Focus::Title | Focus::Description) || !self.can_focus(focus) {
                continue;
            }
            self.focus = focus;
            match focus {
                Focus::WorkingFiles => {
                    let count = self.snapshot.as_ref().map_or(0, |s| s.working.files.len());
                    self.working_files.select(Some(if forward {
                        0
                    } else {
                        count.saturating_sub(1)
                    }));
                }
                Focus::BranchFiles => {
                    let count = self.snapshot.as_ref().map_or(0, |s| s.files.len());
                    self.branch_files.select(Some(if forward {
                        0
                    } else {
                        count.saturating_sub(1)
                    }));
                }
                Focus::Graph => {
                    self.history
                        .navigate(if forward { isize::MIN } else { isize::MAX });
                }
                Focus::Diff => self.preview.enter_navigation_edge(forward),
                _ => (),
            }
            if matches!(
                focus,
                Focus::WorkingFiles | Focus::BranchFiles | Focus::Graph
            ) {
                self.preview_origin = focus;
                self.sync_preview();
            }
            break;
        }
    }

    fn navigate_across(&mut self, delta: isize) {
        let forward = delta > 0;
        let (selected, count) = match self.focus {
            Focus::WorkingFiles => (
                self.working_files.selected(),
                self.snapshot.as_ref().map_or(0, |s| s.working.files.len()),
            ),
            Focus::BranchFiles => (
                self.branch_files.selected(),
                self.snapshot.as_ref().map_or(0, |s| s.files.len()),
            ),
            Focus::Graph => {
                let moved = self.history.navigate(delta);
                self.preview_origin = Focus::Graph;
                self.sync_preview();
                if !moved {
                    self.jump_navigation_focus(forward);
                }
                return;
            }
            _ => {
                self.jump_navigation_focus(forward);
                return;
            }
        };
        let at_edge = if forward {
            selected.unwrap_or(0) >= count.saturating_sub(1)
        } else {
            selected.unwrap_or(0) == 0
        };
        if at_edge {
            self.jump_navigation_focus(forward);
        } else {
            self.navigate(delta);
        }
    }
    pub fn key(&mut self, key: KeyEvent) -> Result<bool> {
        if let Some(mut dialog) = self.dialog.take() {
            let close = self.dialog_key(&mut dialog, key)?;
            if !close {
                self.dialog = Some(dialog);
            }
            return Ok(false);
        }
        if self.focus == Focus::Diff
            && (self.preview.captures_input()
                || matches!(key.code, KeyCode::Esc | KeyCode::Char('q')))
        {
            if self.preview.key(key) {
                self.leave_preview();
            }
            return Ok(false);
        }
        if key.code == KeyCode::Esc || key_matches(key, "ctrl+c") {
            if self.busy() {
                self.message = "Wait for the active operation to finish before closing".into();
                return Ok(false);
            }
            return Ok(true);
        }
        if key.code == KeyCode::Tab {
            self.change_focus(false);
            return Ok(false);
        }
        if key.code == KeyCode::BackTab {
            self.change_focus(true);
            return Ok(false);
        }
        if key_matches(key, &self.cfg.push_key) {
            self.focus = Focus::Push;
            self.activate()?;
            return Ok(false);
        }
        if key_matches(key, "ctrl+r") {
            self.refresh();
            return Ok(false);
        }
        if key_matches(key, "ctrl+s") {
            self.focus = Focus::Commit;
            self.activate()?;
            return Ok(false);
        }
        if self.focus == Focus::Diff {
            if key_matches(key, "shift+enter") {
                if let Some(request) = self.preview.request() {
                    self.open_diff(request);
                }
                return Ok(false);
            }
            let forward = match key.code {
                KeyCode::Char('j') | KeyCode::Down => Some(true),
                KeyCode::Char('k') | KeyCode::Up => Some(false),
                _ => None,
            };
            if forward.is_some_and(|forward| self.preview.at_navigation_edge(forward)) {
                self.jump_navigation_focus(forward.unwrap());
            } else if self.preview.key(key) {
                self.leave_preview();
            }
            return Ok(false);
        }
        if self.busy() && !self.background_refresh && !self.file_action_running {
            return Ok(false);
        }
        if self.focus == Focus::WorkingFiles && key.modifiers.is_empty() {
            if let Some((kind, path)) = self.working_key() {
                match key.code {
                    KeyCode::Char(' ') => {
                        self.run_file_action(FileAction::ToggleStage(kind, path));
                        return Ok(false);
                    }
                    KeyCode::Char('d') => {
                        self.run_file_action(FileAction::PrepareDiscard(path));
                        return Ok(false);
                    }
                    _ => (),
                }
            }
        }
        if key.code == KeyCode::Char('o')
            && matches!(
                self.focus,
                Focus::WorkingFiles | Focus::BranchFiles | Focus::Graph
            )
        {
            match self.focus {
                Focus::WorkingFiles => {
                    self.open_working_file(self.working_files.selected().unwrap_or(0))
                }
                Focus::BranchFiles => self.open_file(self.branch_files.selected().unwrap_or(0)),
                Focus::Graph => {
                    if let Some(GraphSelection::File(sha, path)) = self.history.selection() {
                        self.open_patch(String::new(), sha, path);
                    }
                }
                _ => (),
            }
            return Ok(false);
        }
        match self.focus {
            Focus::Title if key.code != KeyCode::Enter => {
                self.title.input(key);
            }
            Focus::Description => {
                self.description.input(key);
            }
            _ => match key.code {
                KeyCode::Enter | KeyCode::Char(' ') => self.activate()?,
                KeyCode::Down | KeyCode::Char('j') => self.navigate_across(1),
                KeyCode::Up | KeyCode::Char('k') => self.navigate_across(-1),
                KeyCode::PageDown => self.navigate(8),
                KeyCode::PageUp => self.navigate(-8),
                _ => (),
            },
        }
        Ok(false)
    }
    fn dialog_key(&mut self, dialog: &mut Dialog, key: KeyEvent) -> Result<bool> {
        if key.code == KeyCode::Esc && !matches!(dialog, Dialog::Diff(_)) {
            return Ok(true);
        }
        match dialog {
            Dialog::Branches(branch) => {
                let branches = self
                    .snapshot
                    .as_ref()
                    .map(|s| s.branches.as_slice())
                    .unwrap_or_default();
                let matches = matching_branches(branches, &text(&branch.query));
                match key.code {
                    KeyCode::Down => {
                        branch.selected = (branch.selected + 1).min(matches.len().saturating_sub(1))
                    }
                    KeyCode::Up => branch.selected = branch.selected.saturating_sub(1),
                    KeyCode::Enter => {
                        if let Some(name) = matches.get(branch.selected) {
                            self.choose_branch(name.to_string());
                            return Ok(true);
                        }
                    }
                    _ => {
                        branch.query.input(key);
                        branch.selected = 0;
                    }
                }
            }
            Dialog::Push(remote, target) => {
                if key.code == KeyCode::Enter {
                    let repo = self.repo.clone();
                    let remote = remote.clone();
                    let target = target.clone();
                    self.work("Pushing…", move || {
                        Ok(Update::Pushed(repo.push(&remote, &target)?))
                    });
                    return Ok(true);
                }
            }
            Dialog::Mr(mr) => match key.code {
                KeyCode::Tab => mr.selected = (mr.selected + 1) % 7,
                KeyCode::BackTab => mr.selected = (mr.selected + 6) % 7,
                KeyCode::Enter if mr.selected == 5 => {
                    self.submit_mr(mr);
                    return Ok(true);
                }
                KeyCode::Enter if mr.selected == 6 => return Ok(true),
                KeyCode::Enter if mr.selected != 4 => mr.selected = (mr.selected + 1) % 7,
                _ if mr.selected < 5 => {
                    mr.fields[mr.selected].input(key);
                }
                _ => (),
            },
            Dialog::Diff(viewer) => {
                if viewer.key(key) {
                    return Ok(true);
                }
            }
            Dialog::Discard(plan) => match key.code {
                KeyCode::Char('y' | 'Y')
                    if self.discard_visible
                        && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT) =>
                {
                    self.run_file_action(FileAction::Discard(plan.clone()));
                    return Ok(true);
                }
                KeyCode::Char('n' | 'N') => return Ok(true),
                _ => (),
            },
        }
        Ok(false)
    }
    fn submit_mr(&mut self, form: &MrDialog) {
        let values: Vec<_> = form.fields.iter().map(text).collect();
        let mr = Mr {
            remote: values[0].trim().into(),
            source: values[1].trim().into(),
            target: values[2].trim().into(),
            title: values[3].trim().into(),
            description: values[4].clone(),
        };
        let repo = self.repo.clone();
        let hosts = self.cfg.hosts.clone();
        self.work("Creating merge request…", move || {
            Ok(Update::Mr(repo.create_mr(&mr, &hosts)?))
        });
    }
    fn mouse(&mut self, mouse: MouseEvent) -> Result<()> {
        if self.dialog.is_none()
            && (self.preview_area.contains((mouse.column, mouse.row).into())
                || self.preview.captures_input()
                || self.preview.mouse_captured())
        {
            if self.preview.available() {
                self.focus = Focus::Diff;
                self.preview.mouse(mouse);
            }
            return Ok(());
        }
        if let Some(Dialog::Diff(viewer)) = &mut self.dialog {
            viewer.mouse(mouse);
            return Ok(());
        }
        if matches!(
            mouse.kind,
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp
        ) {
            let delta = if mouse.kind == MouseEventKind::ScrollDown {
                1
            } else {
                -1
            };
            if let Some(dialog) = &mut self.dialog {
                if let Dialog::Branches(branch) = dialog {
                    let count = self.snapshot.as_ref().map_or(0, |s| {
                        matching_branches(&s.branches, &text(&branch.query)).len()
                    });
                    branch.selected = branch
                        .selected
                        .saturating_add_signed(delta)
                        .min(count.saturating_sub(1));
                }
            } else {
                if let Some((_, hit)) = self
                    .hits
                    .iter()
                    .rev()
                    .find(|(rect, _)| rect.contains((mouse.column, mouse.row).into()))
                {
                    match hit {
                        Hit::GraphRow(_) | Hit::Focus(Focus::Graph) => self.focus = Focus::Graph,
                        Hit::FileRow(Focus::BranchFiles, _) | Hit::Focus(Focus::BranchFiles) => {
                            self.focus = Focus::BranchFiles
                        }
                        Hit::FileRow(Focus::WorkingFiles, _) | Hit::Focus(Focus::WorkingFiles) => {
                            self.focus = Focus::WorkingFiles
                        }
                        _ => (),
                    }
                }
                self.navigate(delta);
            }
            return Ok(());
        }
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return Ok(());
        }
        let Some(hit) = self
            .hits
            .iter()
            .rev()
            .find(|(r, _)| r.contains((mouse.column, mouse.row).into()))
            .map(|(_, a)| a.clone())
        else {
            return Ok(());
        };
        if let Some(mut dialog) = self.dialog.take() {
            let mut close = false;
            match (&mut dialog, hit) {
                (Dialog::Branches(branch), Hit::BranchRow(index)) => {
                    if let Some(name) = self.snapshot.as_ref().and_then(|s| {
                        matching_branches(&s.branches, &text(&branch.query))
                            .get(index)
                            .map(|s| s.to_string())
                    }) {
                        self.choose_branch(name);
                        close = true;
                    }
                }
                (Dialog::Mr(form), Hit::ModalField(index)) => form.selected = index,
                (Dialog::Mr(form), Hit::Submit) => {
                    self.submit_mr(form);
                    close = true;
                }
                (Dialog::Push(remote, target), Hit::Submit) => {
                    let repo = self.repo.clone();
                    let remote = remote.clone();
                    let target = target.clone();
                    self.work("Pushing…", move || {
                        Ok(Update::Pushed(repo.push(&remote, &target)?))
                    });
                    close = true;
                }
                (Dialog::Discard(plan), Hit::Submit) if self.discard_visible => {
                    self.run_file_action(FileAction::Discard(plan.clone()));
                    close = true;
                }
                (_, Hit::Cancel) => close = true,
                _ => (),
            }
            if !close {
                self.dialog = Some(dialog);
            }
        } else if !self.busy() || self.background_refresh || self.file_action_running {
            match hit {
                Hit::Focus(focus) => {
                    if !self.can_focus(focus) {
                        return Ok(());
                    }
                    self.focus = focus;
                    if matches!(
                        focus,
                        Focus::WorkingFiles | Focus::BranchFiles | Focus::Graph
                    ) {
                        self.preview_origin = focus;
                        self.sync_preview();
                    }
                    if !matches!(
                        focus,
                        Focus::Title
                            | Focus::Description
                            | Focus::Graph
                            | Focus::BranchFiles
                            | Focus::WorkingFiles
                            | Focus::Diff
                    ) {
                        self.activate()?;
                    }
                }
                Hit::GraphRow(index) => {
                    self.focus = Focus::Graph;
                    self.history.select_row(index);
                    self.preview_origin = Focus::Graph;
                    self.sync_preview();
                    if matches!(self.history.selection(), Some(GraphSelection::Commit(_))) {
                        self.activate()?;
                    }
                }
                Hit::FileRow(focus, index) => {
                    self.focus = focus;
                    match focus {
                        Focus::WorkingFiles => {
                            self.working_files.select(Some(index));
                            self.preview_origin = focus;
                            self.sync_preview();
                        }
                        Focus::BranchFiles => {
                            self.branch_files.select(Some(index));
                            self.preview_origin = focus;
                            self.sync_preview();
                        }
                        _ => (),
                    }
                }
                Hit::Refresh => self.refresh(),
                Hit::ToggleGraph if self.can_focus(Focus::HistoryToggle) => self.toggle_history(),
                _ => (),
            }
        }
        Ok(())
    }
    fn draw_pending<B: Backend>(&mut self, terminal: &mut Terminal<B>) -> io::Result<bool> {
        if !self.dirty {
            return Ok(false);
        }
        terminal.draw(|frame| self.render(frame))?;
        self.dirty = false;
        Ok(true)
    }
    fn handle_event(&mut self, event: Event) -> Result<bool> {
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                self.dirty = true;
                return self.key(key);
            }
            Event::Mouse(mouse)
                if matches!(
                    mouse.kind,
                    MouseEventKind::Down(MouseButton::Left)
                        | MouseEventKind::Drag(MouseButton::Left)
                        | MouseEventKind::Up(MouseButton::Left)
                        | MouseEventKind::ScrollUp
                        | MouseEventKind::ScrollDown
                ) =>
            {
                self.dirty = true;
                self.mouse(mouse)?;
            }
            Event::Paste(value) => {
                self.dirty = true;
                if let Some(dialog) = &mut self.dialog {
                    match dialog {
                        Dialog::Branches(b) => {
                            b.query.insert_str(value.replace(['\r', '\n'], ""));
                            b.selected = 0;
                        }
                        Dialog::Mr(m) if m.selected < 5 => {
                            m.fields[m.selected].insert_str(value);
                        }
                        _ => (),
                    }
                } else if !self.busy() || self.background_refresh {
                    match self.focus {
                        Focus::Title => {
                            self.title.insert_str(value.replace(['\r', '\n'], " "));
                        }
                        Focus::Description => {
                            self.description.insert_str(value);
                        }
                        _ => (),
                    }
                }
            }
            Event::Resize(_, _) | Event::FocusGained => self.dirty = true,
            _ => (),
        }
        Ok(false)
    }
    pub fn render(&mut self, frame: &mut Frame) {
        self.discard_visible = false;
        if !self.can_focus(self.focus) {
            self.change_focus(false);
        }
        self.hits.clear();
        let area = frame.area();
        if area.width < 26 || area.height < 22 {
            self.preview_area = Rect::default();
            self.preview.hide_review();
            if let Some(Dialog::Diff(viewer)) = &mut self.dialog {
                viewer.hide_review();
            }
            frame.render_widget(Paragraph::new("Resize panel to at least 26 × 22."), area);
            self.theme.apply(frame.buffer_mut());
            return;
        }
        let link = self.mr_link_label();
        let action_height = ActionBarLayout::new(area, ACTION_LABELS, link.as_deref()).height;
        let outer = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Length(action_height),
            Constraint::Length(2),
            Constraint::Min(6),
            Constraint::Length(1),
        ])
        .split(area);
        let name = self
            .repo
            .path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        let branch = self
            .snapshot
            .as_ref()
            .map(|s| s.branch.as_str())
            .unwrap_or("");
        frame.render_widget(
            Paragraph::new(format!(
                "Git · {name}  ⑂ {}",
                if branch.is_empty() {
                    "detached / unborn"
                } else {
                    branch
                }
            ))
            .style(BOLD),
            outer[0],
        );
        let base = self
            .snapshot
            .as_ref()
            .map(|s| s.base.as_str())
            .unwrap_or("loading…");
        frame.render_widget(
            Paragraph::new(format!("Compare: {base} ▾")).style(if !self.can_focus(Focus::Base) {
                DIM
            } else if self.focus == Focus::Base {
                SELECTED
            } else {
                NORMAL
            }),
            outer[1],
        );
        if self.can_focus(Focus::Base) {
            self.hits.push((outer[1], Hit::Focus(Focus::Base)));
        }
        paint_input(
            frame,
            outer[2],
            &mut self.title,
            "Title",
            self.focus == Focus::Title,
        );
        paint_input(
            frame,
            outer[3],
            &mut self.description,
            "Description",
            self.focus == Focus::Description,
        );
        self.hits.push((outer[2], Hit::Focus(Focus::Title)));
        self.hits.push((outer[3], Hit::Focus(Focus::Description)));
        frame.render_widget(
            Paragraph::new(format!(
                "[{}] Stage all changes",
                if self.stage { "x" } else { " " }
            ))
            .style(if self.focus == Focus::Stage {
                SELECTED
            } else {
                NORMAL
            }),
            outer[4],
        );
        self.hits.push((outer[4], Hit::Focus(Focus::Stage)));
        let actions = ActionBarLayout::new(outer[5], ACTION_LABELS, link.as_deref());
        for ((rect, focus), title) in actions
            .buttons
            .into_iter()
            .zip([Focus::Commit, Focus::Push, Focus::Mr])
            .zip(ACTION_LABELS)
        {
            let enabled = self.can_focus(focus);
            frame.render_widget(
                Paragraph::new(title).style(if !enabled {
                    DIM
                } else if self.focus == focus {
                    SELECTED
                } else {
                    BOLD
                }),
                rect,
            );
            if enabled {
                self.hits.push((rect, Hit::Focus(focus)));
            }
        }
        if let Some(label) = link {
            let rect = actions.link;
            frame.render_widget(
                Paragraph::new(label).style(
                    if self.focus == Focus::Link {
                        NAVIGATION
                    } else {
                        BOLD
                    }
                    .add_modifier(Modifier::UNDERLINED),
                ),
                rect,
            );
            if self.can_focus(Focus::Link) && !rect.is_empty() {
                self.hits.push((rect, Hit::Focus(Focus::Link)));
            }
        }
        let info = if !self.message.is_empty() {
            visible(&self.message)
        } else if let Some(notice) = &self.watch_notice {
            visible(notice)
        } else {
            self.snapshot
                .as_ref()
                .map(|s| {
                    format!(
                        "{}{}",
                        s.push
                            .as_ref()
                            .map(|(r, t)| format!("HEAD → {r}/{t} · "))
                            .unwrap_or_default(),
                        if s.status.trim().is_empty() {
                            "Working tree clean".into()
                        } else {
                            format!("{} changed paths", s.status.lines().count())
                        }
                    )
                })
                .unwrap_or_default()
        };
        frame.render_widget(
            Paragraph::new(info).wrap(Wrap { trim: false }).style(DIM),
            outer[6],
        );
        self.render_history(frame, outer[7]);
        let hint = if self.focus == Focus::WorkingFiles {
            "Space stage/unstage  d discard  j/k move  Tab jump  Enter diff".to_string()
        } else {
            format!(
                "j/k move  Tab jump  Enter activate  ^R refresh  {} push  Esc close",
                self.cfg.push_key
            )
        };
        footer(frame, outer[8], &hint);
        if let Some(mut dialog) = self.dialog.take() {
            self.render_dialog(frame, &mut dialog);
            self.dialog = Some(dialog);
        }
        self.theme.apply(frame.buffer_mut());
    }
    fn ensure_files(&mut self) {
        if self.file_cache.valid {
            return;
        }
        let working = self
            .snapshot
            .as_ref()
            .map(|s| {
                s.working
                    .files
                    .iter()
                    .map(|f| {
                        let label = match f.kind {
                            ChangeKind::Unstaged => "[U] ",
                            ChangeKind::Staged => "[S] ",
                            ChangeKind::Untracked => "[?] ",
                        };
                        file_item(&f.stat, label)
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let branch = self
            .snapshot
            .as_ref()
            .map(|s| s.files.iter().map(|f| file_item(f, "")).collect::<Vec<_>>())
            .unwrap_or_default();
        self.file_cache = FileCache {
            valid: true,
            working: List::new(working).highlight_style(NAVIGATION),
            branch: List::new(branch).highlight_style(NAVIGATION),
        };
    }
    fn render_history(&mut self, frame: &mut Frame, area: Rect) {
        self.ensure_files();
        let file_count = self.snapshot.as_ref().map_or(0, |s| s.files.len());
        let working_count = self.snapshot.as_ref().map_or(0, |s| s.working.files.len());
        let show_more = !self.history.folded() && self.history.count() >= self.limit;
        let footer_height = u16::from(show_more);
        let available = area.height.saturating_sub(footer_height);
        let working_min = if working_count > 0 { 2 } else { 1 };
        let branch_min = if file_count > 0 { 2 } else { 1 };
        let history_min = if self.history.folded() { 1 } else { 3 };
        let preview_min = if available >= MIN_PREVIEW_HEIGHT + 4 {
            MIN_PREVIEW_HEIGHT
        } else {
            0
        };
        let working_max = available.saturating_sub(history_min + branch_min + preview_min);
        let working_height = (working_count
            .saturating_add(1)
            .min(if self.history.folded() { 8 } else { 6 }) as u16)
            .min(working_max)
            .max(working_min.min(working_max));
        let branch_max = available.saturating_sub(history_min + working_height + preview_min);
        let branch_height = (file_count.saturating_add(1).min(4) as u16)
            .min(branch_max)
            .max(branch_min.min(branch_max));
        let remaining = available.saturating_sub(working_height + branch_height);
        let (preview_height, history_height) = if self.history.folded() {
            (remaining.saturating_sub(1), remaining.min(1))
        } else {
            let preview = if remaining >= MIN_PREVIEW_HEIGHT + 3 {
                (remaining / 2).max(MIN_PREVIEW_HEIGHT)
            } else {
                0
            };
            (preview, remaining.saturating_sub(preview))
        };
        let layout = Layout::vertical([
            Constraint::Length(working_height),
            Constraint::Length(branch_height),
            Constraint::Length(preview_height),
            Constraint::Length(history_height),
            Constraint::Length(footer_height),
        ])
        .split(area);
        self.preview_area = layout[2];
        if self.focus == Focus::Diff && !self.can_focus(Focus::Diff) {
            self.leave_preview();
        }
        self.preview
            .render(frame, self.preview_area, self.focus == Focus::Diff);
        if !self.preview_area.is_empty() && self.preview.available() {
            self.hits.push((self.preview_area, Hit::Focus(Focus::Diff)));
        }
        render_files(
            frame,
            layout[0],
            FileList {
                title: self
                    .snapshot
                    .as_ref()
                    .map_or("Working tree", |s| s.working.summary.as_str()),
                list: &self.file_cache.working,
                len: working_count,
                active: self.focus == Focus::WorkingFiles,
                focus: Focus::WorkingFiles,
            },
            &mut self.working_files,
            &mut self.hits,
        );
        let summary = self
            .snapshot
            .as_ref()
            .map(|s| {
                s.summary
                    .lines()
                    .last()
                    .unwrap_or("No committed changes")
                    .trim()
                    .to_string()
            })
            .unwrap_or_default();
        let title = if file_count == 0 {
            "Committed on branch · no changes".to_string()
        } else {
            format!("Committed · {summary}")
        };
        render_files(
            frame,
            layout[1],
            FileList {
                title: &title,
                list: &self.file_cache.branch,
                len: file_count,
                active: self.focus == Focus::BranchFiles,
                focus: Focus::BranchFiles,
            },
            &mut self.branch_files,
            &mut self.hits,
        );
        if self.snapshot.is_some() {
            let count = self.history.count();
            let header = Rect::new(layout[3].x, layout[3].y, layout[3].width, 1);
            let label = format!(
                "{} COMMITS {count}{}",
                if self.history.folded() { "›" } else { "▾" },
                if count >= self.limit { " +" } else { "" }
            );
            frame.render_widget(
                Paragraph::new(label).style(
                    if self.focus == Focus::HistoryToggle && self.can_focus(Focus::HistoryToggle) {
                        NAVIGATION
                    } else {
                        DIM.add_modifier(Modifier::BOLD)
                    },
                ),
                header,
            );
            let refresh = Rect::new(header.right().saturating_sub(2), header.y, 2, 1);
            frame.render_widget(Paragraph::new("↻").style(DIM), refresh);
            if self.can_focus(Focus::HistoryToggle) {
                self.hits.push((header, Hit::Focus(Focus::HistoryToggle)));
                self.hits
                    .push((Rect::new(header.x, header.y, 2, 1), Hit::ToggleGraph));
            }
            self.hits.push((refresh, Hit::Refresh));
            let body = Rect::new(
                layout[3].x,
                layout[3].y + 1,
                layout[3].width,
                layout[3].height.saturating_sub(1),
            );
            if !self.history.folded() {
                self.hits.push((body, Hit::Focus(Focus::Graph)));
                self.history
                    .render(frame, body, self.focus == Focus::Graph, |rect, index| {
                        self.hits.push((rect, Hit::GraphRow(index)));
                    });
            }
        }
        if show_more {
            let more = Rect::new(layout[4].x, layout[4].y, layout[4].width, 1);
            frame.render_widget(
                Paragraph::new("[Load older commits]").style(if self.focus == Focus::More {
                    SELECTED
                } else {
                    DIM
                }),
                more,
            );
            if self.can_focus(Focus::More) {
                self.hits.push((more, Hit::Focus(Focus::More)));
            }
        }
    }
    fn mr_link_label(&self) -> Option<String> {
        if self.last_mr.is_empty() {
            return None;
        }
        let url = url::Url::parse(&self.last_mr).ok()?;
        let number = url.path_segments()?.rfind(|part| !part.is_empty())?;
        Some(format!("!MR {number} ↗"))
    }
    fn render_dialog(&mut self, frame: &mut Frame, dialog: &mut Dialog) {
        self.hits.clear();
        match dialog {
            Dialog::Branches(branch) => {
                let area = dialog_area(frame.area(), 80, 20);
                frame.render_widget(Clear, area);
                let inner = Block::bordered()
                    .title("Compare with · local / fetched refs")
                    .inner(area);
                frame.render_widget(
                    Block::bordered().title("Compare with · local / fetched refs"),
                    area,
                );
                let layout = Layout::vertical([
                    Constraint::Length(3),
                    Constraint::Min(1),
                    Constraint::Length(1),
                ])
                .split(inner);
                paint_input(frame, layout[0], &mut branch.query, "Branch name", true);
                let all = self
                    .snapshot
                    .as_ref()
                    .map(|s| s.branches.as_slice())
                    .unwrap_or_default();
                let matches = matching_branches(all, &text(&branch.query));
                branch.selected = branch.selected.min(matches.len().saturating_sub(1));
                branch.state.select(if matches.is_empty() {
                    None
                } else {
                    Some(branch.selected)
                });
                let current = self
                    .snapshot
                    .as_ref()
                    .map(|s| s.base.as_str())
                    .unwrap_or("");
                let items = matches
                    .iter()
                    .map(|b| {
                        ListItem::new(format!("{} {b}", if *b == current { "✓" } else { " " }))
                    })
                    .collect::<Vec<_>>();
                frame.render_stateful_widget(
                    List::new(items).highlight_style(NAVIGATION),
                    layout[1],
                    &mut branch.state,
                );
                for y in 0..layout[1].height {
                    let i = branch.state.offset() + y as usize;
                    if i < matches.len() {
                        self.hits.push((
                            Rect::new(layout[1].x, layout[1].y + y, layout[1].width, 1),
                            Hit::BranchRow(i),
                        ));
                    }
                }
                footer(
                    frame,
                    layout[2],
                    &if matches.is_empty() {
                        "No matches · Esc cancel".into()
                    } else {
                        format!(
                            "{}/{} branches · ↑↓ choose · Enter select",
                            matches.len(),
                            all.len()
                        )
                    },
                );
            }
            Dialog::Mr(form) => {
                let area = dialog_area(frame.area(), 88, 27);
                frame.render_widget(Clear, area);
                let block = Block::bordered().title("Create PR/MR");
                let inner = block.inner(area);
                frame.render_widget(block, area);
                let layout = Layout::vertical([
                    Constraint::Length(3),
                    Constraint::Length(3),
                    Constraint::Length(3),
                    Constraint::Length(3),
                    Constraint::Min(3),
                    Constraint::Length(1),
                    Constraint::Length(1),
                ])
                .split(inner);
                for (i, label) in [
                    "Remote",
                    "Source branch",
                    "Target branch",
                    "Title",
                    "Description",
                ]
                .iter()
                .enumerate()
                {
                    paint_input(
                        frame,
                        layout[i],
                        &mut form.fields[i],
                        label,
                        form.selected == i,
                    );
                    self.hits.push((layout[i], Hit::ModalField(i)));
                }
                let buttons =
                    Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                        .split(layout[5]);
                for (i, title, hit) in [(0, "[Create]", Hit::Submit), (1, "[Cancel]", Hit::Cancel)]
                {
                    frame.render_widget(
                        Paragraph::new(title).style(if form.selected == i + 5 {
                            SELECTED
                        } else {
                            BOLD
                        }),
                        buttons[i],
                    );
                    self.hits.push((buttons[i], hit));
                }
                footer(
                    frame,
                    layout[6],
                    "Tab to navigate · source must be pushed · Esc cancel",
                );
            }
            Dialog::Push(remote, target) => {
                let area = dialog_area(frame.area(), 80, 10);
                frame.render_widget(Clear, area);
                frame.render_widget(Paragraph::new(format!("Push HEAD to {remote}/{target}?\n\ngit push {remote} HEAD:refs/heads/{target}\n\nEnter push · Esc cancel\nUses selected comparison branch as destination.")).wrap(Wrap{trim:false}).block(Block::bordered().title("Push")),area);
                let buttons =
                    Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                        .split(Rect::new(
                            area.x + 1,
                            area.bottom().saturating_sub(2),
                            area.width.saturating_sub(2),
                            1,
                        ));
                for (rect, label, hit) in [
                    (buttons[0], "[Push]", Hit::Submit),
                    (buttons[1], "[Cancel]", Hit::Cancel),
                ] {
                    frame.render_widget(Paragraph::new(label).style(BOLD), rect);
                    self.hits.push((rect, hit));
                }
            }
            Dialog::Diff(viewer) => {
                let area = dialog_area(frame.area(), frame.area().width, frame.area().height);
                frame.render_widget(Clear, area);
                viewer.render(frame, area);
            }
            Dialog::Discard(plan) => {
                let bounds = dialog_area(frame.area(), 80, frame.area().height.saturating_sub(2));
                let width = usize::from(bounds.width.saturating_sub(2)).max(1);
                let effect = if plan.deletes_file {
                    "Delete this newly added file:"
                } else {
                    "Reset staged and unstaged changes to HEAD:"
                };
                let content = format!("{effect}\n\n{}", visible(&plan.path));
                let mut lines = Vec::new();
                let mut text = String::new();
                let mut columns = 0;
                for ch in content.chars() {
                    let cells = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                    if ch == '\n' || columns + cells > width {
                        lines.push(Line::raw(std::mem::take(&mut text)));
                        columns = 0;
                        if ch == '\n' {
                            continue;
                        }
                    }
                    text.push(ch);
                    columns += cells;
                }
                lines.push(Line::raw(text));
                let height = u16::try_from(lines.len())
                    .unwrap_or(u16::MAX)
                    .saturating_add(4);
                let area = dialog_area(frame.area(), 80, height.min(bounds.height));
                let block = Block::bordered().title("Discard file changes?");
                let inner = block.inner(area);
                frame.render_widget(Clear, area);
                frame.render_widget(block, area);
                let rows =
                    Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(inner);
                self.discard_visible = lines.len() <= usize::from(rows[0].height);
                frame.render_widget(Paragraph::new(lines), rows[0]);
                let hint = if self.discard_visible {
                    "[y Discard]   [n Cancel]"
                } else {
                    "n Cancel · enlarge to confirm"
                };
                frame.render_widget(Paragraph::new(hint).style(BOLD), rows[1]);
                if self.discard_visible {
                    self.hits.push((
                        Rect::new(rows[1].x, rows[1].y, rows[1].width.min(11), rows[1].height),
                        Hit::Submit,
                    ));
                    self.hits.push((
                        Rect::new(
                            rows[1].x.saturating_add(14),
                            rows[1].y,
                            rows[1].width.saturating_sub(14),
                            rows[1].height,
                        ),
                        Hit::Cancel,
                    ));
                }
            }
        }
    }
}
struct FileList<'a> {
    title: &'a str,
    list: &'a List<'static>,
    len: usize,
    active: bool,
    focus: Focus,
}
fn file_item(file: &FileStat, prefix: &str) -> ListItem<'static> {
    let mut spans = vec![
        Span::styled(prefix.to_string(), DIM),
        Span::raw(format!("{}  ", visible(&file.path))),
    ];
    if file.added == "?" {
        spans.push(Span::styled("untracked", DIM));
    } else if file.added == "-" {
        spans.push(Span::styled("binary", DIM));
    } else {
        spans.push(Span::styled(
            format!("+{}", file.added),
            NORMAL.fg(Color::Green),
        ));
        spans.push(Span::styled(
            format!(" -{}", file.deleted),
            NORMAL.fg(Color::Red),
        ));
    }
    ListItem::new(Line::from(spans))
}
fn render_files(
    frame: &mut Frame,
    area: Rect,
    view: FileList<'_>,
    state: &mut ListState,
    hits: &mut Vec<(Rect, Hit)>,
) {
    let FileList {
        title,
        list,
        len,
        active,
        focus,
    } = view;
    if area.height == 0 {
        return;
    }
    frame.render_widget(
        Paragraph::new(title.to_string()).style(BOLD),
        Rect::new(area.x, area.y, area.width, 1),
    );
    let rect = Rect::new(
        area.x,
        area.y + 1,
        area.width,
        area.height.saturating_sub(1),
    );
    // Backfill the enlarged viewport when a list was previously scrolled near
    // its end; otherwise collapsing history would leave mostly blank space.
    let max_offset = len.saturating_sub(rect.height as usize);
    if state.offset() > max_offset {
        *state.offset_mut() = max_offset;
    }
    let offset = if active {
        frame.render_stateful_widget(list, rect, state);
        state.offset()
    } else {
        let mut display = state.clone();
        *display.selected_mut() = None;
        frame.render_stateful_widget(list, rect, &mut display);
        display.offset()
    };
    for y in 0..rect.height {
        let index = offset + y as usize;
        if index < len {
            hits.push((
                Rect::new(rect.x, rect.y + y, rect.width, 1),
                Hit::FileRow(focus, index),
            ));
        }
    }
    if len > 0 && rect.height > 0 {
        hits.push((Rect::new(area.x, area.y, area.width, 1), Hit::Focus(focus)));
    }
}
fn open_url(url: &str) -> Result<()> {
    if !matches!(url::Url::parse(url)?.scheme(), "http" | "https") {
        bail!("Only HTTP links can be opened");
    }
    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    Command::new(program)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    Ok(())
}
struct TerminalGuard;
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(
            io::stdout(),
            PopKeyboardEnhancementFlags,
            DisableMouseCapture,
            DisableBracketedPaste
        );
        ratatui::restore();
    }
}
fn terminal() -> Result<(DefaultTerminal, TerminalGuard)> {
    let terminal = ratatui::try_init()?;
    let guard = TerminalGuard;
    execute!(
        io::stdout(),
        EnableMouseCapture,
        EnableBracketedPaste,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )?;
    Ok((terminal, guard))
}
pub fn run(repo: Repo, cfg: Config) -> Result<()> {
    let mut panel = Panel::new(repo, cfg);
    let (mut terminal, _guard) = terminal()?;
    let events = crate::runtime::Events::start()?;
    panel.wake = Some(events.waker());
    let follow = crate::follow::Follow::start(events.waker())?;
    let mut pending_repo = None;
    let mut drafts = HashMap::new();
    let mut watch = panel.start_watch(events.waker());
    panel.refresh();
    loop {
        if let Some(repo) = follow.as_ref().and_then(|follow| follow.take_latest()) {
            if repo.path != panel.repo.path || pending_repo.is_some() {
                pending_repo = Some(repo);
                // Deferred actions and confirmations belong to the old repository.
                panel.pending_file_action = None;
                panel.pending_activation = None;
                panel.pending_refresh = false;
                panel.dialog = None;
            }
        }
        if pending_repo.is_some() {
            panel.finish_before_switch();
        } else {
            panel.receive();
        }
        if !panel.busy() {
            if let Some(repo) = pending_repo.take() {
                if panel.repo.path != repo.path {
                    panel.switch_repo(repo, &mut drafts);
                    watch = panel.start_watch(events.waker());
                    panel.refresh();
                }
            }
        }
        if let Some(watch) = &watch {
            if let Some(warning) = watch.warning() {
                panel.watch_notice = Some(warning);
                panel.dirty = true;
            }
            if pending_repo.is_none() && !panel.busy() && panel.dialog.is_none() {
                if let Some(changes) = watch.take_ready(Instant::now()) {
                    panel.auto_refresh(changes);
                }
            }
        }
        panel.draw_pending(&mut terminal)?;
        // Only pending filesystem batches and preview selections need deadlines.
        let watch_timeout = if panel.busy() || panel.dialog.is_some() {
            None
        } else {
            watch.as_ref().and_then(|w| w.wait(Instant::now()))
        };
        let timeout = match (watch_timeout, panel.preview.wait(Instant::now())) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let Some(event) = events.next(timeout)? else {
            continue;
        };
        if pending_repo.is_some() {
            // Finish any already running operation before accepting new input.
            continue;
        }
        match panel.handle_event(event) {
            Ok(true) => break,
            Err(error) => {
                panel.message = error.to_string();
                panel.dirty = true;
            }
            _ => (),
        }
    }
    if let Some(worker) = panel.worker.take() {
        let _ = worker.join();
    }
    Ok(())
}

type Drafts = HashMap<PathBuf, (TextArea<'static>, TextArea<'static>, bool)>;

impl Panel {
    fn finish_before_switch(&mut self) {
        // Let active Git operations finish, but never apply their old snapshot
        // or open their deferred popup after a new repository was requested.
        if self.receiver.try_recv().is_ok() {
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    fn switch_repo(&mut self, repo: Repo, drafts: &mut Drafts) {
        debug_assert!(!self.busy());
        let mut next = Self::new(repo, self.cfg.clone());
        next.theme = self.theme.clone();
        next.wake = self.wake.clone();
        if let Some((title, description, stage)) = drafts.remove(&next.repo.path) {
            next.title = title;
            next.description = description;
            next.stage = stage;
        }
        let old = std::mem::replace(self, next);
        drafts.insert(old.repo.path, (old.title, old.description, old.stage));
    }

    fn start_watch(&mut self, wake: SyncSender<()>) -> Option<RepoWatch> {
        if !self.cfg.auto_refresh {
            None
        } else {
            match RepoWatch::start(&self.repo, wake) {
                Ok(watch) => Some(watch),
                Err(error) => {
                    self.watch_notice = Some(format!(
                        "Auto refresh unavailable: {error:#}; Ctrl+R to refresh"
                    ));
                    None
                }
            }
        }
    }
}
pub fn run_diff(data: DiffData) -> Result<()> {
    let theme = Theme::load()?;
    let (mut terminal, _guard) = terminal()?;
    let events = crate::runtime::Events::start()?;
    let mut viewer = Viewer::new(data, Some(events.waker()));
    let mut dirty = true;
    loop {
        dirty |= viewer.poll();
        if dirty {
            terminal.draw(|frame| {
                viewer.render(frame, frame.area());
                theme.apply(frame.buffer_mut());
            })?;
            dirty = false;
        }
        let Some(event) = events.next(None)? else {
            continue;
        };
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                if viewer.key(key) {
                    break;
                }
                dirty = true;
            }
            Event::Mouse(mouse)
                if matches!(
                    mouse.kind,
                    MouseEventKind::ScrollDown
                        | MouseEventKind::ScrollUp
                        | MouseEventKind::Down(MouseButton::Left)
                        | MouseEventKind::Drag(MouseButton::Left)
                        | MouseEventKind::Up(MouseButton::Left)
                ) =>
            {
                viewer.mouse(mouse);
                dirty = true;
            }
            Event::Resize(_, _) | Event::FocusGained => dirty = true,
            _ => (),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::GraphRow;
    use crate::graph;
    use ratatui::{backend::TestBackend, Terminal};
    #[test]
    fn switching_waits_for_worker_and_discards_its_old_result() {
        let mut panel = fixture();
        let (release, wait) = mpsc::channel();
        let sender = panel.sender.clone();
        panel.worker = Some(thread::spawn(move || {
            wait.recv().unwrap();
            sender
                .send(Ok(Update::Committed("old commit result".into())))
                .unwrap();
        }));
        panel.finish_before_switch();
        assert!(panel.busy());
        release.send(()).unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(2);
        while panel.busy() {
            assert!(Instant::now() < deadline);
            panel.finish_before_switch();
            thread::yield_now();
        }
        assert!(panel.message.is_empty());
        assert!(panel.refresh_notice.is_none());
    }
    #[test]
    fn repository_switch_resets_views_and_keeps_each_repositories_draft() {
        let mut panel = fixture();
        let original = panel.repo.clone();
        panel.title.insert_str("original draft");
        panel.description.insert_str("original description");
        panel.last_mr = "old merge request".into();
        let mut drafts = Drafts::new();
        panel.switch_repo(
            Repo {
                path: PathBuf::from("/linked"),
            },
            &mut drafts,
        );
        assert_eq!(panel.repo.path, PathBuf::from("/linked"));
        assert!(panel.snapshot.is_none());
        assert!(panel.preview.wait(Instant::now()).is_none());
        assert!(panel.last_mr.is_empty());
        assert!(text(&panel.title).is_empty());
        panel.title.insert_str("linked draft");
        panel.switch_repo(original, &mut drafts);
        assert_eq!(text(&panel.title), "original draft");
        assert_eq!(text(&panel.description), "original description");
        panel.switch_repo(
            Repo {
                path: PathBuf::from("/linked"),
            },
            &mut drafts,
        );
        assert_eq!(text(&panel.title), "linked draft");
    }
    #[test]
    fn focused_commit_inputs_have_a_visible_cursor_with_and_without_text() {
        for focus in [Focus::Title, Focus::Description] {
            for value in ["", "a", "中文"] {
                let mut panel = fixture();
                panel.theme = Theme::from_config("[theme]\nname='vesper'\n[theme.custom]\npanel_bg='#2D2A2E'\ntext='#E3E1E4'\n[ui]\naccent='#9A8ED9'", crate::theme::Appearance::Dark).unwrap();
                panel.focus = focus;
                let input = if focus == Focus::Title {
                    &mut panel.title
                } else {
                    &mut panel.description
                };
                input.insert_str(value);
                input.move_cursor(tui_textarea::CursorMove::Head);
                let mut terminal = Terminal::new(TestBackend::new(60, 40)).unwrap();
                terminal.draw(|f| panel.render(f)).unwrap();
                let rect = panel
                    .hits
                    .iter()
                    .find(|(_, h)| matches!(h, Hit::Focus(f) if *f == focus))
                    .unwrap()
                    .0;
                let cell = &terminal.backend().buffer()[(rect.x + 1, rect.y + 1)];
                assert_eq!(
                    cell.bg, panel.theme.accent,
                    "focused {focus:?} cursor must not use the dark selection fill"
                );
                assert_eq!(
                    cell.fg, panel.theme.background,
                    "text beneath the cursor must remain readable"
                );
                let other = if focus == Focus::Title {
                    Focus::Description
                } else {
                    Focus::Title
                };
                let rect = panel
                    .hits
                    .iter()
                    .find(|(_, h)| matches!(h, Hit::Focus(f) if *f == other))
                    .unwrap()
                    .0;
                assert_eq!(
                    terminal.backend().buffer()[(rect.x + 1, rect.y + 1)].bg,
                    panel.theme.background
                );
            }
        }
    }
    #[test]
    fn opening_history_routes_jk_to_commits_and_expanded_files() {
        for mouse in [false, true] {
            let mut panel = fixture();
            panel.history.toggle();
            panel.focus = Focus::HistoryToggle;
            let mut terminal = Terminal::new(TestBackend::new(60, 40)).unwrap();
            panel.draw_pending(&mut terminal).unwrap();
            let event = if mouse {
                let rect = panel
                    .hits
                    .iter()
                    .find(|(_, h)| matches!(h, Hit::ToggleGraph))
                    .unwrap()
                    .0;
                Event::Mouse(MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: rect.x,
                    row: rect.y,
                    modifiers: KeyModifiers::NONE,
                })
            } else {
                Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            };
            panel.handle_event(event).unwrap();
            panel.draw_pending(&mut terminal).unwrap();
            assert_eq!(
                panel.focus,
                Focus::Graph,
                "expanded history must accept j/k immediately"
            );
            panel
                .handle_event(Event::Key(KeyEvent::new(
                    KeyCode::Char('j'),
                    KeyModifiers::NONE,
                )))
                .unwrap();
            assert_eq!(
                panel.history.selection(),
                Some(GraphSelection::Commit("def12345".into()))
            );
            panel
                .handle_event(Event::Key(KeyEvent::new(
                    KeyCode::Char('k'),
                    KeyModifiers::NONE,
                )))
                .unwrap();
            panel
                .sender
                .send(Ok(Update::Details(
                    "abc12345".into(),
                    "1 file changed".into(),
                    vec![FileStat {
                        path: "detail.rs".into(),
                        added: "1".into(),
                        deleted: "0".into(),
                    }],
                )))
                .unwrap();
            panel.receive();
            panel.draw_pending(&mut terminal).unwrap();
            panel
                .handle_event(Event::Key(KeyEvent::new(
                    KeyCode::Char('j'),
                    KeyModifiers::NONE,
                )))
                .unwrap();
            assert_eq!(
                panel.history.selection(),
                Some(GraphSelection::File("abc12345".into(), "detail.rs".into()))
            );
            panel
                .handle_event(Event::Key(KeyEvent::new(
                    KeyCode::Char('j'),
                    KeyModifiers::NONE,
                )))
                .unwrap();
            assert_eq!(
                panel.history.selection(),
                Some(GraphSelection::Commit("def12345".into()))
            );
            panel
                .handle_event(Event::Key(KeyEvent::new(
                    KeyCode::Char('k'),
                    KeyModifiers::NONE,
                )))
                .unwrap();
            assert_eq!(
                panel.history.selection(),
                Some(GraphSelection::File("abc12345".into(), "detail.rs".into()))
            );
        }
    }
    #[test]
    fn automatic_worktree_updates_preserve_editors_selection_and_graph_cache() {
        let mut panel = fixture();
        panel.title.insert_str("WIP title");
        panel.description.insert_str("Keep this description");
        panel.stage = false;
        let file = |path: &str| crate::git::WorkingFile {
            stat: FileStat {
                path: path.into(),
                added: "1".into(),
                deleted: "0".into(),
            },
            kind: ChangeKind::Unstaged,
        };
        panel.snapshot.as_mut().unwrap().working.files = vec![file("z.rs")];
        panel.working_files.select(Some(0));
        panel.focus = Focus::Title;
        let mut terminal = Terminal::new(TestBackend::new(60, 40)).unwrap();
        panel.draw_pending(&mut terminal).unwrap();
        let projections = graph::projection_count();
        panel.background_refresh = true;
        panel.worker = Some(thread::spawn(|| {}));
        panel
            .handle_event(Event::Key(KeyEvent::new(
                KeyCode::Char('!'),
                KeyModifiers::NONE,
            )))
            .unwrap();
        panel
            .sender
            .send(Ok(Update::Automatic(Some(AutoData::Working(
                " M z.rs".into(),
                crate::git::WorkingTree {
                    files: vec![file("a.rs"), file("z.rs")],
                    summary: "2 changed paths".into(),
                },
            )))))
            .unwrap();
        panel.receive();
        panel.draw_pending(&mut terminal).unwrap();
        assert_eq!(text(&panel.title), "WIP title!");
        assert_eq!(text(&panel.description), "Keep this description");
        assert!(!panel.stage);
        assert_eq!(panel.focus, Focus::Title);
        assert_eq!(panel.working_files.selected(), Some(1));
        assert_eq!(
            graph::projection_count(),
            projections,
            "file changes never rebuild history"
        );
        panel.sender.send(Ok(Update::Automatic(None))).unwrap();
        panel.receive();
        assert!(
            !panel.draw_pending(&mut terminal).unwrap(),
            "ignored output does not redraw"
        );
        let snapshot = snapshot_with_history(&panel);
        panel
            .sender
            .send(Ok(Update::Automatic(Some(AutoData::Snapshot(Box::new(
                snapshot,
            ))))))
            .unwrap();
        panel.receive();
        assert!(
            !panel.draw_pending(&mut terminal).unwrap(),
            "identical snapshots do not redraw"
        );
    }
    #[test]
    fn repository_refresh_preserves_multiple_expansions_and_selected_file_by_identity() {
        let mut panel = fixture();
        panel.focus = Focus::Graph;
        for sha in ["abc12345", "def12345"] {
            assert!(panel.history.select(&GraphSelection::Commit(sha.into())));
            panel
                .sender
                .send(Ok(Update::Details(
                    sha.into(),
                    "1 file changed".into(),
                    vec![FileStat {
                        path: format!("{sha}.rs"),
                        added: "1".into(),
                        deleted: "0".into(),
                    }],
                )))
                .unwrap();
            panel.receive();
        }
        panel.navigate(1);
        let selected = panel.history.selection().unwrap();
        assert_eq!(
            selected,
            GraphSelection::File("def12345".into(), "def12345.rs".into())
        );
        let mut snapshot = snapshot_with_history(&panel);
        snapshot.graph.insert(
            0,
            crate::git::GraphRow {
                lanes: "* ".into(),
                sha: "new12345".into(),
                subject: "New commit".into(),
                ..Default::default()
            },
        );
        panel.toggle_history();
        let projections = graph::projection_count();
        panel
            .sender
            .send(Ok(Update::Automatic(Some(AutoData::Snapshot(Box::new(
                snapshot,
            ))))))
            .unwrap();
        panel.receive();
        assert_eq!(panel.history.selection(), Some(selected.clone()));
        assert_eq!(
            graph::projection_count(),
            projections,
            "folded history is not projected"
        );
        panel.toggle_history();
        let mut terminal = Terminal::new(TestBackend::new(60, 40)).unwrap();
        terminal.draw(|frame| panel.render(frame)).unwrap();
        assert_eq!(panel.history.selection(), Some(selected));
        assert_eq!(
            panel.history.selection(),
            Some(GraphSelection::File(
                "def12345".into(),
                "def12345.rs".into()
            ))
        );
    }
    #[test]
    fn background_query_allows_jk_and_deferred_enter_keeps_clicked_commit_identity() {
        let mut panel = fixture();
        panel.focus = Focus::Graph;
        panel
            .sender
            .send(Ok(Update::Details(
                "abc12345".into(),
                "no files".into(),
                vec![],
            )))
            .unwrap();
        panel.receive();
        panel.background_refresh = true;
        panel.worker = Some(thread::spawn(|| {}));
        // Enter on the first commit, then navigate away before the query returns.
        panel
            .handle_event(Event::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .unwrap();
        panel
            .handle_event(Event::Key(KeyEvent::new(
                KeyCode::Char('j'),
                KeyModifiers::NONE,
            )))
            .unwrap();
        assert_eq!(
            panel.history.selection(),
            Some(GraphSelection::Commit("def12345".into()))
        );
        panel.sender.send(Ok(Update::Automatic(None))).unwrap();
        panel.receive();
        assert!(
            panel.history.activate() == Some(HistoryAction::ReadCommit("abc12345".into())),
            "Enter must collapse the clicked commit, not the new selection"
        );
        assert!(!panel.busy());
        assert_eq!(
            panel.history.selection(),
            Some(GraphSelection::Commit("abc12345".into()))
        );
    }
    #[test]
    fn manual_refresh_is_retained_when_a_worker_is_running() {
        let mut panel = fixture();
        panel.worker = Some(thread::spawn(|| {}));
        panel.refresh();
        assert!(panel.pending_refresh);
        panel.worker.take().unwrap().join().unwrap();
        // Completion starts the queued refresh rather than silently discarding it.
        panel.sender.send(Ok(Update::Done)).unwrap();
        panel.receive();
        assert!(!panel.pending_refresh);
        assert!(panel.busy());
        panel.worker.take().unwrap().join().unwrap();
    }
    #[test]
    fn automatic_file_event_recovers_when_initial_snapshot_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-b", "main"]);
        git(&["config", "user.name", "Test"]);
        git(&["config", "user.email", "test@example.invalid"]);
        std::fs::write(dir.path().join("file"), "base\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-m", "initial"]);
        let mut panel = Panel::new(Repo::discover(dir.path()).unwrap(), Config::default());
        panel.auto_refresh(Changes::Worktree(vec![]));
        panel.worker.take().unwrap().join().unwrap();
        panel.receive();
        assert!(
            panel.snapshot.is_some(),
            "a file event must recover the full initial snapshot"
        );
        assert_eq!(panel.history.count(), 1);
        assert!(panel.history.folded());
    }
    #[test]
    fn action_bar_adapts_to_label_display_widths_without_clipping_or_overlapping() {
        for width in [26, 33, 43, 60] {
            for title in ["[Create PR]", "[Create PR/MR]", "[创建合并请求]"] {
                let labels = ["[Commit]", "[Push]", title];
                let link = "!MR 123 ↗";
                let area = Rect::new(2, 1, width, 2);
                let layout = ActionBarLayout::new(area, labels, Some(link));
                let mut terminal = Terminal::new(TestBackend::new(width + 4, 4)).unwrap();
                terminal
                    .draw(|frame| {
                        for (rect, label) in layout.buttons.into_iter().zip(labels) {
                            frame.render_widget(Paragraph::new(label), rect);
                        }
                        frame.render_widget(Paragraph::new(link), layout.link);
                    })
                    .unwrap();
                for (rect, label) in layout
                    .buttons
                    .into_iter()
                    .zip(labels)
                    .chain(std::iter::once((layout.link, link)))
                {
                    let mut x = rect.x;
                    for ch in label.chars() {
                        assert_eq!(
                            terminal.backend().buffer()[(x, rect.y)].symbol(),
                            ch.to_string(),
                            "{title} at width {width}"
                        );
                        x += unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0) as u16;
                    }
                }
                for rect in layout.buttons {
                    assert!(rect.x >= area.x && rect.right() <= area.right());
                }
                assert_eq!(layout.link.x, layout.buttons[2].right() + 1);
                assert_eq!(layout.link.y, layout.buttons[2].y);
                assert!(layout.link.right() <= area.right());
                if layout.height == 2 {
                    assert_eq!(layout.buttons[2].y, area.y + 1);
                }
            }
        }
    }

    #[test]
    fn created_mr_link_sits_beside_button_at_wide_and_narrow_widths() {
        for width in [26, 42, 60] {
            for url in [
                "https://gitlab.example/team/repo/-/merge_requests/123",
                "https://github.com/team/repo/pull/123?tab=files",
            ] {
                let mut panel = fixture();
                let mut terminal = Terminal::new(TestBackend::new(width, 40)).unwrap();
                panel.draw_pending(&mut terminal).unwrap();
                assert!(!panel.can_focus(Focus::Link));
                assert!(!panel
                    .hits
                    .iter()
                    .any(|(_, hit)| matches!(hit, Hit::Focus(Focus::Link))));
                panel.sender.send(Ok(Update::Mr(url.into()))).unwrap();
                panel.receive();
                panel.draw_pending(&mut terminal).unwrap();
                let button = panel
                    .hits
                    .iter()
                    .find(|(_, hit)| matches!(hit, Hit::Focus(Focus::Mr)))
                    .unwrap()
                    .0;
                let link = panel
                    .hits
                    .iter()
                    .find(|(_, hit)| matches!(hit, Hit::Focus(Focus::Link)))
                    .unwrap()
                    .0;
                assert_eq!(button.y, link.y, "link must share the MR button row");
                assert!(
                    link.x > button.right(),
                    "link must be to the right with a gap"
                );
                assert!(link.right() <= width);
                assert!(dump(&terminal)
                    .iter()
                    .any(|line| line.contains("[Create PR/MR] !MR 123 ↗")));
                panel.focus = Focus::Mr;
                panel.change_focus(false);
                assert_eq!(
                    panel.focus,
                    Focus::Link,
                    "Tab must reach the adjacent link next"
                );
                assert!(matches!(
                    panel.capture_activation(),
                    Some(Activation::Focus(Focus::Link))
                ));
                assert_eq!(panel.last_mr, url);
            }
        }
    }
    #[test]
    fn new_panels_start_with_commit_history_collapsed() {
        let panel = Panel::new(
            Repo {
                path: std::env::temp_dir(),
            },
            Config::default(),
        );
        assert!(panel.history.folded());
    }
    #[test]
    fn tab_skips_empty_lists_and_unavailable_commit() {
        let mut panel = fixture();
        panel.snapshot.as_mut().unwrap().files.clear();
        panel.snapshot.as_mut().unwrap().push = None;
        panel.focus = Focus::Stage;
        panel.change_focus(false);
        assert_ne!(
            panel.focus,
            Focus::Commit,
            "an empty commit title is not actionable"
        );
        panel.focus = Focus::WorkingFiles;
        panel.change_focus(false);
        assert_ne!(
            panel.focus,
            Focus::BranchFiles,
            "empty lists must not capture Tab"
        );
    }
    #[test]
    fn tab_and_shift_tab_only_cycle_actionable_controls_when_history_is_folded() {
        let mut panel = fixture();
        panel.history.toggle();
        let snapshot = panel.snapshot.as_mut().unwrap();
        snapshot.files.clear();
        snapshot.remotes.clear();
        snapshot.push = None;
        panel.focus = Focus::Base;
        for expected in [
            Focus::Title,
            Focus::Description,
            Focus::Stage,
            Focus::HistoryToggle,
            Focus::Base,
        ] {
            panel.change_focus(false);
            assert_eq!(panel.focus, expected);
        }
        for expected in [
            Focus::HistoryToggle,
            Focus::Stage,
            Focus::Description,
            Focus::Title,
            Focus::Base,
        ] {
            panel.change_focus(true);
            assert_eq!(panel.focus, expected);
        }
        panel.worker = Some(thread::spawn(|| {}));
        panel.change_focus(false);
        assert_eq!(panel.focus, Focus::Base);
        panel.worker.take().unwrap().join().unwrap();
    }
    #[test]
    fn commit_focus_requires_title_and_changes_in_the_selected_staging_mode() {
        let mut panel = fixture();
        panel.title.insert_str("A commit");
        assert!(!panel.can_focus(Focus::Commit));
        panel
            .snapshot
            .as_mut()
            .unwrap()
            .working
            .files
            .push(crate::git::WorkingFile {
                stat: FileStat {
                    path: "local.rs".into(),
                    added: "1".into(),
                    deleted: "0".into(),
                },
                kind: ChangeKind::Unstaged,
            });
        assert!(panel.can_focus(Focus::Commit));
        panel.stage = false;
        assert!(!panel.can_focus(Focus::Commit));
        panel.snapshot.as_mut().unwrap().working.files[0].kind = ChangeKind::Staged;
        assert!(panel.can_focus(Focus::Commit));
    }
    #[test]
    fn folded_history_defers_projection_until_opened_at_the_current_width() {
        let mut panel = fixture();
        panel.history.toggle();
        panel.focus = Focus::HistoryToggle;
        let mut terminal = Terminal::new(TestBackend::new(60, 40)).unwrap();
        let before = graph::projection_count();
        panel.draw_pending(&mut terminal).unwrap();
        assert_eq!(graph::projection_count(), before);
        terminal.backend_mut().resize(42, 32);
        terminal.autoresize().unwrap();
        panel.handle_event(Event::Resize(42, 32)).unwrap();
        panel.draw_pending(&mut terminal).unwrap();
        assert_eq!(graph::projection_count(), before);
        panel
            .handle_event(Event::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .unwrap();
        panel.draw_pending(&mut terminal).unwrap();
        assert!(!panel.history.folded());
        assert_eq!(graph::projection_count(), before + 1);
    }
    #[test]
    fn collapsing_history_moves_it_down_and_gives_worktree_more_rows() {
        for (width, height) in [(42, 32), (60, 40)] {
            let mut panel = fixture();
            panel.snapshot.as_mut().unwrap().working = crate::git::WorkingTree {
                summary: "Working tree · 30 paths  +30 -0".into(),
                files: (0..30)
                    .map(|i| crate::git::WorkingFile {
                        stat: FileStat {
                            path: format!("change-{i:02}.rs"),
                            added: "1".into(),
                            deleted: "0".into(),
                        },
                        kind: ChangeKind::Unstaged,
                    })
                    .collect(),
            };
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            panel.focus = Focus::WorkingFiles;
            panel.working_files.select(Some(29));
            terminal.draw(|f| panel.render(f)).unwrap();
            let expanded_rows = panel
                .hits
                .iter()
                .filter(|(_, h)| matches!(h, Hit::FileRow(Focus::WorkingFiles, _)))
                .count();
            let expanded_header = panel
                .hits
                .iter()
                .find(|(_, h)| matches!(h, Hit::ToggleGraph))
                .unwrap()
                .0;
            panel
                .mouse(MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: expanded_header.x,
                    row: expanded_header.y,
                    modifiers: KeyModifiers::NONE,
                })
                .unwrap();
            terminal.draw(|f| panel.render(f)).unwrap();
            let collapsed_rows = panel
                .hits
                .iter()
                .filter(|(_, h)| matches!(h, Hit::FileRow(Focus::WorkingFiles, _)))
                .count();
            let collapsed_header = panel
                .hits
                .iter()
                .find(|(_, h)| matches!(h, Hit::ToggleGraph))
                .unwrap()
                .0;
            assert!(
                collapsed_rows > expanded_rows,
                "folded history must release its height"
            );
            assert!(
                collapsed_header.y > expanded_header.y,
                "collapse should move the history bar down"
            );
            assert!(!panel
                .hits
                .iter()
                .any(|(_, h)| matches!(h, Hit::Focus(Focus::More))));
        }
    }
    #[test]
    fn jk_and_repeated_draws_reuse_graph_projection() {
        let mut panel = fixture();
        panel.focus = Focus::Graph;
        let mut terminal = Terminal::new(TestBackend::new(60, 40)).unwrap();
        terminal.draw(|f| panel.render(f)).unwrap();
        let before = graph::projection_count();
        for _ in 0..10 {
            panel
                .key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE))
                .unwrap();
            terminal.draw(|f| panel.render(f)).unwrap();
            panel
                .key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE))
                .unwrap();
            terminal.draw(|f| panel.render(f)).unwrap();
        }
        assert_eq!(
            panel.history.selection(),
            Some(GraphSelection::Commit("abc12345".into()))
        );
        assert_eq!(
            graph::projection_count(),
            before,
            "navigation must not re-project unchanged history"
        );
    }
    #[test]
    fn idle_does_not_draw_but_input_background_results_and_resize_do() {
        let mut panel = fixture();
        panel.focus = Focus::Graph;
        let mut terminal = Terminal::new(TestBackend::new(60, 40)).unwrap();
        assert!(panel.draw_pending(&mut terminal).unwrap());
        let projections = graph::projection_count();
        for _ in 0..20 {
            assert!(!panel.draw_pending(&mut terminal).unwrap());
        }
        panel
            .handle_event(Event::Mouse(MouseEvent {
                kind: MouseEventKind::Moved,
                column: 10,
                row: 10,
                modifiers: KeyModifiers::NONE,
            }))
            .unwrap();
        assert!(!panel.draw_pending(&mut terminal).unwrap());
        panel
            .handle_event(Event::Key(KeyEvent::new(
                KeyCode::Char('j'),
                KeyModifiers::NONE,
            )))
            .unwrap();
        assert!(panel.draw_pending(&mut terminal).unwrap());
        assert_eq!(graph::projection_count(), projections);
        assert!(!panel.draw_pending(&mut terminal).unwrap());
        panel.sender.send(Ok(Update::Done)).unwrap();
        panel.receive();
        assert!(panel.draw_pending(&mut terminal).unwrap());
        assert_eq!(graph::projection_count(), projections);
        terminal.backend_mut().resize(42, 32);
        terminal.autoresize().unwrap();
        panel.handle_event(Event::Resize(42, 32)).unwrap();
        assert!(panel.draw_pending(&mut terminal).unwrap());
        assert_eq!(graph::projection_count(), projections + 1);
        assert!(!panel.draw_pending(&mut terminal).unwrap());
    }
    #[test]
    fn new_repository_data_and_expansion_invalidate_cached_rows() {
        let mut panel = fixture();
        panel.focus = Focus::Graph;
        let mut terminal = Terminal::new(TestBackend::new(60, 40)).unwrap();
        panel.draw_pending(&mut terminal).unwrap();
        let projections = graph::projection_count();
        let mut snapshot = snapshot_with_history(&panel);
        snapshot.graph[0].subject = "Updated commit".into();
        panel
            .sender
            .send(Ok(Update::Snapshot(Box::new(snapshot))))
            .unwrap();
        panel.receive();
        panel.draw_pending(&mut terminal).unwrap();
        assert_eq!(graph::projection_count(), projections + 1);
        assert!(dump(&terminal)
            .iter()
            .any(|line| line.contains("Updated commit")));
        panel
            .sender
            .send(Ok(Update::Details(
                "abc12345".into(),
                "1 file changed".into(),
                vec![FileStat {
                    path: "fresh.rs".into(),
                    added: "1".into(),
                    deleted: "0".into(),
                }],
            )))
            .unwrap();
        panel.receive();
        panel.draw_pending(&mut terminal).unwrap();
        assert_eq!(graph::projection_count(), projections + 2);
        assert!(dump(&terminal).iter().any(|line| line.contains("fresh.rs")));
        panel
            .handle_event(Event::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .unwrap();
        panel.draw_pending(&mut terminal).unwrap();
        assert_eq!(graph::projection_count(), projections + 3);
        assert!(!dump(&terminal).iter().any(|line| line.contains("fresh.rs")));
    }
    #[test]
    fn working_changes_are_visible_when_committed_comparison_is_clean() {
        let mut panel = fixture();
        let snapshot = panel.snapshot.as_mut().unwrap();
        snapshot.files.clear();
        snapshot.working = crate::git::WorkingTree {
            summary: "Working tree · 1 paths  +12 -3".into(),
            files: vec![crate::git::WorkingFile {
                stat: FileStat {
                    path: "local-change.rs".into(),
                    added: "12".into(),
                    deleted: "3".into(),
                },
                kind: ChangeKind::Unstaged,
            }],
        };
        let mut terminal = Terminal::new(TestBackend::new(60, 40)).unwrap();
        panel.draw_pending(&mut terminal).unwrap();
        let lines = dump(&terminal);
        assert!(lines
            .iter()
            .any(|line| line.contains("Working tree · 1 paths  +12 -3")));
        assert!(lines
            .iter()
            .any(|line| line.contains("[U] local-change.rs")));
        assert!(lines
            .iter()
            .any(|line| line.contains("Committed on branch · no changes")));
    }
    fn inline_fixture() -> (tempfile::TempDir, Panel) {
        let root = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let result = Command::new("git")
                .arg("-C")
                .arg(root.path())
                .args(args)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
        };
        git(&["init", "-b", "main"]);
        git(&["config", "user.name", "Inline Test"]);
        git(&["config", "user.email", "inline@example.invalid"]);
        std::fs::write(root.path().join("base.txt"), "base\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-m", "Base"]);
        std::fs::write(root.path().join("a.txt"), "FIRST_INLINE_CONTENT\n").unwrap();
        std::fs::write(root.path().join("b.txt"), "SECOND_INLINE_CONTENT\n").unwrap();
        let repo = Repo::discover(root.path()).unwrap();
        let snapshot = repo.snapshot("main", 100).unwrap();
        let mut panel = Panel::new(
            repo,
            Config {
                auto_refresh: false,
                ..Config::default()
            },
        );
        panel
            .sender
            .send(Ok(Update::Snapshot(Box::new(snapshot))))
            .unwrap();
        panel.receive();
        (root, panel)
    }

    fn finish_file_action(panel: &mut Panel, terminal: &mut Terminal<TestBackend>) {
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        loop {
            panel.receive();
            panel.draw_pending(terminal).unwrap();
            if !panel.busy() && panel.pending_file_action.is_none() {
                return;
            }
            assert!(Instant::now() < deadline, "file action did not complete");
            thread::sleep(std::time::Duration::from_millis(3));
        }
    }

    #[test]
    fn space_stages_and_unstages_the_selected_file_without_losing_focus_or_drafts() {
        let (root, mut panel) = inline_fixture();
        let mut terminal = Terminal::new(TestBackend::new(100, 45)).unwrap();
        wait_inline(&mut panel, &mut terminal, "FIRST_INLINE_CONTENT");
        panel.focus = Focus::WorkingFiles;
        panel.title.insert_str("Keep draft");
        panel.key(KeyEvent::from(KeyCode::Char(' '))).unwrap();
        panel.draw_pending(&mut terminal).unwrap();
        assert_eq!(panel.focus, Focus::WorkingFiles);
        finish_file_action(&mut panel, &mut terminal);
        assert_eq!(
            panel.working_key(),
            Some((ChangeKind::Staged, "a.txt".into()))
        );
        panel.key(KeyEvent::from(KeyCode::Char(' '))).unwrap();
        finish_file_action(&mut panel, &mut terminal);
        assert_eq!(
            panel.working_key(),
            Some((ChangeKind::Untracked, "a.txt".into()))
        );
        assert_eq!(panel.focus, Focus::WorkingFiles);
        assert_eq!(text(&panel.title), "Keep draft");
        assert_eq!(
            std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
            "FIRST_INLINE_CONTENT\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("b.txt")).unwrap(),
            "SECOND_INLINE_CONTENT\n"
        );
    }

    #[test]
    fn discard_requires_y_and_n_cancels_without_mutating_the_file() {
        let (root, mut panel) = inline_fixture();
        let mut terminal = Terminal::new(TestBackend::new(100, 45)).unwrap();
        panel.focus = Focus::WorkingFiles;
        panel.key(KeyEvent::from(KeyCode::Char('d'))).unwrap();
        finish_file_action(&mut panel, &mut terminal);
        assert!(matches!(panel.dialog, Some(Dialog::Discard(_))));
        panel.key(KeyEvent::from(KeyCode::Char('n'))).unwrap();
        assert!(panel.dialog.is_none());
        assert!(root.path().join("a.txt").exists());
        panel.key(KeyEvent::from(KeyCode::Char('d'))).unwrap();
        finish_file_action(&mut panel, &mut terminal);
        panel.key(KeyEvent::from(KeyCode::Enter)).unwrap();
        assert!(matches!(panel.dialog, Some(Dialog::Discard(_))));
        assert!(root.path().join("a.txt").exists());
        panel.key(KeyEvent::from(KeyCode::Char('y'))).unwrap();
        finish_file_action(&mut panel, &mut terminal);
        assert!(!root.path().join("a.txt").exists());
        assert!(root.path().join("b.txt").exists());
        assert_eq!(
            panel.working_key(),
            Some((ChangeKind::Untracked, "b.txt".into()))
        );
    }

    #[test]
    fn hidden_or_stale_discard_confirmation_cannot_remove_new_edits() {
        let (root, mut panel) = inline_fixture();
        let mut terminal = Terminal::new(TestBackend::new(100, 45)).unwrap();
        panel.focus = Focus::WorkingFiles;
        panel.key(KeyEvent::from(KeyCode::Char('d'))).unwrap();
        finish_file_action(&mut panel, &mut terminal);
        terminal.backend_mut().resize(25, 18);
        terminal.draw(|f| panel.render(f)).unwrap();
        panel.key(KeyEvent::from(KeyCode::Char('y'))).unwrap();
        assert!(matches!(panel.dialog, Some(Dialog::Discard(_))));
        assert!(root.path().join("a.txt").exists());
        terminal.backend_mut().resize(100, 45);
        terminal.draw(|f| panel.render(f)).unwrap();
        std::fs::write(root.path().join("a.txt"), "Edited after confirmation\n").unwrap();
        panel.key(KeyEvent::from(KeyCode::Char('y'))).unwrap();
        finish_file_action(&mut panel, &mut terminal);
        assert!(panel.message.contains("changed since confirmation"));
        assert_eq!(
            std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
            "Edited after confirmation\n"
        );
    }

    #[test]
    fn a_queued_file_action_keeps_its_path_when_navigation_changes_selection() {
        let (root, mut panel) = inline_fixture();
        let mut terminal = Terminal::new(TestBackend::new(100, 45)).unwrap();
        panel.focus = Focus::WorkingFiles;
        panel.worker = Some(thread::spawn(|| ()));
        panel.background_refresh = true;
        panel.key(KeyEvent::from(KeyCode::Char(' '))).unwrap();
        panel.key(KeyEvent::from(KeyCode::Char('j'))).unwrap();
        assert_eq!(panel.working_key().unwrap().1, "b.txt");
        panel.sender.send(Ok(Update::Automatic(None))).unwrap();
        finish_file_action(&mut panel, &mut terminal);
        assert_eq!(
            panel.working_key(),
            Some((ChangeKind::Untracked, "b.txt".into()))
        );
        let staged = panel
            .repo
            .git(&["diff", "--cached", "--name-only"])
            .unwrap();
        assert_eq!(staged.trim(), "a.txt");
        assert!(root.path().join("b.txt").exists());
    }

    #[test]
    fn repeated_file_actions_cannot_replace_an_inflight_discard_confirmation() {
        let (root, mut panel) = inline_fixture();
        let mut terminal = Terminal::new(TestBackend::new(100, 45)).unwrap();
        let plan = panel.repo.prepare_discard("a.txt").unwrap();
        panel.focus = Focus::WorkingFiles;
        panel.worker = Some(thread::spawn(|| ()));
        panel.file_action_running = true;
        panel.key(KeyEvent::from(KeyCode::Char('d'))).unwrap();
        panel.key(KeyEvent::from(KeyCode::Char('j'))).unwrap();
        panel.key(KeyEvent::from(KeyCode::Char('d'))).unwrap();
        panel.key(KeyEvent::from(KeyCode::Char(' '))).unwrap();
        assert!(panel.pending_file_action.is_none());
        panel
            .sender
            .send(Ok(Update::DiscardReady(Box::new(plan))))
            .unwrap();
        finish_file_action(&mut panel, &mut terminal);
        assert!(matches!(&panel.dialog, Some(Dialog::Discard(plan)) if plan.path == "a.txt"));
        panel.key(KeyEvent::from(KeyCode::Char('y'))).unwrap();
        finish_file_action(&mut panel, &mut terminal);
        assert!(!root.path().join("a.txt").exists());
        assert_eq!(
            std::fs::read_to_string(root.path().join("b.txt")).unwrap(),
            "SECOND_INLINE_CONTENT\n"
        );
    }

    #[test]
    fn jk_crosses_list_diff_and_history_boundaries_while_tab_jumps_sections() {
        let (_root, mut panel) = inline_fixture();
        let mut terminal = Terminal::new(TestBackend::new(100, 45)).unwrap();
        wait_inline(&mut panel, &mut terminal, "FIRST_INLINE_CONTENT");
        panel.snapshot.as_mut().unwrap().files = vec![FileStat {
            path: "base.txt".into(),
            added: "1".into(),
            deleted: "0".into(),
        }];
        panel.focus = Focus::WorkingFiles;
        panel.working_files.select(Some(1));
        panel.key(KeyEvent::from(KeyCode::Char('j'))).unwrap();
        assert_eq!(panel.focus, Focus::BranchFiles);
        assert_eq!(panel.branch_files.selected(), Some(0));
        panel.key(KeyEvent::from(KeyCode::Char('k'))).unwrap();
        assert_eq!(panel.focus, Focus::WorkingFiles);
        assert_eq!(panel.working_files.selected(), Some(1));
        panel.key(KeyEvent::from(KeyCode::Char('j'))).unwrap();
        panel.key(KeyEvent::from(KeyCode::Char('j'))).unwrap();
        assert_eq!(panel.focus, Focus::Diff);
        panel.key(KeyEvent::from(KeyCode::Char('k'))).unwrap();
        assert_eq!(panel.focus, Focus::BranchFiles);
        panel.key(KeyEvent::from(KeyCode::Tab)).unwrap();
        assert_eq!(panel.focus, Focus::Diff);
        panel.key(KeyEvent::from(KeyCode::Tab)).unwrap();
        assert_eq!(panel.focus, Focus::HistoryToggle);
        panel.toggle_history();
        panel.key(KeyEvent::from(KeyCode::Char('k'))).unwrap();
        assert_eq!(panel.focus, Focus::HistoryToggle);
        panel.focus = Focus::Title;
        panel.key(KeyEvent::from(KeyCode::Char('j'))).unwrap();
        panel.key(KeyEvent::from(KeyCode::Char('k'))).unwrap();
        assert_eq!(text(&panel.title), "jk");
    }

    fn inline_screen(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    fn wait_inline(panel: &mut Panel, terminal: &mut Terminal<TestBackend>, content: &str) {
        let deadline = Instant::now() + std::time::Duration::from_secs(3);
        loop {
            panel.receive();
            panel.draw_pending(terminal).unwrap();
            if inline_screen(terminal).contains(content) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "inline preview missing {content}"
            );
            thread::sleep(std::time::Duration::from_millis(3));
        }
        assert!(panel.dialog.is_none());
    }

    #[test]
    fn selection_previews_without_popup_and_enter_escape_only_move_focus() {
        let (_root, mut panel) = inline_fixture();
        let mut terminal = Terminal::new(TestBackend::new(100, 45)).unwrap();
        wait_inline(&mut panel, &mut terminal, "FIRST_INLINE_CONTENT");
        panel.focus = Focus::WorkingFiles;
        panel.key(KeyEvent::from(KeyCode::Char('j'))).unwrap();
        panel.key(KeyEvent::from(KeyCode::Enter)).unwrap();
        assert_eq!(panel.focus, Focus::Diff);
        wait_inline(&mut panel, &mut terminal, "SECOND_INLINE_CONTENT");
        assert!(!panel.key(KeyEvent::from(KeyCode::Esc)).unwrap());
        assert_eq!(panel.focus, Focus::WorkingFiles);
        let rect = panel
            .hits
            .iter()
            .find_map(|(rect, hit)| {
                matches!(hit, Hit::FileRow(Focus::WorkingFiles, 0)).then_some(*rect)
            })
            .unwrap();
        panel
            .handle_event(Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: rect.x,
                row: rect.y,
                modifiers: KeyModifiers::NONE,
            }))
            .unwrap();
        assert_eq!(panel.focus, Focus::WorkingFiles);
        wait_inline(&mut panel, &mut terminal, "FIRST_INLINE_CONTENT");
        assert!(
            panel.worker.is_none(),
            "inline browsing must not open a popup worker"
        );
    }

    fn popup_result(panel: &mut Panel) -> DiffData {
        panel
            .worker
            .take()
            .expect("popup read worker")
            .join()
            .unwrap();
        match panel.receiver.recv().unwrap().unwrap() {
            Update::Diff(data) => data,
            _ => panic!("expected a popup diff"),
        }
    }

    #[test]
    fn shift_enter_opens_the_preview_source_and_keeps_inline_selection() {
        let (_root, mut panel) = inline_fixture();
        let mut terminal = Terminal::new(TestBackend::new(100, 45)).unwrap();
        wait_inline(&mut panel, &mut terminal, "FIRST_INLINE_CONTENT");
        panel.focus = Focus::Diff;
        panel.key(KeyEvent::from(KeyCode::Enter)).unwrap();
        assert!(panel.worker.is_none(), "plain Enter stays in the viewer");
        panel.key(KeyEvent::from(KeyCode::Char('v'))).unwrap();
        assert!(panel.preview.review_active());

        // A popup follows the preview, even if the file-list selection differs.
        panel.working_files.select(Some(1));
        panel
            .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT))
            .unwrap();
        let data = popup_result(&mut panel);
        assert!(data.patch.contains("FIRST_INLINE_CONTENT"));
        assert!(!data.patch.contains("SECOND_INLINE_CONTENT"));
        assert!(data.source.is_some());
        assert!(panel.preview.review_active());
        assert_eq!(panel.focus, Focus::Diff);

        // The local popup uses the same Viewer and layered Escape handling.
        panel.dialog = Some(Dialog::Diff(Box::new(Viewer::new(data, None))));
        terminal.draw(|f| panel.render(f)).unwrap();
        panel.key(KeyEvent::from(KeyCode::Char('v'))).unwrap();
        panel.key(KeyEvent::from(KeyCode::Down)).unwrap();
        panel.key(KeyEvent::from(KeyCode::Esc)).unwrap();
        assert!(panel.dialog.is_some(), "first Escape cancels selection");
        panel.key(KeyEvent::from(KeyCode::Esc)).unwrap();
        assert!(panel.dialog.is_none());
        assert!(panel.preview.review_active());
        assert_eq!(panel.focus, Focus::Diff);
        panel.key(KeyEvent::from(KeyCode::Esc)).unwrap();
        assert!(!panel.preview.review_active());
        assert_eq!(panel.focus, Focus::Diff);
    }

    #[test]
    fn shift_enter_uses_the_current_branch_or_history_comparison() {
        let (root, mut panel) = inline_fixture();
        panel.repo.git(&["branch", "comparison-base"]).unwrap();
        std::fs::write(root.path().join("base.txt"), "COMMITTED_POPUP\n").unwrap();
        panel.repo.git(&["add", "base.txt"]).unwrap();
        panel.repo.git(&["commit", "-m", "Change base"]).unwrap();
        let sha = panel.repo.git(&["rev-parse", "HEAD"]).unwrap();
        panel.focus = Focus::Diff;
        for request in [
            PreviewRequest::Branch {
                base: "comparison-base".into(),
                path: "base.txt".into(),
            },
            PreviewRequest::Commit {
                sha: sha.trim().into(),
                path: "base.txt".into(),
            },
        ] {
            panel.preview.select(Some(request.clone()), None);
            panel
                .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT))
                .unwrap();
            let data = popup_result(&mut panel);
            let expected = request.read(&panel.repo).unwrap();
            assert!(data.patch.contains("COMMITTED_POPUP"));
            assert_eq!(data.patch, expected.patch);
            assert_eq!(data.source, expected.source);
        }
    }

    #[test]
    fn folded_history_gives_preview_the_free_space_and_expanded_history_shares_it() {
        let (_root, mut panel) = inline_fixture();
        let mut terminal = Terminal::new(TestBackend::new(100, 50)).unwrap();
        wait_inline(&mut panel, &mut terminal, "FIRST_INLINE_CONTENT");
        let folded = panel.preview_area;
        let header = panel
            .hits
            .iter()
            .find(|(_, hit)| matches!(hit, Hit::ToggleGraph))
            .unwrap()
            .0;
        assert!(folded.height > 20);
        assert_eq!(folded.bottom(), header.y);
        panel.toggle_history();
        terminal.draw(|f| panel.render(f)).unwrap();
        assert!(panel.preview_area.height >= 4);
        assert!(panel.preview_area.height < folded.height);
        assert!(panel
            .hits
            .iter()
            .any(|(_, hit)| matches!(hit, Hit::GraphRow(_))));
        panel.toggle_history();
        terminal.draw(|f| panel.render(f)).unwrap();
        assert_eq!(panel.preview_area, folded);
    }

    #[test]
    fn very_short_panels_do_not_focus_a_preview_without_a_code_row() {
        let (_root, mut panel) = inline_fixture();
        for height in [18, 24] {
            let mut terminal = Terminal::new(TestBackend::new(100, height)).unwrap();
            terminal.draw(|f| panel.render(f)).unwrap();
            assert!(panel.preview_area.height < MIN_PREVIEW_HEIGHT);
            assert!(!panel.can_focus(Focus::Diff));
            panel.focus = Focus::WorkingFiles;
            panel.focus_preview(Focus::WorkingFiles);
            assert_eq!(panel.focus, Focus::WorkingFiles);
        }
    }

    #[test]
    fn file_save_refreshes_inline_content_even_when_file_statistics_match() {
        let (root, mut panel) = inline_fixture();
        let mut terminal = Terminal::new(TestBackend::new(100, 45)).unwrap();
        wait_inline(&mut panel, &mut terminal, "FIRST_INLINE_CONTENT");
        panel.title.insert_str("Keep draft");
        let before = panel.snapshot.as_ref().unwrap().working.clone();
        std::fs::write(root.path().join("a.txt"), "UPDATED_INLINE_TEXT\n").unwrap();
        let (status, working) = panel.repo.worktree_snapshot().unwrap();
        assert_eq!(working, before);
        panel
            .sender
            .send(Ok(Update::Automatic(Some(AutoData::Working(
                status, working,
            )))))
            .unwrap();
        panel.receive();
        wait_inline(&mut panel, &mut terminal, "UPDATED_INLINE_TEXT");
        assert_eq!(text(&panel.title), "Keep draft");
        assert_eq!(panel.working_files.selected(), Some(0));
    }

    #[test]
    fn inline_preview_initially_shows_five_context_lines_on_each_side() {
        let (root, mut panel) = inline_fixture();
        let original: String = (1..=100).map(|i| format!("context_{i:03}\n")).collect();
        std::fs::write(root.path().join("code.txt"), &original).unwrap();
        for args in [vec!["add", "code.txt"], vec!["commit", "-m", "Code"]] {
            let result = Command::new("git")
                .arg("-C")
                .arg(root.path())
                .args(args)
                .output()
                .unwrap();
            assert!(result.status.success());
        }
        std::fs::write(
            root.path().join("code.txt"),
            original.replace("context_050", "INLINE_CHANGE"),
        )
        .unwrap();
        let snapshot = panel.repo.snapshot("main", 100).unwrap();
        panel.replace_snapshot(snapshot);
        let index = panel
            .snapshot
            .as_ref()
            .unwrap()
            .working
            .files
            .iter()
            .position(|f| f.stat.path == "code.txt")
            .unwrap();
        panel.working_files.select(Some(index));
        panel.preview_origin = Focus::WorkingFiles;
        panel.sync_preview();
        let mut terminal = Terminal::new(TestBackend::new(100, 55)).unwrap();
        wait_inline(&mut panel, &mut terminal, "INLINE_CHANGE");
        let screen = inline_screen(&terminal);
        for i in [45, 46, 47, 48, 49, 51, 52, 53, 54, 55] {
            assert!(
                screen.contains(&format!("context_{i:03}")),
                "missing context line {i}"
            );
        }
        assert!(!screen.contains("context_044"));
        assert!(!screen.contains("context_056"));
    }

    #[test]
    fn review_selection_survives_file_removal_until_escape_releases_it() {
        let (root, mut panel) = inline_fixture();
        let mut terminal = Terminal::new(TestBackend::new(100, 45)).unwrap();
        wait_inline(&mut panel, &mut terminal, "FIRST_INLINE_CONTENT");
        panel.focus_preview(Focus::WorkingFiles);
        panel.key(KeyEvent::from(KeyCode::Char('v'))).unwrap();
        std::fs::remove_file(root.path().join("a.txt")).unwrap();
        let (status, working) = panel.repo.worktree_snapshot().unwrap();
        panel
            .sender
            .send(Ok(Update::Automatic(Some(AutoData::Working(
                status, working,
            )))))
            .unwrap();
        panel.receive();
        terminal.draw(|f| panel.render(f)).unwrap();
        assert!(inline_screen(&terminal).contains("FIRST_INLINE_CONTENT"));
        assert!(panel.preview_refresh_pending);
        assert!(!panel.key(KeyEvent::from(KeyCode::Esc)).unwrap());
        assert_eq!(
            panel.focus,
            Focus::Diff,
            "first Escape cancels the selection"
        );
        wait_inline(&mut panel, &mut terminal, "SECOND_INLINE_CONTENT");
        assert!(!panel.key(KeyEvent::from(KeyCode::Esc)).unwrap());
        assert_eq!(panel.focus, Focus::WorkingFiles);
    }

    fn fixture() -> Panel {
        let mut panel = Panel::new(
            Repo {
                path: std::env::temp_dir(),
            },
            Config::default(),
        );
        panel.theme = Theme::default();
        // Most fixture tests exercise the expanded graph. New-panel defaults
        // are covered separately, without changing production construction.
        panel.history.toggle();
        panel.replace_snapshot(fixture_snapshot());
        panel
    }
    fn snapshot_with_history(panel: &Panel) -> Snapshot {
        let mut snapshot = panel.snapshot.clone().unwrap();
        snapshot.graph = fixture_snapshot().graph;
        snapshot
    }
    fn fixture_snapshot() -> Snapshot {
        Snapshot {
            branch: "main".into(),
            branches: vec![
                "main".into(),
                "release/local".into(),
                "origin/release-ready".into(),
            ],
            remotes: vec!["origin".into()],
            base: "main".into(),
            status: String::new(),
            summary: "1 file changed".into(),
            files: vec![FileStat {
                path: "branch.rs".into(),
                added: "2".into(),
                deleted: "1".into(),
            }],
            working: crate::git::WorkingTree::default(),
            push: Some(("origin".into(), "main".into())),
            graph: vec![
                GraphRow {
                    lanes: "*   ".into(),
                    sha: "abc12345".into(),
                    short: "abc1234".into(),
                    subject: "Merge feature".into(),
                    refs: "HEAD -> main".into(),
                    parents: vec!["ace12345".into(), "def12345".into()],
                    ..Default::default()
                },
                GraphRow {
                    lanes: "|\\  ".into(),
                    ..Default::default()
                },
                GraphRow {
                    lanes: "| * ".into(),
                    sha: "def12345".into(),
                    short: "def1234".into(),
                    subject: "Feature".into(),
                    refs: "feature".into(),
                    parents: vec!["bad12345".into()],
                    ..Default::default()
                },
                GraphRow {
                    lanes: "* | ".into(),
                    sha: "ace12345".into(),
                    short: "ace1234".into(),
                    subject: "Main update".into(),
                    parents: vec!["bad12345".into()],
                    ..Default::default()
                },
                GraphRow {
                    lanes: "|/  ".into(),
                    ..Default::default()
                },
                GraphRow {
                    lanes: "*   ".into(),
                    sha: "bad12345".into(),
                    short: "bad1234".into(),
                    subject: "Initial".into(),
                    ..Default::default()
                },
            ],
        }
    }
    fn dump(terminal: &Terminal<TestBackend>) -> Vec<String> {
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect()
            })
            .collect()
    }
    #[test]
    fn graph_lanes_remain_aligned_with_details_at_narrow_widths() {
        for (width, height) in [(42, 32), (60, 40)] {
            let mut panel = fixture();
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|f| panel.render(f)).unwrap();
            let before = dump(&terminal);
            let lane_x = before
                .iter()
                .find(|l| l.contains("Merge feature"))
                .unwrap()
                .find('○')
                .unwrap();
            panel.history.expand(
                "abc12345".into(),
                "1 file changed".into(),
                vec![FileStat {
                    path: "feature.rs".into(),
                    added: "1".into(),
                    deleted: "0".into(),
                }],
            );
            terminal.draw(|f| panel.render(f)).unwrap();
            let after = dump(&terminal);
            assert_eq!(
                after
                    .iter()
                    .find(|l| l.contains("Merge feature") && l.contains('○'))
                    .unwrap()
                    .find('○')
                    .unwrap(),
                lane_x
            );
            assert!(after.iter().any(|l| l.contains("○─╮")));
            assert!(after.iter().any(|l| l.contains("COMMITS 4")));
            assert!(!before.iter().any(|l| l.contains("abc1234")));
            assert!(after.iter().any(|l| l.contains("feature.rs")));
            let cells = &terminal.backend().buffer().content;
            assert!(cells.iter().all(|c| c.bg == panel.theme.background
                || c.bg == panel.theme.selection
                || c.bg == panel.theme.navigation));
            assert!(cells.iter().any(|c| c.fg == panel.theme.accent));
            assert!(cells.iter().any(|c| c.fg == panel.theme.green));
        }
    }
    #[test]
    fn branch_picker_handles_search_no_match_cancel_and_arrows() {
        let mut panel = fixture();
        panel.activate().unwrap();
        for ch in "RELEASE".chars() {
            panel
                .key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE))
                .unwrap();
        }
        panel
            .key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
            .unwrap();
        let Some(Dialog::Branches(branch)) = &panel.dialog else {
            panic!("No branch dialog")
        };
        assert_eq!(
            matching_branches(
                &panel.snapshot.as_ref().unwrap().branches,
                &text(&branch.query)
            )
            .len(),
            2
        );
        assert_eq!(branch.selected, 1);
        for _ in 0..7 {
            panel
                .key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE))
                .unwrap();
        }
        for ch in "not-fetched".chars() {
            panel
                .key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE))
                .unwrap();
        }
        panel
            .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert!(panel.dialog.is_some());
        panel
            .key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        assert!(panel.dialog.is_none());
        assert_eq!(panel.snapshot.as_ref().unwrap().base, "main");
    }
    #[test]
    fn selecting_graph_skips_connectors_and_push_shows_destination() {
        let mut panel = fixture();
        panel.focus = Focus::Graph;
        panel.navigate(1);
        assert_eq!(
            panel.history.selection(),
            Some(GraphSelection::Commit("def12345".into()))
        );
        panel.navigate(-1);
        assert_eq!(
            panel.history.selection(),
            Some(GraphSelection::Commit("abc12345".into()))
        );
        panel.focus = Focus::Push;
        panel.activate().unwrap();
        assert!(matches!(panel.dialog,Some(Dialog::Push(ref r,ref t)) if r=="origin"&&t=="main"));
    }
    #[test]
    fn commit_section_can_collapse_and_reopen() {
        let mut panel = fixture();
        let mut terminal = Terminal::new(TestBackend::new(60, 40)).unwrap();
        terminal.draw(|f| panel.render(f)).unwrap();
        let rect = panel
            .hits
            .iter()
            .find(|(_, hit)| matches!(hit, Hit::ToggleGraph))
            .unwrap()
            .0;
        panel
            .mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: rect.x,
                row: rect.y,
                modifiers: KeyModifiers::NONE,
            })
            .unwrap();
        assert!(panel.history.folded());
        panel.focus = Focus::Graph;
        panel
            .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert!(!panel.history.folded());
    }
    #[test]
    fn editors_keep_unicode_and_multiline_and_shortcuts_are_configurable() {
        let mut panel = fixture();
        panel.focus = Focus::Title;
        for ch in "修复问题".chars() {
            panel
                .key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE))
                .unwrap();
        }
        assert_eq!(text(&panel.title), "修复问题");
        panel.focus = Focus::Description;
        panel
            .key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE))
            .unwrap();
        panel
            .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        panel
            .key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(text(&panel.description), "a\nb");
        assert!(key_matches(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            "ctrl+p"
        ));
        assert!(!key_matches(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE),
            "ctrl+p"
        ));
    }
}
