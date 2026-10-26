use herdr_git_panel::{
    git::{matching_branches, mr_command, mr_url, parse_remote, Mr, Repo},
    host::install_binding,
};
use std::{collections::HashMap, fs, os::unix::fs::symlink, path::Path, process::Command};
use tempfile::TempDir;
fn git(path: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().into()
}
fn fixture() -> (TempDir, Repo) {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-b", "main"]);
    git(dir.path(), &["config", "user.name", "Test"]);
    git(
        dir.path(),
        &["config", "user.email", "test@example.invalid"],
    );
    fs::write(dir.path().join("base.txt"), "base\n").unwrap();
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-m", "Initial"]);
    let repo = Repo::discover(dir.path()).unwrap();
    (dir, repo)
}
#[test]
fn root_commit_special_filenames_and_merge_graph() {
    let (dir, repo) = fixture();
    let root = git(dir.path(), &["rev-parse", "HEAD"]);
    assert_eq!(repo.stats("", &root).unwrap().1[0].path, "base.txt");
    git(dir.path(), &["checkout", "-b", "feature"]);
    let odd = "odd\tfile\n[chars].txt";
    fs::write(dir.path().join(odd), "one\ntwo\n").unwrap();
    repo.commit("Feature", "Description\n\nDetails", true)
        .unwrap();
    assert_eq!(
        git(dir.path(), &["log", "-1", "--format=%B"]),
        "Feature\n\nDescription\n\nDetails"
    );
    let files = repo.stats("main", "").unwrap().1;
    assert_eq!(files[0].path, odd);
    assert_eq!(files[0].added, "2");
    assert!(repo.patch("main", "", odd).unwrap().contains("+two"));
    git(dir.path(), &["checkout", "main"]);
    fs::write(dir.path().join("main.txt"), "main\n").unwrap();
    repo.commit("Main", "", true).unwrap();
    git(dir.path(), &["checkout", "feature"]);
    git(
        dir.path(),
        &["merge", "--no-ff", "main", "-m", "Merge main"],
    );
    let merge = git(dir.path(), &["rev-parse", "HEAD"]);
    assert_eq!(repo.stats("", &merge).unwrap().1[0].path, "main.txt");
    let rows = repo.graph(100).unwrap();
    assert_eq!(rows[0].parents.len(), 2);
    assert!(rows.iter().any(|r| r.lanes.contains('/')));
    assert_eq!(rows.iter().filter(|r| !r.sha.is_empty()).count(), 4);
}
#[test]
fn staged_only_preserves_unstaged_changes_and_empty_title_does_not_stage() {
    let (dir, repo) = fixture();
    fs::write(dir.path().join("base.txt"), "changed\n").unwrap();
    assert!(repo.commit("No staged changes", "", false).is_err());
    assert!(!repo.git(&["diff"]).unwrap().is_empty());
    assert!(repo.commit("", "", true).is_err());
    assert!(repo.git(&["diff", "--cached"]).unwrap().is_empty());
    git(dir.path(), &["add", "."]);
    repo.commit("Staged", "", false).unwrap();
    assert!(repo.git(&["status", "--porcelain"]).unwrap().is_empty());
}
#[test]
fn search_reads_only_local_and_previously_fetched_refs() {
    let (dir, repo) = fixture();
    let remote = tempfile::tempdir().unwrap();
    git(remote.path(), &["init", "--bare"]);
    git(
        dir.path(),
        &["remote", "add", "origin", remote.path().to_str().unwrap()],
    );
    git(
        dir.path(),
        &["push", "origin", "HEAD:refs/heads/not-fetched"],
    );
    git(
        dir.path(),
        &["update-ref", "-d", "refs/remotes/origin/not-fetched"],
    );
    git(
        dir.path(),
        &["update-ref", "refs/remotes/origin/release-ready", "HEAD"],
    );
    git(dir.path(), &["branch", "release/local"]);
    let refs = repo.branches().unwrap();
    assert!(!refs.iter().any(|b| b.contains("not-fetched")));
    assert_eq!(matching_branches(&refs, "RELEASE").len(), 2);
    assert_eq!(
        matching_branches(&refs, "origin ready"),
        vec!["origin/release-ready"]
    );
    assert!(matching_branches(&refs, "unknown").is_empty());
    assert!(repo.resolve("--upload-pack=bad").is_err());
    assert!(!repo
        .git(&["show-ref"])
        .unwrap()
        .contains("origin/not-fetched"));
}
#[test]
fn push_resolves_remote_ref_and_local_upstream_to_exact_destination() {
    let (dir, repo) = fixture();
    let remote = tempfile::tempdir().unwrap();
    git(remote.path(), &["init", "--bare"]);
    git(
        dir.path(),
        &["remote", "add", "origin", remote.path().to_str().unwrap()],
    );
    repo.push("origin", "release").unwrap();
    assert_eq!(
        repo.push_target("origin/release").unwrap(),
        ("origin".into(), "release".into())
    );
    git(
        dir.path(),
        &["branch", "--set-upstream-to=origin/release", "main"],
    );
    assert_eq!(
        repo.push_target("main").unwrap(),
        ("origin".into(), "release".into())
    );
    fs::write(dir.path().join("more.txt"), "more\n").unwrap();
    repo.commit("More", "", true).unwrap();
    repo.push("origin", "release").unwrap();
    assert_eq!(
        git(remote.path(), &["rev-parse", "refs/heads/release"]),
        git(dir.path(), &["rev-parse", "HEAD"])
    );
}
#[test]
fn mr_arguments_preserve_user_text_and_support_self_hosted_forges() {
    let remote = parse_remote("origin", "git@code.example.com:team/sub/project.git").unwrap();
    assert_eq!(remote.project, "team/sub/project");
    let mr = Mr {
        remote: "origin".into(),
        source: "feature".into(),
        target: "main".into(),
        title: "Title $(touch bad)".into(),
        description: "Body\nnext".into(),
    };
    let hosts = HashMap::from([("code.example.com".into(), "gitlab".into())]);
    let (program, args) = mr_command(&remote, &mr, &hosts).unwrap();
    assert_eq!(program, "glab");
    assert_eq!(
        args[args.iter().position(|s| s == "--title").unwrap() + 1],
        mr.title
    );
    assert_eq!(
        args[args.iter().position(|s| s == "--source-branch").unwrap() + 1],
        "feature"
    );
    let github = parse_remote("origin", "https://github.com/org/project.git").unwrap();
    assert_eq!(mr_command(&github, &mr, &HashMap::new()).unwrap().0, "gh");
    assert_eq!(
        mr_url("Created https://gitlab.com/a/b/-/merge_requests/42\n").as_deref(),
        Some("https://gitlab.com/a/b/-/merge_requests/42")
    );
    assert!(mr_url("no URL").is_none());
}
#[test]
fn setup_keeps_symlink_comments_and_existing_settings() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("dotfiles.toml");
    let link = dir.path().join("config.toml");
    let original = "# my settings\n[ui]\nmouse_capture = true\n";
    fs::write(&target, original).unwrap();
    symlink(&target, &link).unwrap();
    assert!(install_binding(&link).unwrap());
    assert!(!install_binding(&link).unwrap());
    assert!(link.is_symlink());
    let text = fs::read_to_string(&target).unwrap();
    assert!(text.starts_with(original.trim_end()));
    assert_eq!(text.matches("kingdee.git-panel.toggle").count(), 1);
    assert!(fs::read_dir(dir.path()).unwrap().any(|p| p
        .unwrap()
        .path()
        .extension()
        .is_some_and(|e| e == "bak")));
}
#[test]
fn conflicting_shortcut_is_not_modified() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let original="[[keys.command]]\nkey = \"cmd+shift+r\"\ncommand = \"other.toggle\"\ntype = \"plugin_action\"\n";
    fs::write(&path, original).unwrap();
    assert!(install_binding(&path).is_err());
    assert_eq!(fs::read_to_string(path).unwrap(), original);
}

#[test]
fn worktree_stats_include_unstaged_staged_and_untracked_without_mutation() {
    use herdr_git_panel::git::ChangeKind;
    let (dir, repo) = fixture();
    fs::write(dir.path().join("base.txt"), "changed\nsecond line\n").unwrap();
    fs::write(dir.path().join("staged.txt"), "staged content\n").unwrap();
    git(dir.path(), &["add", "staged.txt"]);
    let untracked = "new\tfile\n[chars].txt";
    fs::write(dir.path().join(untracked), "new untracked content\n").unwrap();
    let before = git(dir.path(), &["status", "--porcelain"]);
    let snapshot = repo.snapshot("main", 150).unwrap();
    assert!(snapshot.files.is_empty(), "committed comparison is clean");
    assert_eq!(snapshot.working.files.len(), 3);
    let find = |kind| {
        snapshot
            .working
            .files
            .iter()
            .find(|f| f.kind == kind)
            .unwrap()
    };
    assert_eq!(find(ChangeKind::Unstaged).stat.path, "base.txt");
    assert_eq!(find(ChangeKind::Unstaged).stat.added, "2");
    assert_eq!(find(ChangeKind::Staged).stat.path, "staged.txt");
    assert_eq!(find(ChangeKind::Untracked).stat.path, untracked);
    assert!(repo
        .working_patch(find(ChangeKind::Unstaged))
        .unwrap()
        .contains("+changed"));
    assert!(repo
        .working_patch(find(ChangeKind::Staged))
        .unwrap()
        .contains("+staged content"));
    assert!(repo
        .working_patch(find(ChangeKind::Untracked))
        .unwrap()
        .contains("+new untracked content"));
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), before);
}

#[test]
fn staged_and_unstaged_versions_of_one_file_have_distinct_patches() {
    use herdr_git_panel::git::ChangeKind;
    let (dir, repo) = fixture();
    fs::write(dir.path().join("base.txt"), "staged version\n").unwrap();
    git(dir.path(), &["add", "base.txt"]);
    fs::write(dir.path().join("base.txt"), "unstaged version\n").unwrap();
    let working = repo.working_stats().unwrap();
    assert_eq!(working.files.len(), 2);
    let staged = working
        .files
        .iter()
        .find(|f| f.kind == ChangeKind::Staged)
        .unwrap();
    let unstaged = working
        .files
        .iter()
        .find(|f| f.kind == ChangeKind::Unstaged)
        .unwrap();
    assert!(repo
        .working_patch(staged)
        .unwrap()
        .contains("+staged version"));
    assert!(!repo
        .working_patch(staged)
        .unwrap()
        .contains("+unstaged version"));
    assert!(repo
        .working_patch(unstaged)
        .unwrap()
        .contains("+unstaged version"));
}
