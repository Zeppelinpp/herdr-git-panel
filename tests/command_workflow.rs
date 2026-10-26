use herdr_git_panel::git::command;
use std::time::{Duration, Instant};

#[test]
fn timeout_stops_descendants_holding_pipes_even_after_parent_exits() {
    for script in ["sleep 4 & wait", "sleep 4 & exit 0"] {
        let started = Instant::now();
        let error = command(
            "sh",
            &["-c".into(), script.into()],
            std::env::temp_dir().as_path(),
            1,
        )
        .unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "pipe holders exceeded deadline: {script}"
        );
    }
}

#[test]
fn successful_commands_drain_both_pipes_and_preserve_nonzero_errors() {
    let path = std::env::temp_dir();
    let output = command(
        "sh",
        &["-c".into(), "printf result; printf diagnostic >&2".into()],
        &path,
        3,
    )
    .unwrap();
    assert_eq!(output, "result");
    let error = command(
        "sh",
        &["-c".into(), "printf rejected >&2; exit 7".into()],
        &path,
        3,
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "rejected");
}
