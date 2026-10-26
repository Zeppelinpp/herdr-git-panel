use herdr_git_panel::{
    git::Repo,
    watch::{Changes, RepoWatch},
};
use std::{
    fs,
    path::Path,
    process::Command,
    sync::mpsc,
    time::{Duration, Instant},
};

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
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-b", "main"]);
    git(dir.path(), &["config", "user.name", "Test"]);
    git(
        dir.path(),
        &["config", "user.email", "test@example.invalid"],
    );
    fs::write(dir.path().join(".gitignore"), "build/\n").unwrap();
    fs::write(dir.path().join("tracked"), "base\n").unwrap();
    fs::create_dir(dir.path().join("build")).unwrap();
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-m", "initial"]);
    let repo = Repo::discover(dir.path()).unwrap();
    (dir, repo)
}
fn next(watch: &RepoWatch, wake: &mpsc::Receiver<()>) -> Changes {
    let limit = Instant::now() + Duration::from_secs(8);
    loop {
        let now = Instant::now();
        assert!(now < limit, "native notification was not delivered");
        if let Some(changes) = watch.take_ready(now) {
            return changes;
        }
        let wait = watch.wait(now).unwrap_or(Duration::from_millis(100));
        let _ = wake.recv_timeout(wait.min(limit - now));
    }
}
#[test]
fn native_saves_stage_checkout_and_ignored_output_refresh_the_right_scope() {
    let (_dir, repo) = fixture();
    let (tx, rx) = mpsc::sync_channel(1);
    let watch = RepoWatch::start(&repo, tx).unwrap();
    fs::write(repo.path.join("tracked"), "base\nsaved\n").unwrap();
    let changes = next(&watch, &rx);
    assert!(matches!(changes, Changes::Worktree(_)));
    assert!(changes.relevant(&repo).unwrap());
    let (_, working) = repo.worktree_snapshot().unwrap();
    assert_eq!(working.files[0].stat.path, "tracked");
    git(&repo.path, &["add", "tracked"]);
    assert!(matches!(next(&watch, &rx), Changes::Worktree(_)));
    let (_, working) = repo.worktree_snapshot().unwrap();
    assert!(working
        .files
        .iter()
        .all(|f| f.kind == herdr_git_panel::git::ChangeKind::Staged));
    git(&repo.path, &["checkout", "-b", "feature"]);
    assert!(matches!(next(&watch, &rx), Changes::Repository));
    assert_eq!(repo.snapshot("main", 10).unwrap().branch, "feature");
    for n in 0..600 {
        fs::write(repo.path.join(format!("build/output-{n}")), "ignored\n").unwrap();
    }
    let changes = next(&watch, &rx);
    assert!(matches!(changes, Changes::Worktree(_)));
    assert!(
        !changes.relevant(&repo).unwrap(),
        "ignored output does not reread Git statistics"
    );
    assert!(watch.warning().is_none());
    assert!(
        watch.wait(Instant::now()).is_none(),
        "no recurring idle refresh"
    );
    // Reading status must neither rewrite the index nor feed a refresh loop.
    let _ = repo.worktree_snapshot().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        assert!(
            watch.take_ready(Instant::now()).is_none(),
            "Git reads triggered another refresh"
        );
        let _ = rx.recv_timeout(Duration::from_millis(100));
    }
}
#[test]
fn linked_worktree_observes_its_own_index_and_shared_refs() {
    let (_dir, repo) = fixture();
    let linked_parent = tempfile::tempdir().unwrap();
    let linked_path = linked_parent.path().join("linked");
    git(
        &repo.path,
        &[
            "worktree",
            "add",
            "-b",
            "linked",
            linked_path.to_str().unwrap(),
        ],
    );
    let linked = Repo::discover(&linked_path).unwrap();
    let (tx, rx) = mpsc::sync_channel(1);
    let watch = RepoWatch::start(&linked, tx).unwrap();
    fs::write(linked.path.join("tracked"), "linked\n").unwrap();
    git(&linked.path, &["add", "tracked"]);
    assert!(matches!(next(&watch, &rx), Changes::Worktree(_)));
    git(&repo.path, &["branch", "shared-new-branch"]);
    assert!(matches!(next(&watch, &rx), Changes::Repository));
    assert!(linked
        .branches()
        .unwrap()
        .iter()
        .any(|b| b == "shared-new-branch"));
}

#[test]
fn submodule_edits_and_head_only_commits_refresh_parent_worktree() {
    let (_dir, repo) = fixture();
    let (_source_dir, source) = fixture();
    git(
        &repo.path,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            source.path.to_str().unwrap(),
            "dep",
        ],
    );
    git(&repo.path, &["commit", "-am", "add submodule"]);
    let sub = repo.path.join("dep");
    git(&sub, &["config", "user.name", "Test"]);
    git(&sub, &["config", "user.email", "test@example.invalid"]);
    let (tx, rx) = mpsc::sync_channel(1);
    let watch = RepoWatch::start(&repo, tx).unwrap();
    fs::write(sub.join("tracked"), "dirty\n").unwrap();
    let changes = next(&watch, &rx);
    assert!(matches!(changes, Changes::Worktree(_)));
    assert!(
        changes.relevant(&repo).unwrap(),
        "submodule event paths rejected by check-ignore must request status"
    );
    assert!(repo.worktree_snapshot().unwrap().0.contains("dep"));
    git(&sub, &["checkout", "--", "tracked"]);
    let _ = next(&watch, &rx);
    git(&sub, &["commit", "--allow-empty", "-m", "HEAD only"]);
    assert!(
        matches!(next(&watch, &rx), Changes::Worktree(_)),
        "gitlink changes affect worktree, not parent history"
    );
    assert!(repo.worktree_snapshot().unwrap().0.contains("dep"));
}

#[test]
fn old_form_embedded_submodule_head_changes_refresh_parent() {
    let (_dir, repo) = fixture();
    let sub = repo.path.join("embedded");
    fs::create_dir(&sub).unwrap();
    git(&sub, &["init", "-b", "main"]);
    git(&sub, &["config", "user.name", "Test"]);
    git(&sub, &["config", "user.email", "test@example.invalid"]);
    fs::write(sub.join("file"), "base\n").unwrap();
    git(&sub, &["add", "."]);
    git(&sub, &["commit", "-m", "initial"]);
    git(&repo.path, &["submodule", "add", "./embedded", "embedded"]);
    git(&repo.path, &["commit", "-am", "embedded submodule"]);
    assert!(sub.join(".git").is_dir());
    let (tx, rx) = mpsc::sync_channel(1);
    let watch = RepoWatch::start(&repo, tx).unwrap();
    git(&sub, &["commit", "--allow-empty", "-m", "HEAD only"]);
    assert!(matches!(next(&watch, &rx), Changes::Worktree(_)));
    assert!(repo.worktree_snapshot().unwrap().0.contains("embedded"));
}
