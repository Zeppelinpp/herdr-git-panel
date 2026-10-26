//! Native repository notifications, bounded aggregation and refresh scheduling.
//! No Git commands run in the filesystem callback; readers sleep when idle.
use crate::git::Repo;
use anyhow::{Context, Result};
use notify::{event::ModifyKind, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{mpsc::SyncSender, Arc, Mutex},
    time::{Duration, Instant},
};

const QUIET: Duration = Duration::from_millis(500);
const MAX_DELAY: Duration = Duration::from_secs(2);
const MIN_INTERVAL: Duration = Duration::from_secs(1);
const MAX_PATHS: usize = 256;

#[derive(Debug)]
pub enum Changes {
    Repository,
    /// Empty paths means an index/exclude change. Large path batches collapse to a common directory.
    Worktree(Vec<PathBuf>),
}
impl Changes {
    pub fn relevant(&self, repo: &Repo) -> Result<bool> {
        match self {
            Self::Repository => Ok(true),
            Self::Worktree(paths) if paths.is_empty() => Ok(true),
            Self::Worktree(paths) => repo.has_relevant_changes(paths),
        }
    }
}

struct Paths {
    root: PathBuf,
    git: PathBuf,
    common: PathBuf,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    None,
    Worktree,
    Index,
    Repository,
}
impl Paths {
    fn classify(&self, path: &Path) -> Scope {
        for dir in [&self.git, &self.common] {
            if let Ok(relative) = path.strip_prefix(dir) {
                let name = relative.to_string_lossy();
                if name.ends_with(".lock") {
                    return Scope::None;
                }
                if relative.starts_with("modules") {
                    return submodule_scope(relative);
                }
                if name == "index" || name == "info/exclude" {
                    return Scope::Index;
                }
                if [
                    "HEAD",
                    "packed-refs",
                    "config",
                    "config.worktree",
                    "shallow",
                ]
                .contains(&name.as_ref())
                    || relative.starts_with("refs")
                {
                    return Scope::Repository;
                }
                // Objects, reflogs, locks and other administrative writes are noise.
                return Scope::None;
            }
        }
        if path == self.root.join(".git") {
            return Scope::Repository;
        }
        if let Ok(relative) = path.strip_prefix(&self.root) {
            let mut parts = relative.components();
            while let Some(part) = parts.next() {
                if part.as_os_str() == ".git" {
                    // Old-form submodules keep metadata in their own .git/.
                    return submodule_scope(parts.as_path());
                }
            }
            if relative
                .file_name()
                .is_some_and(|name| name == ".gitignore")
            {
                return Scope::Index;
            }
            return Scope::Worktree;
        }
        Scope::None
    }
}

fn submodule_scope(relative: &Path) -> Scope {
    if relative.to_string_lossy().ends_with(".lock") {
        return Scope::None;
    }
    // A gitlink can change without a source-file event. Its metadata affects
    // parent working-tree status, never parent history or a network request.
    // Module names can themselves be "objects" or "logs". Match meaningful
    // metadata rather than discarding any path containing those names; extra
    // reflog signals coalesce with the associated HEAD/ref update.
    if relative.file_name().is_some_and(|part| {
        [
            "HEAD",
            "index",
            "packed-refs",
            "config",
            "shallow",
            "modules",
            "exclude",
        ]
        .iter()
        .any(|name| part == *name)
    }) || relative.components().any(|part| part.as_os_str() == "refs")
    {
        Scope::Index
    } else {
        Scope::None
    }
}

#[derive(Default)]
struct Pending {
    first: Option<Instant>,
    last: Option<Instant>,
    taken: Option<Instant>,
    repository: bool,
    rescan: bool,
    paths: HashSet<PathBuf>,
    compressed: bool,
    warning: Option<String>,
}
impl Pending {
    fn record(&mut self, scope: Scope, path: &Path, now: Instant) -> bool {
        if scope == Scope::None {
            return false;
        }
        self.first.get_or_insert(now);
        self.last = Some(now);
        self.repository |= scope == Scope::Repository;
        self.rescan |= scope == Scope::Index;
        if !self.repository && !self.rescan {
            if self.compressed || (self.paths.len() >= MAX_PATHS && !self.paths.contains(path)) {
                // Collapse a storm to its common directory, preserving ignore
                // filtering. An ignored build/ burst stays build/, not a forced
                // repository scan. Mixed source/output paths conservatively widen.
                let mut common = path.to_path_buf();
                for old in &self.paths {
                    while !old.starts_with(&common) {
                        if !common.pop() {
                            break;
                        }
                    }
                }
                self.paths.clear();
                self.paths.insert(common);
                self.compressed = true;
            } else {
                self.paths.insert(path.to_path_buf());
            }
        }
        if self.repository || self.rescan {
            self.paths.clear();
        }
        true
    }
    fn deadline(&self) -> Option<Instant> {
        let deadline = (self.last? + QUIET).min(self.first? + MAX_DELAY);
        Some(
            self.taken
                .map_or(deadline, |taken| deadline.max(taken + MIN_INTERVAL)),
        )
    }
    fn take(&mut self, now: Instant) -> Option<Changes> {
        if now < self.deadline()? {
            return None;
        }
        let paths = self.paths.drain().collect();
        let changes = if self.repository {
            Changes::Repository
        } else {
            Changes::Worktree(paths)
        };
        self.first = None;
        self.last = None;
        self.repository = false;
        self.rescan = false;
        self.compressed = false;
        self.taken = Some(now);
        Some(changes)
    }
    fn event(&mut self, paths: &Paths, event: Event, now: Instant) -> bool {
        if event.kind.is_access() {
            return false;
        }
        if event.need_rescan() {
            return self.record(Scope::Repository, &paths.root, now);
        }
        let mut changed = false;
        for path in event.paths {
            // Directory timestamp changes are incidental to creating/removing a
            // child and can turn ignored build output into false worktree work.
            if matches!(event.kind, EventKind::Modify(ModifyKind::Metadata(_))) && path.is_dir() {
                continue;
            }
            changed |= self.record(paths.classify(&path), &path, now);
        }
        changed
    }
}

pub struct RepoWatch {
    native: Mutex<RecommendedWatcher>,
    pending: Arc<Mutex<Pending>>,
    module_dirs: Vec<PathBuf>,
    watched_modules: Mutex<HashSet<PathBuf>>,
}
impl RepoWatch {
    pub fn start(repo: &Repo, wake: SyncSender<()>) -> Result<Self> {
        let (git, common) = repo.watch_paths()?;
        let paths = Paths {
            root: repo.path.clone(),
            git,
            common,
        };
        let pending = Arc::new(Mutex::new(Pending::default()));
        let callback_pending = pending.clone();
        let root = paths.root.clone();
        let git = paths.git.clone();
        let common = paths.common.clone();
        let mut native = notify::recommended_watcher(move |event: notify::Result<Event>| {
            let mut pending = callback_pending.lock().unwrap();
            let changed = match event {
                Ok(event) => pending.event(&paths, event, Instant::now()),
                Err(error) => {
                    pending.warning = Some(format!("Git watch: {error}; Ctrl+R can refresh"));
                    pending.record(Scope::Repository, &paths.root, Instant::now());
                    true
                }
            };
            if changed {
                let _ = wake.try_send(());
            }
        })?;
        native
            .watch(&root, RecursiveMode::Recursive)
            .context("Could not watch working tree")?;
        // A linked worktree stores its index/HEAD outside the working tree, and
        // shares refs/config with the main repository. Never assume root/.git.
        let mut module_dirs = Vec::new();
        for dir in [git, common] {
            if dir.starts_with(&root) {
                continue;
            }
            native.watch(&dir, RecursiveMode::NonRecursive)?;
            let modules = dir.join("modules");
            if !module_dirs.contains(&modules) {
                module_dirs.push(modules);
            }
            for child in ["refs", "info"] {
                let child = dir.join(child);
                if child.is_dir() {
                    native.watch(&child, RecursiveMode::Recursive)?;
                }
            }
        }
        let watch = Self {
            native: Mutex::new(native),
            pending,
            module_dirs,
            watched_modules: Mutex::new(HashSet::new()),
        };
        watch.watch_modules();
        Ok(watch)
    }
    pub fn take_ready(&self, now: Instant) -> Option<Changes> {
        let changes = self.pending.lock().unwrap().take(now);
        if changes.is_some() {
            self.watch_modules();
        }
        changes
    }
    fn watch_modules(&self) {
        let mut watched = self.watched_modules.lock().unwrap();
        watched.retain(|path| path.is_dir());
        for path in &self.module_dirs {
            if path.is_dir() && !watched.contains(path) {
                match self
                    .native
                    .lock()
                    .unwrap()
                    .watch(path, RecursiveMode::Recursive)
                {
                    Ok(()) => {
                        watched.insert(path.clone());
                    }
                    Err(error) => {
                        self.pending.lock().unwrap().warning =
                            Some(format!("Submodule watch: {error}; Ctrl+R can refresh"));
                    }
                }
            }
        }
    }
    pub fn wait(&self, now: Instant) -> Option<Duration> {
        self.pending
            .lock()
            .unwrap()
            .deadline()
            .map(|deadline| deadline.saturating_duration_since(now))
    }
    pub fn warning(&self) -> Option<String> {
        self.pending.lock().unwrap().warning.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn paths() -> Paths {
        Paths {
            root: "/repo".into(),
            git: "/repo/.git".into(),
            common: "/repo/.git".into(),
        }
    }
    #[test]
    fn burst_debounces_has_a_maximum_delay_and_bounded_memory() {
        let mut pending = Pending::default();
        let now = Instant::now();
        for n in 0..1000 {
            pending.record(
                Scope::Worktree,
                Path::new(&format!("/repo/{n}")),
                now + Duration::from_millis(n),
            );
        }
        assert!(pending.paths.len() <= MAX_PATHS);
        assert!(pending.take(now + Duration::from_millis(1498)).is_none());
        assert!(
            matches!(pending.take(now + Duration::from_millis(1499)), Some(Changes::Worktree(paths)) if paths == vec![PathBuf::from("/repo")])
        );
        assert!(pending.deadline().is_none(), "idle has no timer");
        for n in 0..50 {
            pending.record(
                Scope::Worktree,
                Path::new("/repo/a"),
                now + Duration::from_millis(2500 + n * 50),
            );
        }
        assert!(matches!(
            pending.take(now + Duration::from_millis(5000)),
            Some(Changes::Worktree(_))
        ));
    }
    #[test]
    fn busy_worker_can_leave_notifications_pending_and_merge_stronger_scope() {
        let mut pending = Pending::default();
        let now = Instant::now();
        pending.record(Scope::Worktree, Path::new("/repo/a"), now);
        pending.record(Scope::Repository, Path::new("/repo/.git/HEAD"), now + QUIET);
        assert!(matches!(
            pending.take(now + Duration::from_secs(3)),
            Some(Changes::Repository)
        ));
        pending.record(
            Scope::Index,
            Path::new("/repo/.git/index"),
            now + Duration::from_secs(3),
        );
        assert!(
            pending.take(now + Duration::from_millis(3500)).is_none(),
            "at most once per second"
        );
        assert!(pending.take(now + Duration::from_secs(4)).is_some());
    }
    #[test]
    fn metadata_classification_handles_linked_worktrees_and_ignores_git_noise() {
        let paths = Paths {
            root: "/linked".into(),
            git: "/repo/.git/worktrees/linked".into(),
            common: "/repo/.git".into(),
        };
        for path in [
            "/repo/.git/worktrees/linked/HEAD",
            "/repo/.git/refs/heads/main",
            "/repo/.git/config",
            "/linked/.git",
        ] {
            assert!(paths.classify(Path::new(path)) == Scope::Repository);
        }
        assert!(paths.classify(Path::new("/repo/.git/worktrees/linked/index")) == Scope::Index);
        for path in [
            "/repo/.git/objects/ab/cd",
            "/repo/.git/logs/HEAD",
            "/repo/.git/index.lock",
            "/elsewhere",
        ] {
            assert!(paths.classify(Path::new(path)) == Scope::None);
        }
        for path in [
            "/repo/.git/modules/objects/HEAD",
            "/repo/.git/modules/logs/HEAD",
            "/linked/embedded/.git/HEAD",
        ] {
            assert!(
                paths.classify(Path::new(path)) == Scope::Index,
                "must detect submodule metadata at {path}"
            );
        }
        assert!(paths.classify(Path::new("/linked/src/main.rs")) == Scope::Worktree);
    }
    #[test]
    fn reads_do_not_schedule_refresh_and_rescan_requests_recover() {
        let mut pending = Pending::default();
        let now = Instant::now();
        assert!(!pending.event(
            &paths(),
            Event::new(EventKind::Access(notify::event::AccessKind::Any))
                .add_path("/repo/a".into()),
            now
        ));
        assert!(!pending.event(
            &paths(),
            Event::new(EventKind::Modify(ModifyKind::Any)).add_path("/repo/.git/objects/ab".into()),
            now
        ));
        assert!(pending.deadline().is_none());
        let mut event = Event::new(EventKind::Other);
        event.attrs.set_flag(notify::event::Flag::Rescan);
        assert!(pending.event(&paths(), event, now));
        assert!(matches!(
            pending.take(now + QUIET),
            Some(Changes::Repository)
        ));
    }
}
