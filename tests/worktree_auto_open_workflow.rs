use serde_json::{json, Value};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Output},
};

struct Fixture {
    _root: tempfile::TempDir,
    repo: PathBuf,
    herdr: PathBuf,
    open_args: PathBuf,
    config_dir: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        fs::create_dir(&repo).unwrap();
        assert!(Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&repo)
            .status()
            .unwrap()
            .success());

        let herdr = root.path().join("herdr");
        fs::write(
            &herdr,
            r#"#!/bin/sh
case "$1 $2" in
  'pane list') printf '{"result":{"panes":[{"pane_id":"w9:p1"}]}}\n' ;;
  'pane process-info')
    if [ "$TEST_EXISTING" = 1 ]; then
      printf '{"result":{"process_info":{"foreground_processes":[{"argv":["/tmp/herdr-git-panel","panel"]}]}}}\n'
    else
      printf '{"result":{"process_info":{"foreground_processes":[]}}}\n'
    fi
    ;;
  'plugin pane') printf '%s\n' "$@" > "$TEST_OPEN_ARGS"; printf '{"result":{}}\n' ;;
  *) exit 1 ;;
esac
"#,
        )
        .unwrap();
        fs::set_permissions(&herdr, fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            open_args: root.path().join("open-args"),
            config_dir: root.path().to_path_buf(),
            repo,
            herdr,
            _root: root,
        }
    }

    fn event(&self) -> Value {
        json!({
            "event": "worktree_opened",
            "data": {
                "workspace": {
                    "workspace_id": "w9",
                    "worktree": {"checkout_path": self.repo}
                },
                "already_open": false
            }
        })
    }

    fn run(&self, event: &Value, existing: bool) -> Output {
        Command::new(env!("CARGO_BIN_EXE_herdr-git-panel"))
            .arg("auto-open")
            .env("HERDR_ENV", "1")
            .env("HERDR_BIN_PATH", &self.herdr)
            .env("HERDR_PLUGIN_CONFIG_DIR", &self.config_dir)
            .env("HERDR_PLUGIN_EVENT_JSON", event.to_string())
            .env("TEST_EXISTING", if existing { "1" } else { "0" })
            .env("TEST_OPEN_ARGS", &self.open_args)
            .output()
            .unwrap()
    }
}

#[test]
fn worktree_birth_opens_panel_in_event_workspace_without_taking_focus() {
    let fixture = Fixture::new();
    let output = fixture.run(&fixture.event(), false);
    assert!(output.status.success(), "{output:?}");

    let arguments = fs::read_to_string(&fixture.open_args).unwrap();
    let arguments: Vec<_> = arguments.lines().collect();
    assert!(arguments.contains(&"--no-focus"));
    assert!(arguments
        .windows(2)
        .any(|pair| pair == ["--target-pane", "w9:p1"]));
    let repo = fixture.repo.canonicalize().unwrap();
    assert!(arguments
        .windows(2)
        .any(|pair| pair == ["--cwd", repo.to_str().unwrap()]));
    assert!(arguments
        .windows(2)
        .any(|pair| pair == ["--env", "GIT_PANEL_SOURCE_PANE=w9:p1"]));
}

#[test]
fn opening_live_workspace_does_not_resurrect_closed_panel() {
    let fixture = Fixture::new();
    let mut event = fixture.event();
    event["data"]["already_open"] = json!(true);

    let output = fixture.run(&event, false);
    assert!(output.status.success(), "{output:?}");
    assert!(!fixture.open_args.exists());
}

#[test]
fn worktree_birth_does_not_duplicate_existing_panel() {
    let fixture = Fixture::new();
    let output = fixture.run(&fixture.event(), true);
    assert!(output.status.success(), "{output:?}");
    assert!(!fixture.open_args.exists());
}
