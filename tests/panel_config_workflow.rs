use std::{fs, os::unix::fs::PermissionsExt, process::Command};

#[test]
fn toggle_reads_direction_and_rejects_unsupported_directions_before_opening() {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    fs::create_dir(&repo).unwrap();
    assert!(Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(&repo)
        .status()
        .unwrap()
        .success());

    // Exercise the real toggle entrypoint without changing a running Herdr session.
    let herdr = root.path().join("herdr");
    fs::write(
        &herdr,
        r#"#!/bin/sh
case "$1 $2" in
  'pane get') printf '{"result":{"pane":{"tab_id":"w1:t1","workspace_id":"w1","cwd":"%s"}}}\n' "$TEST_REPO" ;;
  'pane list') printf '{"result":{"panes":[]}}\n' ;;
  'plugin pane') printf '%s\n' "$@" > "$TEST_OPEN_ARGS"; printf '{"result":{}}\n' ;;
  *) exit 1 ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(&herdr, fs::Permissions::from_mode(0o755)).unwrap();
    let config = root.path().join("config.json");
    let open_args = root.path().join("open-args");
    for (settings, expected) in [
        ("{}", Some("right")),
        (r#"{"direction":"down"}"#, Some("down")),
        (r#"{"direction":"right"}"#, Some("right")),
        (r#"{"direction":"left"}"#, None),
        (r#"{"direction":"up"}"#, None),
        (r#"{"direction":"invalid"}"#, None),
    ] {
        fs::write(&config, settings).unwrap();
        let _ = fs::remove_file(&open_args);
        let output = Command::new(env!("CARGO_BIN_EXE_herdr-git-panel"))
            .arg("toggle")
            .env("HERDR_ENV", "1")
            .env("HERDR_BIN_PATH", &herdr)
            .env("HERDR_PLUGIN_CONFIG_DIR", root.path())
            .env("HERDR_PLUGIN_STATE_DIR", root.path().join("state"))
            .env(
                "HERDR_PLUGIN_CONTEXT_JSON",
                r#"{"focused_pane_id":"w1:p1"}"#,
            )
            .env("HERDR_SOCKET_PATH", root.path().join("unused.sock"))
            .env("TEST_REPO", &repo)
            .env("TEST_OPEN_ARGS", &open_args)
            .output()
            .unwrap();
        if let Some(direction) = expected {
            assert!(output.status.success(), "{:?}", output);
            let arguments = fs::read_to_string(&open_args).unwrap();
            let arguments: Vec<_> = arguments.lines().collect();
            assert!(arguments
                .windows(2)
                .any(|pair| pair == ["--direction", direction]));
        } else {
            assert!(!output.status.success());
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("direction must be right or down")
            );
            assert!(!open_args.exists(), "invalid config opened a pane");
        }
    }
}
