use anyhow::{bail, Context, Result};
use herdr_git_panel::{git::Repo, host, ui};
use std::env;
fn main() {
    if let Err(error) = run() {
        eprintln!("Git panel: {error:#}");
        std::process::exit(1);
    }
}
fn run() -> Result<()> {
    let args: Vec<_> = env::args().skip(1).collect();
    match args.first().map(String::as_str).unwrap_or("panel") {
        "toggle" => host::toggle(),
        "auto-open" => host::auto_open(),
        "setup" => host::setup(),
        "diff" => ui::run_diff(host::take_diff()?),
        "panel" => {
            let explicit = args
                .iter()
                .position(|a| a == "--repo")
                .and_then(|i| args.get(i + 1))
                .cloned();
            let context: serde_json::Value = serde_json::from_str(
                &env::var("HERDR_PLUGIN_CONTEXT_JSON").unwrap_or_else(|_| "{}".into()),
            )?;
            let cwd = explicit
                .or_else(|| env::var("GIT_PANEL_REPO").ok())
                .or_else(|| {
                    context
                        .get("focused_pane_cwd")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                })
                .or_else(|| {
                    context
                        .get("workspace_cwd")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                })
                .unwrap_or(
                    env::current_dir()?
                        .to_str()
                        .context("Non-UTF8 working directory")?
                        .into(),
                );
            let repo = Repo::discover(cwd)?;
            host::register()?;
            ui::run(repo, host::Config::load()?)
        }
        "--help" | "-h" => {
            println!("herdr-git-panel [panel [--repo PATH] | toggle | auto-open | diff | setup]");
            Ok(())
        }
        other => bail!("Unknown command: {other}"),
    }
}
