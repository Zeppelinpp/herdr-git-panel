use herdr_git_panel::git::Repo;
use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command};

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
    String::from_utf8_lossy(&output.stdout).trim().into()
}
#[test]
fn summary_and_numstat_use_the_same_tips_if_head_moves_between_queries() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("repo");
    fs::create_dir(&path).unwrap();
    git(&path, &["init", "-b", "main"]);
    git(&path, &["config", "user.name", "Test"]);
    git(&path, &["config", "user.email", "test@example.invalid"]);
    fs::write(path.join("base"), "base\n").unwrap();
    git(&path, &["add", "."]);
    git(&path, &["commit", "-m", "initial"]);
    git(&path, &["branch", "base"]);
    for name in ["one", "two"] {
        fs::write(path.join(name), "line\n").unwrap();
        git(&path, &["add", "."]);
        git(&path, &["commit", "-m", name]);
    }
    let next = git(&path, &["rev-parse", "HEAD"]);
    git(&path, &["reset", "--hard", "HEAD~1"]);
    let real_git = Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .unwrap();
    let real_git = String::from_utf8(real_git.stdout).unwrap();
    let shim = dir.path().join("shim");
    fs::create_dir(&shim).unwrap();
    let wrapper = shim.join("git");
    fs::write(&wrapper, r#"#!/bin/sh
case " $* " in
  *" --stat "*) "$HERDR_TEST_REAL_GIT" "$@"; code=$?; "$HERDR_TEST_REAL_GIT" update-ref HEAD "$HERDR_TEST_NEXT_TIP"; exit "$code" ;;
  *) exec "$HERDR_TEST_REAL_GIT" "$@" ;;
esac
"#).unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
    // Isolate PATH in a subprocess: parallel tests must never see the shim.
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "comparison_probe", "--nocapture"])
        .env(
            "PATH",
            format!("{}:{}", shim.display(), std::env::var("PATH").unwrap()),
        )
        .env("HERDR_TEST_REAL_GIT", real_git.trim())
        .env("HERDR_TEST_COMPARISON_REPO", &path)
        .env("HERDR_TEST_NEXT_TIP", next)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
#[test]
fn comparison_probe() {
    let Some(path) = std::env::var_os("HERDR_TEST_COMPARISON_REPO") else {
        return;
    };
    let repo = Repo::discover(path).unwrap();
    let (summary, files) = repo.stats("base", "").unwrap();
    assert!(summary.contains("1 file changed"), "{summary}");
    assert_eq!(
        files.len(),
        1,
        "numstat must use HEAD captured before --stat"
    );
    assert_eq!(files[0].path, "one");
    assert_eq!(
        repo.git(&["rev-parse", "HEAD"]).unwrap().trim(),
        std::env::var("HERDR_TEST_NEXT_TIP").unwrap()
    );
}
