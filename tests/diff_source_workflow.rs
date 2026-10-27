use herdr_git_panel::git::{ChangeKind, FileStat, Repo, WorkingFile};
use std::{fs, os::unix::fs::symlink, path::Path, process::Command};

fn git(path: &Path, args: &[&str]) -> String {
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
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn fixture() -> (tempfile::TempDir, Repo) {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-b", "main"]);
    git(dir.path(), &["config", "user.name", "Test"]);
    git(
        dir.path(),
        &["config", "user.email", "test@example.invalid"],
    );
    fs::write(dir.path().join("file.txt"), "first\noriginal\nlast\n").unwrap();
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-m", "Initial"]);
    let repo = Repo::discover(dir.path()).unwrap();
    (dir, repo)
}

fn file(path: &str, kind: ChangeKind) -> WorkingFile {
    WorkingFile {
        stat: FileStat {
            path: path.into(),
            added: String::new(),
            deleted: String::new(),
        },
        kind,
    }
}

#[test]
fn worktree_context_rejects_changes_and_refresh_replaces_snapshot() {
    let (dir, repo) = fixture();
    let original = "first\nedited\nlast\n";
    fs::write(dir.path().join("file.txt"), original).unwrap();
    let data = repo
        .working_diff_data(&file("file.txt", ChangeKind::Unstaged))
        .unwrap();
    let source = data.source.unwrap();
    assert_eq!(source.load_context().unwrap(), original);
    // Same size changes must still invalidate the original metadata stamp.
    let updated = "first\nupdate\nlast\n";
    fs::write(dir.path().join("file.txt"), updated).unwrap();
    assert!(source
        .load_context()
        .unwrap_err()
        .to_string()
        .contains("press r"));
    let refreshed = source.reload().unwrap();
    assert!(refreshed.patch.contains("+update"));
    assert_eq!(refreshed.source.unwrap().load_context().unwrap(), updated);
}

#[test]
fn staged_context_stays_pinned_when_index_and_worktree_change() {
    let (dir, repo) = fixture();
    let staged = "first\nstaged\nlast\n";
    fs::write(dir.path().join("file.txt"), staged).unwrap();
    git(dir.path(), &["add", "file.txt"]);
    let data = repo
        .working_diff_data(&file("file.txt", ChangeKind::Staged))
        .unwrap();
    fs::write(dir.path().join("file.txt"), "new index\n").unwrap();
    git(dir.path(), &["add", "file.txt"]);
    assert_eq!(data.source.unwrap().load_context().unwrap(), staged);
}

#[test]
fn root_and_branch_context_stay_pinned_after_head_moves() {
    let (dir, repo) = fixture();
    let root = git(dir.path(), &["rev-parse", "HEAD"]);
    let root_data = repo.diff_data("", &root, "file.txt").unwrap();
    assert_eq!(
        root_data.source.unwrap().load_context().unwrap(),
        "first\noriginal\nlast\n"
    );
    git(dir.path(), &["checkout", "-b", "feature"]);
    let changed = "first\nfeature\nlast\n";
    fs::write(dir.path().join("file.txt"), changed).unwrap();
    git(dir.path(), &["commit", "-am", "Feature"]);
    let data = repo.diff_data("main", "", "file.txt").unwrap();
    fs::write(dir.path().join("file.txt"), "later\n").unwrap();
    git(dir.path(), &["commit", "-am", "Later"]);
    assert_eq!(data.source.unwrap().load_context().unwrap(), changed);
}

#[test]
fn deleted_empty_untracked_and_special_paths_have_correct_context() {
    let (dir, repo) = fixture();
    fs::remove_file(dir.path().join("file.txt")).unwrap();
    let deleted = repo
        .working_diff_data(&file("file.txt", ChangeKind::Unstaged))
        .unwrap();
    assert_eq!(deleted.source.unwrap().load_context().unwrap(), "");
    let odd = "[odd]\tname\n.txt";
    fs::write(dir.path().join(odd), "content without final newline").unwrap();
    let data = repo
        .working_diff_data(&file(odd, ChangeKind::Untracked))
        .unwrap();
    let serialized = serde_json::to_string(&data).unwrap();
    let data: herdr_git_panel::host::DiffData = serde_json::from_str(&serialized).unwrap();
    assert_eq!(
        data.source.unwrap().load_context().unwrap(),
        "content without final newline"
    );
    fs::write(dir.path().join("empty"), "").unwrap();
    let data = repo
        .working_diff_data(&file("empty", ChangeKind::Untracked))
        .unwrap();
    assert_eq!(data.source.unwrap().load_context().unwrap(), "");
}

#[test]
fn symlink_context_is_the_link_target_text() {
    let (dir, repo) = fixture();
    symlink("file.txt", dir.path().join("link")).unwrap();
    let data = repo
        .working_diff_data(&file("link", ChangeKind::Untracked))
        .unwrap();
    assert_eq!(data.source.unwrap().load_context().unwrap(), "file.txt");
}

#[test]
fn binary_files_do_not_expose_text_context() {
    let (dir, repo) = fixture();
    fs::write(dir.path().join("binary"), b"hello\0world").unwrap();
    let data = repo
        .working_diff_data(&file("binary", ChangeKind::Untracked))
        .unwrap();
    assert!(data
        .source
        .unwrap()
        .load_context()
        .unwrap_err()
        .to_string()
        .contains("Binary"));
}

#[test]
fn patch_format_and_initial_context_are_stable_under_git_configuration() {
    let (dir, repo) = fixture();
    let content = (1..=100)
        .map(|n| {
            if n == 51 {
                "\n".into()
            } else {
                format!("line {n}\n")
            }
        })
        .collect::<String>();
    fs::write(dir.path().join("file.txt"), &content).unwrap();
    git(dir.path(), &["commit", "-am", "Long file"]);
    for (key, value) in [
        ("diff.context", "10000"),
        ("diff.interHunkContext", "10000"),
        ("diff.suppressBlankEmpty", "true"),
        ("diff.algorithm", "histogram"),
        ("diff.relative", "true"),
        ("diff.wordRegex", "."),
        ("color.diff", "always"),
    ] {
        git(dir.path(), &["config", key, value]);
    }
    let changed = content.replace("line 50\n", "edited\n");
    fs::write(dir.path().join("file.txt"), &changed).unwrap();
    let data = repo
        .working_diff_data(&file("file.txt", ChangeKind::Unstaged))
        .unwrap();
    assert!(data.patch.contains("@@ -45,11 +45,11 @@"));
    assert!(
        data.patch.contains("\n \n"),
        "empty context retains its marker"
    );
    assert!(!data.patch.contains('\u{1b}'));
    assert_eq!(data.source.unwrap().load_context().unwrap(), changed);
}

#[test]
fn autocrlf_context_matches_the_normalized_patch() {
    let (dir, repo) = fixture();
    git(dir.path(), &["config", "core.autocrlf", "true"]);
    fs::write(dir.path().join("file.txt"), "first\r\nchanged\r\nlast\r\n").unwrap();
    let data = repo
        .working_diff_data(&file("file.txt", ChangeKind::Unstaged))
        .unwrap();
    assert!(data.patch.contains("+changed\n"));
    assert_eq!(
        data.source.unwrap().load_context().unwrap(),
        "first\nchanged\nlast\n"
    );
}

#[test]
fn staging_without_changing_file_preserves_worktree_snapshot() {
    let (dir, repo) = fixture();
    let changed = "first\nchanged\nlast\n";
    fs::write(dir.path().join("file.txt"), changed).unwrap();
    let data = repo
        .working_diff_data(&file("file.txt", ChangeKind::Unstaged))
        .unwrap();
    git(dir.path(), &["add", "file.txt"]);
    let source = data.source.unwrap();
    assert_eq!(source.load_context().unwrap(), changed);
    assert!(source.reload().unwrap().patch.is_empty());
}

#[test]
fn gitlinks_keep_short_diff_and_report_no_file_context() {
    let (dir, repo) = fixture();
    let oid = git(dir.path(), &["rev-parse", "HEAD"]);
    git(dir.path(), &["config", "diff.submodule", "log"]);
    git(
        dir.path(),
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{oid},module"),
        ],
    );
    let data = repo
        .working_diff_data(&file("module", ChangeKind::Staged))
        .unwrap();
    assert!(data.patch.contains("+Subproject commit "));
    assert!(data
        .source
        .unwrap()
        .load_context()
        .unwrap_err()
        .to_string()
        .contains("Submodule"));
    git(dir.path(), &["commit", "-m", "Add module"]);
    let commit = git(dir.path(), &["rev-parse", "HEAD"]);
    let data = repo.diff_data("", &commit, "module").unwrap();
    assert!(data
        .source
        .unwrap()
        .load_context()
        .unwrap_err()
        .to_string()
        .contains("Submodule"));
}

#[test]
fn reload_after_removal_shows_a_deleted_file() {
    let (dir, repo) = fixture();
    fs::write(dir.path().join("file.txt"), "changed\n").unwrap();
    let data = repo
        .working_diff_data(&file("file.txt", ChangeKind::Unstaged))
        .unwrap();
    fs::remove_file(dir.path().join("file.txt")).unwrap();
    let refreshed = data.source.unwrap().reload().unwrap();
    assert!(refreshed.patch.contains("+++ /dev/null"));
    assert_eq!(refreshed.source.unwrap().load_context().unwrap(), "");
}
