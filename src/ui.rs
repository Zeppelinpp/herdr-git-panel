use crate::{
    git::{matching_branches, ChangeKind, FileStat, Mr, Repo, Snapshot},
    graph::{self, Item as GraphItem},
    host::{self, Config, DiffData},
    theme::{Theme, NAVIGATION},
};
use anyhow::{bail, Result};
use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
        MouseEventKind,
    },
    execute,
};
use ratatui::{
    backend::Backend,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState, Wrap},
    DefaultTerminal, Frame, Terminal,
};
use std::{
    env, io,
    process::{Command, Stdio},
    sync::mpsc::{self, Receiver, Sender},
    thread::{self, JoinHandle},
    time::Duration,
};
use tui_textarea::TextArea;

const NORMAL: Style = Style::new();
const BOLD: Style = Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD);
const DIM: Style = Style::new().fg(Color::Gray).add_modifier(Modifier::DIM);
const SELECTED: Style = Style::new().add_modifier(Modifier::REVERSED);

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
    Graph,
    CommitFiles,
    More,
    Link,
}
const FOCI: [Focus; 13] = [
    Focus::Base,
    Focus::Title,
    Focus::Description,
    Focus::Stage,
    Focus::Commit,
    Focus::Push,
    Focus::Mr,
    Focus::WorkingFiles,
    Focus::BranchFiles,
    Focus::Graph,
    Focus::CommitFiles,
    Focus::More,
    Focus::Link,
];
#[derive(Clone, Debug)]
enum Hit {
    Focus(Focus),
    BranchRow(usize),
    GraphRow(usize),
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
struct Viewer {
    title: String,
    patch: String,
    vertical: u16,
    horizontal: u16,
}
enum Dialog {
    Branches(Box<BranchDialog>),
    Mr(MrDialog),
    Push(String, String),
    Diff(Viewer),
}
enum Update {
    Snapshot(Box<Snapshot>),
    Details(String, String, Vec<FileStat>),
    Committed(String),
    Pushed(String),
    Done,
    Mr(String),
    Diff(DiffData),
}
struct Details {
    sha: String,
    summary: String,
    files: Vec<FileStat>,
}

struct GraphCache {
    width: usize,
    valid: bool,
    table: Table<'static>,
    items: Vec<GraphItem>,
    selectable: Vec<usize>,
    commits: Vec<Option<usize>>,
    files: Vec<usize>,
    commit_count: usize,
}
impl Default for GraphCache {
    fn default() -> Self {
        Self {
            width: 80,
            valid: false,
            table: Table::default(),
            items: Vec::new(),
            selectable: Vec::new(),
            commits: Vec::new(),
            files: Vec::new(),
            commit_count: 0,
        }
    }
}
impl GraphCache {
    fn invalidate(&mut self) {
        self.valid = false;
    }
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
    snapshot: Option<Snapshot>,
    title: TextArea<'static>,
    description: TextArea<'static>,
    focus: Focus,
    stage: bool,
    message: String,
    last_mr: String,
    dialog: Option<Dialog>,
    graph: TableState,
    graph_collapsed: bool,
    graph_cache: GraphCache,
    file_cache: FileCache,
    working_files: ListState,
    branch_files: ListState,
    commit_files: ListState,
    details: Option<Details>,
    limit: usize,
    hits: Vec<(Rect, Hit)>,
    sender: Sender<Result<Update>>,
    receiver: Receiver<Result<Update>>,
    worker: Option<JoinHandle<()>>,
    refresh_notice: Option<String>,
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
    input.set_cursor_style(if active { SELECTED } else { NORMAL });
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
            graph: TableState::default().with_selected(0),
            graph_collapsed: false,
            graph_cache: GraphCache::default(),
            file_cache: FileCache::default(),
            working_files: ListState::default().with_selected(Some(0)),
            branch_files: ListState::default().with_selected(Some(0)),
            commit_files: ListState::default().with_selected(Some(0)),
            details: None,
            limit,
            hits: Vec::new(),
            sender,
            receiver,
            worker: None,
            refresh_notice: None,
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
        let sender = self.sender.clone();
        self.worker = Some(thread::spawn(move || {
            let _ = sender.send(job());
        }));
    }
    pub fn refresh(&mut self) {
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
        let Ok(result) = self.receiver.try_recv() else {
            return;
        };
        self.dirty = true;
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        match result {
            Err(e) => self.message = format!("{e:#}"),
            Ok(Update::Snapshot(snapshot)) => {
                self.snapshot = Some(*snapshot);
                self.message = self.refresh_notice.take().unwrap_or_default();
                self.details = None;
                self.graph_cache.invalidate();
                self.file_cache.valid = false;
                self.working_files = ListState::default().with_selected(Some(0));
                self.branch_files = ListState::default().with_selected(Some(0));
            }
            Ok(Update::Details(sha, summary, files)) => {
                let raw_index = self
                    .snapshot
                    .as_ref()
                    .and_then(|s| s.graph.iter().position(|r| r.sha == sha));
                self.details = Some(Details {
                    sha,
                    summary,
                    files,
                });
                self.graph_cache.invalidate();
                if let Some(raw_index) = raw_index {
                    self.ensure_graph(self.graph_cache.width);
                    let selected = self.graph_cache.commits.get(raw_index).copied().flatten();
                    self.graph.select(selected);
                }
                self.commit_files.select(Some(0));
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
                    self.dialog = Some(Dialog::Diff(Viewer {
                        title: data.title,
                        patch: data.patch,
                        vertical: 0,
                        horizontal: 0,
                    }));
                }
            }
            Ok(Update::Done) => self.message.clear(),
        }
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
            if focus == Focus::CommitFiles && self.details.is_none()
                || focus == Focus::Link && self.last_mr.is_empty()
            {
                continue;
            }
            self.focus = focus;
            break;
        }
    }
    fn select_graph(&mut self) {
        if self.graph_collapsed {
            self.graph_collapsed = false;
            return;
        }
        self.ensure_graph(self.graph_cache.width);
        let item = self
            .graph_cache
            .items
            .get(self.graph.selected().unwrap_or(0))
            .copied();
        let raw_index = match item {
            Some(GraphItem::File(index)) => {
                self.open_file(true, index);
                return;
            }
            Some(GraphItem::Commit(index)) => index,
            _ => return,
        };
        let row = self
            .snapshot
            .as_ref()
            .and_then(|s| s.graph.get(raw_index))
            .cloned();
        let Some(row) = row.filter(|r| !r.sha.is_empty()) else {
            return;
        };
        if self.details.as_ref().is_some_and(|d| d.sha == row.sha) {
            self.details = None;
            self.graph_cache.invalidate();
            return;
        }
        let repo = self.repo.clone();
        self.work("Reading commit…", move || {
            let (summary, files) = repo.stats("", &row.sha)?;
            Ok(Update::Details(row.sha, summary, files))
        });
    }
    fn open_file(&mut self, commit: bool, index: usize) {
        let data = if commit {
            self.details.as_ref().and_then(|d| {
                d.files
                    .get(index)
                    .map(|f| (String::new(), d.sha.clone(), f.path.clone()))
            })
        } else {
            self.snapshot.as_ref().and_then(|s| {
                s.files
                    .get(index)
                    .map(|f| (s.base.clone(), String::new(), f.path.clone()))
            })
        };
        let Some((base, sha, path)) = data else {
            return;
        };
        let repo = self.repo.clone();
        self.work("Reading diff…", move || {
            Ok(Update::Diff(DiffData {
                title: format!(
                    "{} · {}",
                    if sha.is_empty() {
                        format!("{base}…HEAD")
                    } else {
                        sha[..8.min(sha.len())].to_string()
                    },
                    visible(&path)
                ),
                patch: repo.patch(&base, &sha, &path)?,
            }))
        });
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
        let repo = self.repo.clone();
        self.work("Reading working tree diff…", move || {
            Ok(Update::Diff(DiffData {
                title: format!("{} · {}", file.kind.label(), visible(&file.stat.path)),
                patch: repo.working_patch(&file)?,
            }))
        });
    }
    fn activate(&mut self) -> Result<()> {
        if self.busy() {
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
                let (remote, target) = snapshot
                    .push
                    .clone()
                    .unwrap_or((self.cfg.default_remote.clone(), snapshot.base.clone()));
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
            Focus::BranchFiles => self.open_file(false, self.branch_files.selected().unwrap_or(0)),
            Focus::WorkingFiles => {
                self.open_working_file(self.working_files.selected().unwrap_or(0))
            }
            Focus::CommitFiles => self.open_file(true, self.commit_files.selected().unwrap_or(0)),
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
                self.ensure_graph(self.graph_cache.width);
                let indices = &self.graph_cache.selectable;
                if !indices.is_empty() {
                    let position = indices
                        .binary_search(&self.graph.selected().unwrap_or(0))
                        .unwrap_or_else(|i| i.min(indices.len() - 1));
                    let next = position.saturating_add_signed(delta).min(indices.len() - 1);
                    self.graph.select(Some(indices[next]));
                }
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
            Focus::CommitFiles => move_list(
                &mut self.commit_files,
                self.details.as_ref().map_or(0, |d| d.files.len()),
                delta,
            ),
            _ => (),
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
        if self.busy() {
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
                KeyCode::Down | KeyCode::Char('j') => self.navigate(1),
                KeyCode::Up | KeyCode::Char('k') => self.navigate(-1),
                KeyCode::PageDown => self.navigate(8),
                KeyCode::PageUp => self.navigate(-8),
                _ => (),
            },
        }
        Ok(false)
    }
    fn dialog_key(&mut self, dialog: &mut Dialog, key: KeyEvent) -> Result<bool> {
        if key.code == KeyCode::Esc {
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
                if viewer_key(viewer, key) {
                    return Ok(true);
                }
            }
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
                match dialog {
                    Dialog::Branches(branch) => {
                        let count = self.snapshot.as_ref().map_or(0, |s| {
                            matching_branches(&s.branches, &text(&branch.query)).len()
                        });
                        branch.selected = branch
                            .selected
                            .saturating_add_signed(delta)
                            .min(count.saturating_sub(1));
                    }
                    Dialog::Diff(viewer) => {
                        viewer.vertical = viewer.vertical.saturating_add_signed(delta as i16 * 3)
                    }
                    _ => (),
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
                (_, Hit::Cancel) => close = true,
                _ => (),
            }
            if !close {
                self.dialog = Some(dialog);
            }
        } else if !self.busy() {
            match hit {
                Hit::Focus(focus) => {
                    self.focus = focus;
                    if !matches!(
                        focus,
                        Focus::Title
                            | Focus::Description
                            | Focus::Graph
                            | Focus::BranchFiles
                            | Focus::WorkingFiles
                            | Focus::CommitFiles
                    ) {
                        self.activate()?;
                    }
                }
                Hit::GraphRow(index) => {
                    self.focus = Focus::Graph;
                    self.graph.select(Some(index));
                    self.activate()?;
                }
                Hit::FileRow(focus, index) => {
                    self.focus = focus;
                    match focus {
                        Focus::WorkingFiles => {
                            self.working_files.select(Some(index));
                            self.open_working_file(index);
                        }
                        Focus::BranchFiles => {
                            self.branch_files.select(Some(index));
                            self.open_file(false, index);
                        }
                        Focus::CommitFiles => {
                            self.commit_files.select(Some(index));
                            self.open_file(true, index);
                        }
                        _ => (),
                    }
                }
                Hit::Refresh => self.refresh(),
                Hit::ToggleGraph => self.graph_collapsed = !self.graph_collapsed,
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
                } else if !self.busy() {
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
        self.hits.clear();
        let area = frame.area();
        if area.width < 26 || area.height < 22 {
            frame.render_widget(
                Paragraph::new("Resize panel to at least 26 × 22. Esc closes."),
                area,
            );
            self.theme.apply(frame.buffer_mut());
            return;
        }
        let outer = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Length(1),
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
            Paragraph::new(format!("Compare: {base} ▾")).style(if self.focus == Focus::Base {
                SELECTED
            } else {
                NORMAL
            }),
            outer[1],
        );
        self.hits.push((outer[1], Hit::Focus(Focus::Base)));
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
        let buttons = Layout::horizontal([
            Constraint::Length(11),
            Constraint::Length(9),
            Constraint::Min(9),
        ])
        .split(outer[5]);
        for (rect, focus, title) in [
            (buttons[0], Focus::Commit, "[Commit]"),
            (buttons[1], Focus::Push, "[Push]"),
            (buttons[2], Focus::Mr, "[Create MR]"),
        ] {
            frame.render_widget(
                Paragraph::new(title).style(if self.focus == focus { SELECTED } else { BOLD }),
                rect,
            );
            self.hits.push((rect, Hit::Focus(focus)));
        }
        let info = if !self.message.is_empty() {
            visible(&self.message)
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
        footer(
            frame,
            outer[8],
            &format!(
                "Tab focus  Enter open  ^R refresh  {} push  Esc close",
                self.cfg.push_key
            ),
        );
        if let Some(mut dialog) = self.dialog.take() {
            self.render_dialog(frame, &mut dialog);
            self.dialog = Some(dialog);
        }
        self.theme.apply(frame.buffer_mut());
    }
    fn ensure_graph(&mut self, width: usize) {
        if self.graph_cache.valid && self.graph_cache.width == width {
            return;
        }
        let projected = self
            .snapshot
            .as_ref()
            .map(|s| {
                graph::project(
                    &s.graph,
                    self.details.as_ref().map(|d| graph::Expanded {
                        sha: &d.sha,
                        summary: &d.summary,
                        files: &d.files,
                    }),
                    width,
                )
            })
            .unwrap_or_default();
        let mut items = Vec::with_capacity(projected.len());
        let mut selectable = Vec::new();
        let mut commits = vec![None; self.snapshot.as_ref().map_or(0, |s| s.graph.len())];
        let mut files = Vec::new();
        let mut rows = Vec::with_capacity(projected.len());
        let mut commit_count = 0;
        for (index, row) in projected.into_iter().enumerate() {
            if row.item.selectable() {
                selectable.push(index);
            }
            match row.item {
                GraphItem::Commit(raw) => {
                    commits[raw] = Some(index);
                    commit_count += 1;
                }
                GraphItem::File(_) => files.push(index),
                _ => (),
            }
            items.push(row.item);
            rows.push(Row::new(vec![row.line]));
        }
        self.graph_cache = GraphCache {
            width,
            valid: true,
            items,
            selectable,
            commits,
            files,
            commit_count,
            table: Table::new(rows, [Constraint::Percentage(100)])
                .column_spacing(0)
                .row_highlight_style(NAVIGATION),
        };
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
        self.ensure_graph(area.width as usize);
        self.ensure_files();
        let file_count = self.snapshot.as_ref().map_or(0, |s| s.files.len());
        let working_count = self.snapshot.as_ref().map_or(0, |s| s.working.files.len());
        let branch_height = (file_count as u16 + 1).min(4);
        let layout = Layout::vertical([
            Constraint::Length((working_count as u16 + 1).min(5)),
            Constraint::Length(branch_height),
            Constraint::Min(3),
            Constraint::Length(if self.last_mr.is_empty() { 1 } else { 2 }),
        ])
        .split(area);
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
            let count = self.graph_cache.commit_count;
            let header = Rect::new(layout[2].x, layout[2].y, layout[2].width, 1);
            let label = format!(
                "{} COMMITS {count}{}",
                if self.graph_collapsed { "›" } else { "▾" },
                if count >= self.limit { " +" } else { "" }
            );
            frame.render_widget(
                Paragraph::new(label).style(DIM.add_modifier(Modifier::BOLD)),
                header,
            );
            let refresh = Rect::new(header.right().saturating_sub(2), header.y, 2, 1);
            frame.render_widget(Paragraph::new("↻").style(DIM), refresh);
            self.hits.push((header, Hit::Focus(Focus::Graph)));
            self.hits
                .push((Rect::new(header.x, header.y, 2, 1), Hit::ToggleGraph));
            self.hits.push((refresh, Hit::Refresh));
            let body = Rect::new(
                layout[2].x,
                layout[2].y + 1,
                layout[2].width,
                layout[2].height.saturating_sub(1),
            );
            if !self.graph_collapsed {
                if self.focus == Focus::CommitFiles {
                    if let Some(index) = self
                        .graph_cache
                        .files
                        .get(self.commit_files.selected().unwrap_or(0))
                    {
                        self.graph.select(Some(*index));
                    }
                }
                frame.render_stateful_widget(&self.graph_cache.table, body, &mut self.graph);
                self.hits.push((body, Hit::Focus(Focus::Graph)));
                for y in 0..body.height {
                    let index = self.graph.offset() + y as usize;
                    if self
                        .graph_cache
                        .items
                        .get(index)
                        .is_some_and(|item| item.selectable())
                    {
                        self.hits.push((
                            Rect::new(body.x, body.y + y, body.width, 1),
                            Hit::GraphRow(index),
                        ));
                    }
                }
            }
        }
        let more = Rect::new(layout[3].x, layout[3].y, layout[3].width, 1);
        frame.render_widget(
            Paragraph::new("[Load older commits]").style(if self.focus == Focus::More {
                SELECTED
            } else {
                DIM
            }),
            more,
        );
        self.hits.push((more, Hit::Focus(Focus::More)));
        if !self.last_mr.is_empty() {
            let rect = Rect::new(layout[3].x, layout[3].y + 1, layout[3].width, 1);
            frame.render_widget(
                Paragraph::new(format!(
                    "!MR {} ↗",
                    self.last_mr.rsplit('/').next().unwrap_or("")
                ))
                .style(NORMAL.add_modifier(Modifier::UNDERLINED)),
                rect,
            );
            self.hits.push((rect, Hit::Focus(Focus::Link)));
        }
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
                let block = Block::bordered().title("Create MR / PR");
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
                render_viewer(frame, area, viewer);
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
    hits.push((Rect::new(area.x, area.y, area.width, 1), Hit::Focus(focus)));
}
fn viewer_key(viewer: &mut Viewer, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => return true,
        KeyCode::Down | KeyCode::Char('j') => viewer.vertical = viewer.vertical.saturating_add(1),
        KeyCode::Up | KeyCode::Char('k') => viewer.vertical = viewer.vertical.saturating_sub(1),
        KeyCode::PageDown => viewer.vertical = viewer.vertical.saturating_add(15),
        KeyCode::PageUp => viewer.vertical = viewer.vertical.saturating_sub(15),
        KeyCode::Right => viewer.horizontal = viewer.horizontal.saturating_add(4),
        KeyCode::Left => viewer.horizontal = viewer.horizontal.saturating_sub(4),
        KeyCode::Home => {
            viewer.vertical = 0;
            viewer.horizontal = 0;
        }
        _ => (),
    };
    false
}
fn render_viewer(frame: &mut Frame, area: Rect, viewer: &Viewer) {
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(area);
    frame.render_widget(Paragraph::new(visible(&viewer.title)).style(BOLD), rows[0]);
    let lines = viewer
        .patch
        .lines()
        .map(|line| {
            let style = if line.starts_with("diff ")
                || line.starts_with("---")
                || line.starts_with("+++")
            {
                BOLD
            } else if line.starts_with('+') {
                NORMAL.fg(Color::Green)
            } else if line.starts_with('-') {
                NORMAL.fg(Color::Red)
            } else if line.starts_with("@@") {
                NORMAL.fg(Color::Cyan)
            } else {
                NORMAL
            };
            Line::styled(line.replace('\t', "    "), style)
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines).scroll((viewer.vertical, viewer.horizontal)),
        rows[1],
    );
    footer(
        frame,
        rows[2],
        "↑↓ scroll  ←→ horizontal  PgUp/PgDn  Esc/q close",
    );
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
        let _ = execute!(io::stdout(), DisableMouseCapture, DisableBracketedPaste);
        ratatui::restore();
    }
}
fn terminal() -> Result<(DefaultTerminal, TerminalGuard)> {
    let terminal = ratatui::try_init()?;
    let guard = TerminalGuard;
    execute!(io::stdout(), EnableMouseCapture, EnableBracketedPaste)?;
    Ok((terminal, guard))
}
pub fn run(repo: Repo, cfg: Config) -> Result<()> {
    let mut panel = Panel::new(repo, cfg);
    panel.refresh();
    let (mut terminal, _guard) = terminal()?;
    loop {
        panel.receive();
        panel.draw_pending(&mut terminal)?;
        // Only poll while awaiting a worker result. With no pending work,
        // block on terminal input and do not redraw or wake on a timer.
        if panel.busy() && !event::poll(Duration::from_millis(50))? {
            continue;
        }
        match panel.handle_event(event::read()?) {
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
pub fn run_diff(data: DiffData) -> Result<()> {
    let theme = Theme::load()?;
    let mut viewer = Viewer {
        title: data.title,
        patch: data.patch,
        vertical: 0,
        horizontal: 0,
    };
    let (mut terminal, _guard) = terminal()?;
    loop {
        terminal.draw(|frame| {
            render_viewer(frame, frame.area(), &viewer);
            theme.apply(frame.buffer_mut());
        })?;
        match event::read()? {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                if viewer_key(&mut viewer, key) {
                    break;
                }
            }
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollDown => viewer.vertical = viewer.vertical.saturating_add(3),
                MouseEventKind::ScrollUp => viewer.vertical = viewer.vertical.saturating_sub(3),
                _ => (),
            },
            _ => (),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::GraphRow;
    use ratatui::{backend::TestBackend, Terminal};
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
        assert_eq!(panel.graph.selected(), Some(0));
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
        let mut snapshot = panel.snapshot.clone().unwrap();
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
    fn fixture() -> Panel {
        let mut panel = Panel::new(
            Repo {
                path: std::env::temp_dir(),
            },
            Config::default(),
        );
        panel.theme = Theme::default();
        panel.snapshot = Some(Snapshot {
            branch: "main".into(),
            branches: vec![
                "main".into(),
                "release/local".into(),
                "origin/release-ready".into(),
            ],
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
                    parents: vec!["a".into(), "b".into()],
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
                    ..Default::default()
                },
                GraphRow {
                    lanes: "* | ".into(),
                    sha: "ace12345".into(),
                    short: "ace1234".into(),
                    subject: "Main update".into(),
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
        });
        panel
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
            panel.details = Some(Details {
                sha: "abc12345".into(),
                summary: "1 file changed".into(),
                files: vec![FileStat {
                    path: "feature.rs".into(),
                    added: "1".into(),
                    deleted: "0".into(),
                }],
            });
            panel.graph_cache.invalidate();
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
            assert!(after.iter().any(|l| l.contains("│╲")));
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
        assert_eq!(panel.graph.selected(), Some(2));
        panel.navigate(-1);
        assert_eq!(panel.graph.selected(), Some(0));
        panel.focus = Focus::Push;
        panel.activate().unwrap();
        assert!(matches!(panel.dialog,Some(Dialog::Push(ref r,ref t)) if r=="origin"&&t=="main"));
    }
    #[test]
    fn inline_files_are_navigable_and_switching_details_preserves_commit_identity() {
        let mut panel = fixture();
        panel.focus = Focus::Graph;
        panel.details = Some(Details {
            sha: "abc12345".into(),
            summary: "1 file changed".into(),
            files: vec![FileStat {
                path: "merge.rs".into(),
                added: "2".into(),
                deleted: "0".into(),
            }],
        });
        panel.navigate(1);
        assert_eq!(
            panel.graph_cache.items[panel.graph.selected().unwrap()],
            GraphItem::File(0)
        );
        panel.navigate(1);
        assert_eq!(
            panel.graph_cache.items[panel.graph.selected().unwrap()],
            GraphItem::Commit(2)
        );
        panel
            .sender
            .send(Ok(Update::Details(
                "def12345".into(),
                "1 file changed".into(),
                vec![FileStat {
                    path: "feature.rs".into(),
                    added: "3".into(),
                    deleted: "1".into(),
                }],
            )))
            .unwrap();
        panel.receive();
        assert_eq!(
            panel.graph_cache.items[panel.graph.selected().unwrap()],
            GraphItem::Commit(2)
        );
        panel.select_graph();
        assert!(panel.details.is_none());
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
        assert!(panel.graph_collapsed);
        panel.focus = Focus::Graph;
        panel
            .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert!(!panel.graph_collapsed);
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
