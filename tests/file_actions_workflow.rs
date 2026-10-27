use herdr_git_panel::git::{ChangeKind, Repo};
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
    String::from_utf8(output.stdout).unwrap()
}

fn fixture(committed: bool) -> (tempfile::TempDir, Repo) {
    let root = tempfile::tempdir().unwrap();
    git(root.path(), &["init", "-b", "main"]);
    git(root.path(), &["config", "user.name", "File Test"]);
    git(
        root.path(),
        &["config", "user.email", "file@example.invalid"],
    );
    if committed {
        fs::write(root.path().join("file.txt"), "base\n").unwrap();
        fs::write(root.path().join("other.txt"), "other\n").unwrap();
        git(root.path(), &["add", "."]);
        git(root.path(), &["commit", "-m", "Base"]);
    }
    let repo = Repo::discover(root.path()).unwrap();
    (root, repo)
}

#[test]
fn staging_and_unstaging_a_mixed_file_preserves_the_worktree_and_other_paths() {
    let (root, repo) = fixture(true);
    fs::write(root.path().join("file.txt"), "staged\n").unwrap();
    repo.toggle_stage(ChangeKind::Unstaged, "file.txt").unwrap();
    fs::write(root.path().join("file.txt"), "working\n").unwrap();
    fs::write(root.path().join("other.txt"), "other edit\n").unwrap();
    repo.toggle_stage(ChangeKind::Staged, "file.txt").unwrap();
    assert_eq!(
        fs::read_to_string(root.path().join("file.txt")).unwrap(),
        "working\n"
    );
    assert!(git(root.path(), &["diff", "--cached"]).is_empty());
    repo.toggle_stage(ChangeKind::Unstaged, "file.txt").unwrap();
    assert_eq!(git(root.path(), &["show", ":file.txt"]), "working\n");
    assert_eq!(git(root.path(), &["show", ":other.txt"]), "other\n");
    assert_eq!(
        fs::read_to_string(root.path().join("other.txt")).unwrap(),
        "other edit\n"
    );
}

#[test]
fn staging_tracks_new_and_deleted_files_and_unborn_unstage_keeps_contents() {
    let (root, repo) = fixture(true);
    fs::remove_file(root.path().join("file.txt")).unwrap();
    repo.toggle_stage(ChangeKind::Unstaged, "file.txt").unwrap();
    assert!(git(root.path(), &["diff", "--cached", "--name-status"]).starts_with("D\tfile.txt"));
    repo.toggle_stage(ChangeKind::Staged, "file.txt").unwrap();
    assert!(!root.path().join("file.txt").exists());
    assert!(git(root.path(), &["diff", "--cached"]).is_empty());
    fs::write(root.path().join("new.txt"), "new\n").unwrap();
    repo.toggle_stage(ChangeKind::Untracked, "new.txt").unwrap();
    assert_eq!(git(root.path(), &["show", ":new.txt"]), "new\n");
    repo.toggle_stage(ChangeKind::Staged, "new.txt").unwrap();
    assert_eq!(
        fs::read_to_string(root.path().join("new.txt")).unwrap(),
        "new\n"
    );

    let (unborn_root, unborn) = fixture(false);
    fs::write(unborn_root.path().join("first"), "initial\n").unwrap();
    unborn.toggle_stage(ChangeKind::Untracked, "first").unwrap();
    fs::write(unborn_root.path().join("first"), "uncommitted update\n").unwrap();
    unborn.toggle_stage(ChangeKind::Staged, "first").unwrap();
    assert!(git(unborn_root.path(), &["ls-files"]).is_empty());
    assert_eq!(
        fs::read_to_string(unborn_root.path().join("first")).unwrap(),
        "uncommitted update\n"
    );
}

#[test]
fn discard_restores_both_staged_and_unstaged_changes_without_touching_other_files() {
    let (root, repo) = fixture(true);
    fs::write(root.path().join("file.txt"), "staged\n").unwrap();
    git(root.path(), &["add", "file.txt"]);
    fs::write(root.path().join("file.txt"), "working\n").unwrap();
    fs::write(root.path().join("other.txt"), "keep me\n").unwrap();
    let plan = repo.prepare_discard("file.txt").unwrap();
    assert!(!plan.deletes_file);
    git(root.path(), &["add", "other.txt"]);
    repo.discard(&plan).unwrap();
    assert_eq!(
        fs::read_to_string(root.path().join("file.txt")).unwrap(),
        "base\n"
    );
    assert!(git(root.path(), &["diff", "--cached", "--", "file.txt"]).is_empty());
    assert_eq!(git(root.path(), &["show", ":other.txt"]), "keep me\n");
    assert_eq!(
        fs::read_to_string(root.path().join("other.txt")).unwrap(),
        "keep me\n"
    );
    fs::remove_file(root.path().join("file.txt")).unwrap();
    repo.discard(&repo.prepare_discard("file.txt").unwrap())
        .unwrap();
    assert_eq!(
        fs::read_to_string(root.path().join("file.txt")).unwrap(),
        "base\n"
    );
}

#[test]
fn discard_removes_untracked_and_staged_new_files_including_an_unborn_repository() {
    for committed in [true, false] {
        let (root, repo) = fixture(committed);
        for staged in [true, false] {
            fs::write(root.path().join("new"), "new\n").unwrap();
            if staged {
                git(root.path(), &["add", "new"]);
            }
            let plan = repo.prepare_discard("new").unwrap();
            assert!(plan.deletes_file);
            repo.discard(&plan).unwrap();
            assert!(!root.path().join("new").exists());
            assert!(git(root.path(), &["ls-files", "--", "new"]).is_empty());
        }
    }
}

#[test]
fn actions_use_literal_special_paths_and_unlink_symlinks_without_following_targets() {
    let (root, repo) = fixture(true);
    for name in ["-dash", "[glob]*?.txt", "line\nname\t.txt"] {
        fs::write(root.path().join(name), "new\n").unwrap();
        repo.toggle_stage(ChangeKind::Untracked, name).unwrap();
        assert_eq!(git(root.path(), &["show", &format!(":{name}")]), "new\n");
        repo.discard(&repo.prepare_discard(name).unwrap()).unwrap();
        assert!(!root.path().join(name).exists());
    }
    let target = tempfile::NamedTempFile::new().unwrap();
    fs::write(target.path(), "target must survive\n").unwrap();
    symlink(target.path(), root.path().join("link")).unwrap();
    repo.discard(&repo.prepare_discard("link").unwrap())
        .unwrap();
    assert!(root.path().join("link").symlink_metadata().is_err());
    assert_eq!(
        fs::read_to_string(target.path()).unwrap(),
        "target must survive\n"
    );
    symlink(target.path(), root.path().join("link")).unwrap();
    git(root.path(), &["add", "link"]);
    git(root.path(), &["commit", "-m", "Link"]);
    fs::remove_file(root.path().join("link")).unwrap();
    let other = tempfile::NamedTempFile::new().unwrap();
    symlink(other.path(), root.path().join("link")).unwrap();
    repo.discard(&repo.prepare_discard("link").unwrap())
        .unwrap();
    assert_eq!(
        fs::read_link(root.path().join("link")).unwrap(),
        target.path()
    );
    assert_eq!(
        fs::read_to_string(target.path()).unwrap(),
        "target must survive\n"
    );
}

#[test]
fn discard_restores_a_nested_tracked_path_instead_of_deleting_it() {
    let (root, repo) = fixture(true);
    fs::create_dir(root.path().join("nested")).unwrap();
    fs::write(root.path().join("nested/file"), "nested base\n").unwrap();
    git(root.path(), &["add", "nested/file"]);
    git(root.path(), &["commit", "-m", "Nested"]);
    fs::write(root.path().join("nested/file"), "nested edit\n").unwrap();
    let plan = repo.prepare_discard("nested/file").unwrap();
    assert!(!plan.deletes_file);
    repo.discard(&plan).unwrap();
    assert_eq!(
        fs::read_to_string(root.path().join("nested/file")).unwrap(),
        "nested base\n"
    );
}

#[test]
fn discard_rejects_edits_index_changes_and_head_changes_after_confirmation() {
    let (root, repo) = fixture(true);
    fs::write(root.path().join("file.txt"), "first\n").unwrap();
    let plan = repo.prepare_discard("file.txt").unwrap();
    fs::write(root.path().join("file.txt"), "later\n").unwrap();
    assert!(repo
        .discard(&plan)
        .unwrap_err()
        .to_string()
        .contains("changed since confirmation"));
    assert_eq!(
        fs::read_to_string(root.path().join("file.txt")).unwrap(),
        "later\n"
    );
    let plan = repo.prepare_discard("file.txt").unwrap();
    git(root.path(), &["add", "file.txt"]);
    assert!(repo.discard(&plan).is_err());
    let plan = repo.prepare_discard("file.txt").unwrap();
    git(
        root.path(),
        &["commit", "--allow-empty", "-m", "Changed HEAD"],
    );
    assert!(repo.discard(&plan).is_err());
}

#[test]
fn file_actions_reject_directories_symlink_parents_and_unchanged_paths() {
    let (root, repo) = fixture(true);
    assert!(repo.prepare_discard("file.txt").is_err());
    assert!(repo
        .toggle_stage(ChangeKind::Unstaged, "../outside")
        .is_err());
    fs::create_dir(root.path().join("directory")).unwrap();
    fs::write(root.path().join("directory/file"), "nested\n").unwrap();
    assert!(repo.prepare_discard("directory").is_err());
    assert!(repo
        .toggle_stage(ChangeKind::Untracked, "directory")
        .is_err());
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("file"), "outside\n").unwrap();
    symlink(outside.path(), root.path().join("parent")).unwrap();
    assert!(repo.prepare_discard("parent/file").is_err());
    assert_eq!(
        fs::read_to_string(outside.path().join("file")).unwrap(),
        "outside\n"
    );
    let head = git(root.path(), &["rev-parse", "HEAD"]);
    git(
        root.path(),
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            "160000",
            head.trim(),
            "submodule",
        ],
    );
    assert!(repo
        .prepare_discard("submodule")
        .unwrap_err()
        .to_string()
        .contains("submodule"));
}
