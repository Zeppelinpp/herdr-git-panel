use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    io::{Read, Write},
    os::unix::{ffi::OsStrExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use wait_timeout::ChildExt;

#[derive(Clone, Debug)]
pub struct Repo {
    pub path: PathBuf,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStat {
    pub path: String,
    pub added: String,
    pub deleted: String,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Unstaged,
    Staged,
    Untracked,
}
impl ChangeKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Unstaged => "unstaged",
            Self::Staged => "staged",
            Self::Untracked => "untracked",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkingFile {
    pub stat: FileStat,
    pub kind: ChangeKind,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkingTree {
    pub files: Vec<WorkingFile>,
    pub summary: String,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GraphRow {
    pub lanes: String,
    /// Git's color identity for each graph character, mapped to host theme roles.
    pub lane_colors: Vec<Option<u8>>,
    pub sha: String,
    pub short: String,
    pub subject: String,
    pub refs: String,
    pub parents: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Remote {
    pub name: String,
    pub host: String,
    pub project: String,
}
#[derive(Clone, Debug)]
pub struct Mr {
    pub remote: String,
    pub source: String,
    pub target: String,
    pub title: String,
    pub description: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub branch: String,
    pub branches: Vec<String>,
    pub remotes: Vec<String>,
    pub base: String,
    pub status: String,
    pub summary: String,
    pub files: Vec<FileStat>,
    pub working: WorkingTree,
    pub graph: Vec<GraphRow>,
    pub push: Option<(String, String)>,
}

pub fn command(program: &str, args: &[String], cwd: &Path, timeout: u64) -> Result<String> {
    command_accept(program, args, cwd, timeout, &[0])
}
fn command_accept(
    program: &str,
    args: &[String],
    cwd: &Path,
    timeout: u64,
    codes: &[i32],
) -> Result<String> {
    command_accept_input(program, args, cwd, timeout, codes, None)
}
fn command_accept_input(
    program: &str,
    args: &[String],
    cwd: &Path,
    timeout: u64,
    codes: &[i32],
    input: Option<Vec<u8>>,
) -> Result<String> {
    let deadline = Instant::now() + Duration::from_secs(timeout);
    let mut child = Command::new(program)
        .process_group(0)
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        // Read-only refreshes must not rewrite index stat data and wake the watcher.
        // Git still takes mandatory locks for add, commit and other mutations.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GH_PROMPT_DISABLED", "1")
        .env(
            "GIT_LITERAL_PATHSPECS",
            if program == "git" && args.get(1).is_some_and(|arg| arg == "check-ignore") {
                "0"
            } else {
                "1"
            },
        )
        .env("NO_COLOR", "1")
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("Could not start {program}; check it is installed and on PATH"))?;
    // Drain both pipes while waiting, including large patches and hook output.
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let stdout = thread::spawn(move || {
        let mut data = Vec::new();
        out.read_to_end(&mut data).map(|_| data)
    });
    let stderr = thread::spawn(move || {
        let mut data = Vec::new();
        err.read_to_end(&mut data).map(|_| data)
    });
    let stdin = input.map(|data| {
        let mut stdin = child.stdin.take().unwrap();
        thread::spawn(move || stdin.write_all(&data))
    });
    let terminate = |child: &mut std::process::Child| {
        // Each invocation has its own process group. Hooks and SSH helpers must
        // not survive a timeout while holding our output pipes open.
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
        let _ = child.kill();
        let _ = child.wait();
    };
    let status = match child.wait_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(Some(status)) => status,
        Ok(None) => {
            terminate(&mut child);
            bail!("{program} timed out after {timeout}s; check operation state before retrying");
        }
        Err(error) => {
            terminate(&mut child);
            return Err(error.into());
        }
    };
    // The direct child may exit before a descendant closes inherited pipes.
    // Apply the same deadline to collection; never unconditionally join readers.
    while !stdout.is_finished()
        || !stderr.is_finished()
        || stdin.as_ref().is_some_and(|writer| !writer.is_finished())
    {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            terminate(&mut child);
            bail!("{program} timed out after {timeout}s collecting output; check operation state before retrying");
        }
        thread::sleep(remaining.min(Duration::from_millis(2)));
    }
    let out = String::from_utf8_lossy(
        &stdout
            .join()
            .map_err(|_| anyhow::anyhow!("Output reader stopped"))??,
    )
    .into_owned();
    let err = String::from_utf8_lossy(
        &stderr
            .join()
            .map_err(|_| anyhow::anyhow!("Error reader stopped"))??,
    )
    .into_owned();
    let written = stdin.map(|writer| writer.join());
    if !status.code().is_some_and(|code| codes.contains(&code)) {
        bail!(
            "{}",
            if err.trim().is_empty() {
                out.trim()
            } else {
                err.trim()
            }
        );
    }
    if let Some(written) = written {
        written.map_err(|_| anyhow::anyhow!("Input writer stopped"))??;
    }
    Ok(out)
}
impl Repo {
    pub fn discover(path: impl AsRef<Path>) -> Result<Self> {
        let mut repo = Self {
            path: path.as_ref().to_path_buf(),
        };
        repo.path =
            PathBuf::from(repo.git(&["rev-parse", "--show-toplevel"])?.trim()).canonicalize()?;
        Ok(repo)
    }
    pub fn git(&self, args: &[&str]) -> Result<String> {
        let argv: Vec<String> = ["--no-pager", "-c", "color.ui=false"]
            .iter()
            .chain(args)
            .map(|s| s.to_string())
            .collect();
        command("git", &argv, &self.path, 120)
    }
    pub fn optional(&self, args: &[&str]) -> String {
        self.git(args).unwrap_or_default().trim().to_string()
    }
    pub fn branch(&self) -> String {
        self.optional(&["symbolic-ref", "--quiet", "--short", "HEAD"])
    }
    pub fn branches(&self) -> Result<Vec<String>> {
        // Entirely local. Never fetch, list remote advertisements, or contact a server.
        Ok(self
            .git(&[
                "for-each-ref",
                "--format=%(refname:short)",
                "refs/heads",
                "refs/remotes",
            ])?
            .lines()
            .filter(|s| !s.ends_with("/HEAD"))
            .map(str::to_string)
            .collect())
    }
    pub fn resolve(&self, base: &str) -> Result<String> {
        if !self.branches()?.iter().any(|b| b == base) {
            bail!("Choose a local or already fetched branch");
        }
        Ok(self
            .git(&["rev-parse", "--verify", &format!("{base}^{{commit}}")])?
            .trim()
            .to_string())
    }
    fn diff_args(&self, base: &str, commit: &str) -> Result<Vec<String>> {
        if commit.is_empty() {
            if !self.branches()?.iter().any(|branch| branch == base) {
                bail!("Choose a local or already fetched branch");
            }
            let tips = self.git(&[
                "rev-parse",
                "--revs-only",
                "--end-of-options",
                &format!("{base}^{{commit}}"),
                "HEAD^{commit}",
            ])?;
            let tips: Vec<_> = tips.lines().collect();
            if tips.len() != 2
                || tips
                    .iter()
                    .any(|tip| tip.is_empty() || !tip.chars().all(|c| c.is_ascii_hexdigit()))
            {
                bail!("Comparison requires a valid branch and HEAD commit");
            }
            Ok(vec!["diff".into(), format!("{}...{}", tips[0], tips[1])])
        } else {
            if !commit.chars().all(|c| c.is_ascii_hexdigit()) {
                bail!("Invalid commit ID");
            }
            let sha = self.git(&["rev-parse", "--verify", &format!("{commit}^{{commit}}")])?;
            Ok(vec![
                "show".into(),
                "--format=".into(),
                "--first-parent".into(),
                "--root".into(),
                sha.trim().into(),
            ])
        }
    }
    fn diff(&self, base: &str, commit: &str, options: &[&str]) -> Result<String> {
        let mut args = self.diff_args(base, commit)?;
        args.extend(options.iter().map(|s| s.to_string()));
        self.git(&args.iter().map(String::as_str).collect::<Vec<_>>())
    }
    pub fn stats(&self, base: &str, commit: &str) -> Result<(String, Vec<FileStat>)> {
        // Resolve both tips once: summary and numstat must compare the same commits.
        let args = self.diff_args(base, commit)?;
        let query = |options: &[&str]| {
            let mut argv = args.clone();
            argv.extend(options.iter().map(|s| s.to_string()));
            self.git(&argv.iter().map(String::as_str).collect::<Vec<_>>())
        };
        let summary = query(&["--stat", "--no-renames", "--"])?;
        let raw = query(&["--numstat", "-z", "--no-renames", "--"])?;
        let files = parse_numstat(&raw)?;
        Ok((summary, files))
    }
    /// The inexpensive refresh path for file/index changes; no history or refs queries.
    pub fn worktree_snapshot(&self) -> Result<(String, WorkingTree)> {
        Ok((self.git(&["status", "--short"])?, self.working_stats()?))
    }
    /// Actual metadata directories, including linked worktrees and separate git dirs.
    pub fn watch_paths(&self) -> Result<(PathBuf, PathBuf)> {
        let absolute = |flag| -> Result<PathBuf> {
            let path = self.git(&["rev-parse", "--path-format=absolute", flag])?;
            Ok(PathBuf::from(path.trim_end_matches('\n')).canonicalize()?)
        };
        Ok((absolute("--git-dir")?, absolute("--git-common-dir")?))
    }
    /// Ignore build-output events without ignoring tracked files matched by ignore rules.
    /// NUL-delimited stdin preserves non-UTF8 names and avoids argv size limits.
    pub fn has_relevant_changes(&self, paths: &[PathBuf]) -> Result<bool> {
        for batch in paths.chunks(256) {
            let argv = ["--no-pager", "check-ignore", "-z", "--stdin"].map(str::to_string);
            let mut input = Vec::new();
            for path in batch {
                let relative = if path.is_absolute() {
                    match path.strip_prefix(&self.path) {
                        Ok(path) => path,
                        Err(_) => return Ok(true),
                    }
                } else {
                    path.as_path()
                };
                if relative.as_os_str().is_empty() {
                    // Some backends only report the containing directory.
                    return Ok(true);
                }
                input.extend_from_slice(relative.as_os_str().as_bytes());
                input.push(0);
            }
            let Ok(ignored) =
                command_accept_input("git", &argv, &self.path, 120, &[0, 1], Some(input))
            else {
                // Paths inside submodules, or deleted during a save, can be
                // rejected by check-ignore. Authoritative status is the safe fallback.
                return Ok(true);
            };
            if ignored.split('\0').filter(|path| !path.is_empty()).count() < batch.len() {
                return Ok(true);
            }
        }
        Ok(false)
    }
    pub fn working_stats(&self) -> Result<WorkingTree> {
        let mut files = Vec::new();
        for (kind, cached) in [(ChangeKind::Unstaged, false), (ChangeKind::Staged, true)] {
            let mut args = vec![
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--numstat",
                "-z",
                "--no-renames",
            ];
            if cached {
                args.push("--cached");
            }
            args.push("--");
            files.extend(
                parse_numstat(&self.git(&args)?)?
                    .into_iter()
                    .map(|stat| WorkingFile { stat, kind }),
            );
        }
        let raw = self.git(&["ls-files", "--others", "--exclude-standard", "-z"])?;
        files.extend(
            raw.split('\0')
                .filter(|s| !s.is_empty())
                .map(|path| WorkingFile {
                    stat: FileStat {
                        path: path.into(),
                        added: "?".into(),
                        deleted: "?".into(),
                    },
                    kind: ChangeKind::Untracked,
                }),
        );
        let additions: u64 = files
            .iter()
            .filter_map(|f| f.stat.added.parse::<u64>().ok())
            .sum();
        let deletions: u64 = files
            .iter()
            .filter_map(|f| f.stat.deleted.parse::<u64>().ok())
            .sum();
        let paths = files
            .iter()
            .map(|f| f.stat.path.as_str())
            .collect::<std::collections::HashSet<_>>()
            .len();
        let summary = if files.is_empty() {
            "Working tree clean".into()
        } else {
            format!("Working tree · {paths} paths  +{additions} -{deletions}")
        };
        Ok(WorkingTree { files, summary })
    }
    pub fn working_patch(&self, file: &WorkingFile) -> Result<String> {
        if file.kind == ChangeKind::Untracked {
            let args = [
                "--no-pager",
                "-c",
                "color.ui=false",
                "diff",
                "--no-index",
                "--no-ext-diff",
                "--no-textconv",
                "--",
                "/dev/null",
                &file.stat.path,
            ];
            return command_accept(
                "git",
                &args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                &self.path,
                120,
                &[0, 1],
            );
        }
        let mut args = vec!["diff", "--no-ext-diff", "--no-textconv", "--no-renames"];
        if file.kind == ChangeKind::Staged {
            args.push("--cached");
        }
        args.extend(["--", &file.stat.path]);
        self.git(&args)
    }
    pub fn patch(&self, base: &str, commit: &str, path: &str) -> Result<String> {
        self.diff(
            base,
            commit,
            &["--no-ext-diff", "--no-textconv", "--no-renames", "--", path],
        )
    }
    pub fn graph(&self, limit: usize) -> Result<Vec<GraphRow>> {
        if self.optional(&["rev-parse", "--verify", "HEAD"]).is_empty() {
            return Ok(Vec::new());
        }
        let raw = self.git(&[
            "-c",
            "color.graph=blue,green,magenta,cyan,yellow,red",
            "log",
            "--graph",
            "--color=always",
            "--topo-order",
            "--decorate=short",
            &format!("-{limit}"),
            "--format=%x1f%H%x1f%h%x1f%s%x1f%d%x1f%P",
            "HEAD",
            "--",
        ])?;
        let mut rows: Vec<GraphRow> = raw
            .lines()
            .map(|line| {
                let (line, lane_colors) = decode_graph_line(line);
                let f: Vec<_> = line.splitn(6, '\x1f').collect();
                if f.len() != 6 {
                    return GraphRow {
                        lanes: line,
                        lane_colors,
                        ..Default::default()
                    };
                }
                GraphRow {
                    lanes: f[0].into(),
                    lane_colors,
                    sha: f[1].into(),
                    short: f[2].into(),
                    subject: f[3].into(),
                    refs: f[4]
                        .trim()
                        .trim_start_matches('(')
                        .trim_end_matches(')')
                        .into(),
                    parents: f[5].split_whitespace().map(str::to_string).collect(),
                }
            })
            .collect();
        assign_node_colors(&mut rows);
        Ok(rows)
    }
    pub fn snapshot(&self, selected: &str, limit: usize) -> Result<Snapshot> {
        let branches = self.branches()?;
        let branch = self.branch();
        let base = if branches.iter().any(|b| b == selected) {
            selected.to_string()
        } else {
            ["origin/main", "origin/master", "main", "master", &branch]
                .iter()
                .find(|b| branches.iter().any(|s| s == **b))
                .map(|s| s.to_string())
                .or_else(|| branches.first().cloned())
                .unwrap_or_default()
        };
        let (summary, files) = if base.is_empty() {
            (
                "No branches yet. Create the initial commit.".into(),
                Vec::new(),
            )
        } else {
            self.stats(&base, "")
                .unwrap_or_else(|e| (e.to_string(), Vec::new()))
        };
        let remotes = self.remotes()?;
        let push = self.push_target_with_remotes(&base, &remotes).ok();
        Ok(Snapshot {
            branch,
            branches,
            push,
            remotes,
            base,
            status: self.git(&["status", "--short"])?,
            summary,
            files,
            working: self.working_stats()?,
            graph: self.graph(limit)?,
        })
    }
    pub fn commit(&self, title: &str, description: &str, stage: bool) -> Result<String> {
        if title.trim().is_empty() {
            bail!("Commit title is required");
        }
        if self.branch().is_empty() {
            bail!("Check out a branch before committing");
        }
        if stage {
            self.git(&["add", "--all", "--", "."])?;
        }
        self.git(&["commit", "-m", title.trim(), "-m", description])
    }
    pub fn remotes(&self) -> Result<Vec<String>> {
        Ok(self.git(&["remote"])?.lines().map(str::to_string).collect())
    }
    pub fn push_target(&self, base: &str) -> Result<(String, String)> {
        self.push_target_with_remotes(base, &self.remotes()?)
    }
    fn push_target_with_remotes(&self, base: &str, remotes: &[String]) -> Result<(String, String)> {
        self.resolve(base)?;
        let full = self.git(&["rev-parse", "--symbolic-full-name", base])?;
        let (remote, target) = if let Some(rest) = full.trim().strip_prefix("refs/remotes/") {
            let remote = remotes
                .iter()
                .filter(|r| rest.starts_with(&format!("{r}/")))
                .max_by_key(|r| r.len())
                .context("Unknown remote")?;
            (remote.clone(), rest[remote.len() + 1..].to_string())
        } else {
            let remote = self.optional(&["config", "--get", &format!("branch.{base}.remote")]);
            let target = self.optional(&["config", "--get", &format!("branch.{base}.merge")]);
            (
                if remote.is_empty() {
                    "origin".into()
                } else {
                    remote
                },
                if target.is_empty() {
                    base.into()
                } else {
                    target.trim_start_matches("refs/heads/").into()
                },
            )
        };
        if !remotes.contains(&remote) {
            bail!("Configure an upstream or origin to push this branch");
        }
        self.git(&["check-ref-format", &format!("refs/heads/{target}")])?;
        Ok((remote, target))
    }
    pub fn push(&self, remote: &str, target: &str) -> Result<String> {
        if self.branch().is_empty() {
            bail!("Check out a branch before pushing");
        }
        if !self.remotes()?.iter().any(|r| r == remote) {
            bail!("Unknown remote");
        }
        self.git(&["check-ref-format", &format!("refs/heads/{target}")])?;
        command(
            "git",
            &[
                "push",
                "--porcelain",
                "--",
                remote,
                &format!("HEAD:refs/heads/{target}"),
            ]
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>(),
            &self.path,
            120,
        )
    }
    pub fn remote(&self, name: &str) -> Result<Remote> {
        if !self.remotes()?.iter().any(|r| r == name) {
            bail!("Unknown remote: {name}");
        }
        parse_remote(name, self.git(&["remote", "get-url", name])?.trim())
    }
    pub fn create_mr(&self, mr: &Mr, hosts: &HashMap<String, String>) -> Result<String> {
        for b in [&mr.source, &mr.target] {
            self.git(&["check-ref-format", &format!("refs/heads/{b}")])?;
        }
        if mr.title.trim().is_empty() || mr.source == mr.target {
            bail!("Title is required; source and target must differ");
        }
        let remote = self.remote(&mr.remote)?;
        let (program, args) = mr_command(&remote, mr, hosts)?;
        let out = command(&program, &args, &self.path, 120)?;
        mr_url(&out).context(
            "CLI completed without an MR URL. Check the forge before retrying to avoid duplicates.",
        )
    }
}
pub fn parse_remote(name: &str, raw: &str) -> Result<Remote> {
    let (host, project) = if raw.contains("://") {
        let url = url::Url::parse(raw)?;
        (
            url.host_str().context("Remote has no host")?.to_string(),
            url.path().trim_start_matches('/').to_string(),
        )
    } else {
        let (host, path) = raw
            .split_once(':')
            .context("MR creation needs an HTTP or SSH remote")?;
        (
            host.rsplit('@').next().unwrap_or(host).to_string(),
            path.to_string(),
        )
    };
    Ok(Remote {
        name: name.into(),
        host,
        project: project.trim_end_matches(".git").into(),
    })
}
pub fn mr_command(
    remote: &Remote,
    mr: &Mr,
    hosts: &HashMap<String, String>,
) -> Result<(String, Vec<String>)> {
    let inferred = if remote.host.contains("github") {
        "github"
    } else if remote.host.contains("gitlab") {
        "gitlab"
    } else {
        ""
    };
    let provider = hosts
        .get(&remote.host)
        .map(String::as_str)
        .unwrap_or(inferred);
    let repo = format!("{}/{}", remote.host, remote.project);
    let args = match provider {
        "github" => vec![
            "pr",
            "create",
            "--repo",
            &repo,
            "--head",
            &mr.source,
            "--base",
            &mr.target,
            "--title",
            &mr.title,
            "--body",
            &mr.description,
        ],
        "gitlab" => vec![
            "mr",
            "create",
            "--repo",
            &repo,
            "--source-branch",
            &mr.source,
            "--target-branch",
            &mr.target,
            "--title",
            &mr.title,
            "--description",
            &mr.description,
            "--yes",
        ],
        _ => bail!(
            "Unknown forge {}; map this hostname to github/gitlab in config.json",
            remote.host
        ),
    };
    Ok((
        if provider == "github" { "gh" } else { "glab" }.into(),
        args.into_iter().map(str::to_string).collect(),
    ))
}
pub fn mr_url(output: &str) -> Option<String> {
    output
        .split_whitespace()
        .rev()
        .filter_map(|word| {
            let word = word.trim_matches(|c: char| matches!(c, '\'' | '"' | ')' | '\u{1b}'));
            let url = url::Url::parse(word).ok()?;
            let parts: Vec<_> = url.path_segments()?.collect();
            (matches!(url.scheme(), "http" | "https")
                && parts.len() >= 2
                && matches!(parts[parts.len() - 2], "pull" | "merge_requests")
                && parts.last()?.parse::<u64>().is_ok())
            .then(|| word.to_string())
        })
        .next()
}
pub fn matching_branches<'a>(branches: &'a [String], query: &str) -> Vec<&'a str> {
    let tokens: Vec<_> = query
        .to_lowercase()
        .split_whitespace()
        .map(str::to_string)
        .collect();
    branches
        .iter()
        .filter(|b| tokens.iter().all(|t| b.to_lowercase().contains(t)))
        .map(String::as_str)
        .collect()
}

/// Remove Git's ANSI wrappers while retaining semantic graph colors. Keeping
/// color identities from Git matters when a lane moves after a merge/collapse.
fn decode_graph_line(input: &str) -> (String, Vec<Option<u8>>) {
    let mut chars = input.chars().peekable();
    let mut plain = String::new();
    let mut colors = Vec::new();
    let mut color = None;
    let mut prefix = true;
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            let mut sgr = String::new();
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
                sgr.push(c);
            }
            for code in sgr.split(';').filter_map(|s| {
                if s.is_empty() {
                    Some(0)
                } else {
                    s.parse::<u8>().ok()
                }
            }) {
                match code {
                    0 | 39 => color = None,
                    34 | 94 => color = Some(0),
                    32 | 92 => color = Some(1),
                    35 | 95 => color = Some(2),
                    36 | 96 => color = Some(3),
                    33 | 93 => color = Some(4),
                    31 | 91 => color = Some(5),
                    _ => (),
                }
            }
            continue;
        }
        if c == '\x1f' {
            prefix = false;
        }
        if prefix {
            colors.push(color);
        }
        plain.push(c);
    }
    (plain, colors)
}

fn assign_node_colors(rows: &mut [GraphRow]) {
    let mut pending = HashMap::<String, u8>::new();
    let mut mapping = [0, 1, 2, 3, 4, 5];
    let mut next_color = 0u8;
    for index in 0..rows.len() {
        let node = rows[index]
            .lanes
            .chars()
            .position(|c| c == '*')
            .filter(|_| !rows[index].sha.is_empty());
        let node_color = node.map(|column| {
            let color = pending.remove(&rows[index].sha).unwrap_or_else(|| {
                let c = next_color;
                next_color = (next_color + 1) % 6;
                c
            });
            for (parent_index, parent) in rows[index].parents.iter().enumerate() {
                // Existing pending parents already own a rail; a new first parent
                // inherits its child's identity, while extra parents get new rails.
                if pending.contains_key(parent) {
                    continue;
                }
                let parent_color = if parent_index == 0 {
                    color
                } else {
                    let c = next_color;
                    next_color = (next_color + 1) % 6;
                    c
                };
                pending.insert(parent.clone(), parent_color);
                let edge_column = if parent_index == 0 {
                    column
                } else {
                    column + parent_index * 2 - 1
                };
                if let Some(next) = rows.get(index + 1) {
                    let glyph = next.lanes.chars().nth(edge_column);
                    if (parent_index == 0 && glyph == Some('|'))
                        || (parent_index > 0 && glyph == Some('\\'))
                    {
                        if let Some(raw) = next.lane_colors.get(edge_column).copied().flatten() {
                            mapping[raw as usize % 6] = parent_color;
                        }
                    }
                }
            }
            color
        });
        for raw in rows[index].lane_colors.iter_mut().flatten() {
            *raw = mapping[*raw as usize % 6];
        }
        if let (Some(column), Some(color)) = (node, node_color) {
            let width = rows[index].lanes.chars().count();
            rows[index].lane_colors.resize(width, None);
            rows[index].lane_colors[column] = Some(color);
        }
    }
}
fn parse_numstat(raw: &str) -> Result<Vec<FileStat>> {
    raw.split('\0')
        .filter(|s| !s.is_empty())
        .map(|record| {
            let fields: Vec<_> = record.splitn(3, '\t').collect();
            anyhow::ensure!(fields.len() == 3, "Unexpected Git numstat output");
            Ok(FileStat {
                path: fields[2].into(),
                added: fields[0].into(),
                deleted: fields[1].into(),
            })
        })
        .collect()
}
#[cfg(test)]
mod graph_color_tests {
    use super::*;
    #[test]
    fn graph_colors_survive_lane_collapse_and_ansi_is_removed() {
        let (lanes, colors) = decode_graph_line(" \x1b[32m/\x1b[m");
        assert_eq!(lanes, " /");
        assert_eq!(colors, vec![None, Some(1)]);
        let mut rows = vec![
            GraphRow {
                lanes: "* ".into(),
                sha: "top".into(),
                parents: vec!["a".into(), "b".into()],
                ..Default::default()
            },
            GraphRow {
                lanes: "|\\ ".into(),
                lane_colors: vec![Some(5), Some(1), None],
                ..Default::default()
            },
            GraphRow {
                lanes: "| * ".into(),
                sha: "b".into(),
                parents: vec!["c".into()],
                ..Default::default()
            },
            GraphRow {
                lanes: "| * ".into(),
                sha: "c".into(),
                parents: vec!["a".into()],
                ..Default::default()
            },
            GraphRow {
                lanes: "|/ ".into(),
                lane_colors: vec![Some(5), Some(1), None],
                ..Default::default()
            },
            GraphRow {
                lanes: "* ".into(),
                sha: "a".into(),
                ..Default::default()
            },
        ];
        assign_node_colors(&mut rows);
        assert_eq!(rows[0].lane_colors[0], Some(0));
        assert_eq!(rows[1].lane_colors[0], Some(0));
        assert_eq!(rows[2].lane_colors[2], Some(1));
        assert_eq!(rows[3].lane_colors[2], Some(1));
        assert_eq!(rows[5].lane_colors[0], Some(0));
        let (plain, colors) = decode_graph_line("\x1b[34m|\x1b[m * \x1fabc\x1fshort\x1fsubject");
        assert!(!plain.contains('\x1b'));
        assert_eq!(colors.len(), 4);
        assert_eq!(colors[0], Some(0));
    }
}
