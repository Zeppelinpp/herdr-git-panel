//! Render a deterministic, local-only merge fixture for visual QA.
use anyhow::{Context, Result};
use herdr_git_panel::{
    git::Repo,
    graph::{self, Expanded, Item},
    theme::{Theme, NAVIGATION},
};
use ratatui::{
    backend::TestBackend,
    layout::{Constraint, Rect},
    style::{Color, Modifier, Style},
    widgets::{Paragraph, Row, Table, TableState},
    Terminal,
};
use std::{env, fs, path::Path, process::Command};
fn git(path: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()?;
    anyhow::ensure!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(String::from_utf8_lossy(&out.stdout).trim().into())
}
fn color(c: Color) -> String {
    match c {
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        Color::Reset => "#161616".into(),
        _ => "#dddddd".into(),
    }
}
fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
fn main() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path();
    git(path, &["init", "-b", "main"])?;
    git(path, &["config", "user.name", "Preview"])?;
    git(path, &["config", "user.email", "preview@example.invalid"])?;
    fs::write(path.join("base.txt"), "base\n")?;
    git(path, &["add", "."])?;
    git(path, &["commit", "-m", "Initial commit"])?;
    let branches = [
        ("task-sit-work", "feat(bootstrap): ERP 用户信息刷新"),
        ("dev-to-sit-zhihui", "feat(skill): 支持多轮任务与上下文"),
        ("feature-sit", "feat(event): manage_ling 用户输入与个人助理"),
        ("hotfix-lgmm", "fix: 定时任务与对话状态修复"),
    ];
    for (branch, message) in branches {
        git(path, &["checkout", "-b", branch, "main"])?;
        fs::write(path.join(format!("{branch}.rs")), format!("// {message}\n"))?;
        git(path, &["add", "."])?;
        git(path, &["commit", "-m", message])?;
    }
    for (dest, source) in [
        ("dev-to-sit-zhihui", "task-sit-work"),
        ("feature-sit", "dev-to-sit-zhihui"),
        ("hotfix-lgmm", "feature-sit"),
        ("main", "hotfix-lgmm"),
    ] {
        git(path, &["checkout", dest])?;
        git(
            path,
            &[
                "merge",
                "--no-ff",
                source,
                "-m",
                &format!("Merge branch '{source}' into '{dest}'"),
            ],
        )?;
    }
    for (branch, _) in branches {
        git(
            path,
            &[
                "update-ref",
                &format!("refs/remotes/origin/{branch}"),
                branch,
            ],
        )?;
    }
    let repo = Repo::discover(path)?;
    let commits = repo.graph(50)?;
    let theme = Theme::load()?;
    let args = env::args().skip(1).collect::<Vec<_>>();
    let output = args
        .first()
        .context("Usage: cargo run --example graph_preview -- OUTPUT.svg [--expanded]")?;
    let (summary, files) = repo.stats("", &commits[0].sha)?;
    let expanded = args.iter().any(|a| a == "--expanded").then_some(Expanded {
        sha: &commits[0].sha,
        summary: &summary,
        files: &files,
    });
    let rows = graph::project(&commits, expanded, 78);
    let height = (rows.len() + 3) as u16;
    let mut terminal = Terminal::new(TestBackend::new(78, height))?;
    let mut state = TableState::default().with_selected(
        rows.iter()
            .position(|r| r.item == Item::Commit(2))
            .unwrap_or(0),
    );
    terminal.draw(|frame| {
        frame.render_widget(
            Paragraph::new(format!(
                "▾ COMMITS {} +",
                commits.iter().filter(|r| !r.sha.is_empty()).count()
            ))
            .style(Style::new().fg(Color::Gray).add_modifier(Modifier::BOLD)),
            Rect::new(0, 0, 78, 1),
        );
        let table = Table::new(
            rows.iter().map(|r| Row::new(vec![r.line.clone()])),
            [Constraint::Percentage(100)],
        )
        .row_highlight_style(NAVIGATION);
        frame.render_stateful_widget(table, Rect::new(0, 2, 78, height - 2), &mut state);
        theme.apply(frame.buffer_mut());
    })?;
    let buffer = terminal.backend().buffer();
    let width = 804;
    let h = usize::from(height) * 23 + 24;
    let mut svg=format!("<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width}\" height=\"{h}\" viewBox=\"0 0 {width} {h}\"><rect width=\"100%\" height=\"100%\" fill=\"{}\"/>",color(theme.background));
    for y in 0..height {
        for x in 0..78 {
            let c = &buffer[(x, y)];
            svg += &format!(
                "<rect x=\"{}\" y=\"{}\" width=\"10\" height=\"23\" fill=\"{}\"/>",
                12 + x * 10,
                12 + y * 23,
                color(if c.bg == Color::Reset {
                    theme.background
                } else {
                    c.bg
                })
            );
        }
    }
    svg += "<g font-family=\"Menlo, monospace\" font-size=\"16\">";
    for y in 0..height {
        for x in 0..78 {
            let c = &buffer[(x, y)];
            if c.symbol().trim().is_empty() {
                continue;
            }
            svg += &format!(
                "<text x=\"{}\" y=\"{}\" fill=\"{}\"{}>{}</text>",
                12 + x * 10,
                29 + y * 23,
                color(if c.fg == Color::Reset {
                    theme.text
                } else {
                    c.fg
                }),
                if c.modifier.contains(Modifier::BOLD) {
                    " font-weight=\"bold\""
                } else {
                    ""
                },
                xml(c.symbol())
            );
        }
    }
    svg += "</g></svg>";
    fs::write(output, svg)?;
    println!(
        "Rendered {} commits to {output}",
        commits.iter().filter(|r| !r.sha.is_empty()).count()
    );
    Ok(())
}
