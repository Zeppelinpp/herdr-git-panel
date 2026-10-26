# Herdr Git Panel

A **Rust** Git sidebar for Herdr, built with Ratatui and Crossterm. Search local and previously fetched branches, commit with separate title/description, create GitHub PRs or GitLab MRs, inspect a continuous commit graph, open native diff popups, and push to the selected comparison branch.

The interface inherits **Herdr's configured theme**: the named built-in palette, `[theme.custom]` overrides and `[ui].accent`. Backgrounds, text, borders, selection, branch labels, graph lanes and additions/deletions use those host tokens. It does not install its own theme or change terminal fonts. Selecting Herdr's `terminal` theme preserves terminal defaults instead. Reopen the panel or press Ctrl+R after changing Herdr's config. See [palette resolution](assets/README.md) for the auto-switch and remote-client limitations of the current Herdr API.

Navigation rows use a soft fill derived from the Herdr panel background and accent, making j/k selection easier to track while retaining normal text and rail colors. An explicitly configured Herdr `theme.custom.selection_bg` (including active mode overrides) takes precedence. Text selection keeps the inherited selection color; a focused input's insertion cursor uses the host accent with contrasting text, independently of the darker selection fill. Unfocused inputs do not draw a cursor block.

## Install from GitHub

```sh
herdr plugin install Zeppelinpp/herdr-git-panel --ref v0.4.2
```

Herdr runs the manifest build and registers the plugin. This source installation requires Git and Rust ≥1.88; add the shortcut shown below and reload Herdr config. Omit `--ref` to track the default branch instead of a fixed release. Fixed-tag installations stay pinned when updated.

Prebuilt archives are available from [GitHub Releases](https://github.com/Zeppelinpp/herdr-git-panel/releases/latest), together with `SHA256SUMS`. They include the executable and plugin manifest and do not require Rust:

| Platform | Archive target |
| --- | --- |
| macOS Apple Silicon | `aarch64-apple-darwin` |
| macOS Intel | `x86_64-apple-darwin` |
| Linux ARM64 | `aarch64-unknown-linux-gnu` |
| Linux x86_64 | `x86_64-unknown-linux-gnu` |

Linux archives are built on Ubuntu 22.04 and require glibc ≥2.35. For other Linux environments, build from source. For example, after downloading and verifying the Apple Silicon archive:

```sh
grep '  herdr-git-panel-v0.4.2-aarch64-apple-darwin.tar.gz$' SHA256SUMS | shasum -a 256 -c -
tar -xzf herdr-git-panel-v0.4.2-aarch64-apple-darwin.tar.gz
cd herdr-git-panel-v0.4.2-aarch64-apple-darwin
./bin/herdr-git-panel setup
```

The prebuilt setup links the extracted directory and adds the keybinding. Keep that directory in a durable location. `plugin link` does not execute manifest build commands. For GitHub-managed installations, keep the registration created by `plugin install`; the setup command is intended for local/prebuilt directories.

## Build locally

Requires a current stable Rust toolchain (`cargo`), Git, and Herdr ≥0.9.5 on macOS or Linux. Python and uv are not required. MR creation additionally requires authenticated `gh` or `glab`.

```sh
cd ~/projects/herdr-git-panel
sh scripts/build.sh
./bin/herdr-git-panel setup
```

Setup links the checkout, adds **Cmd+Shift+R**, and reloads Herdr config. It preserves dotfiles symlinks and existing comments/settings, backs up the config before changes, and refuses to replace a conflicting shortcut. No Herdr restart is needed. Close an already-running panel and reopen it after rebuilding.

For manual setup:

```sh
herdr plugin link "$PWD"
```

Add to Herdr's `config.toml`, then run `herdr server reload-config`:

```toml
[[keys.command]]
key = "cmd+shift+r"
type = "plugin_action"
command = "kingdee.git-panel.toggle"
description = "Toggle Git panel"
```

If the outer terminal reserves Cmd+Shift+R, configure it to forward the shortcut or pick another binding. Linux users can choose a chord their terminal transmits.

## Interaction

- **Compare:** click the comparison branch or focus it with Tab and press Enter. Type any part of the branch name to filter immediately, case-insensitively. Multiple space-separated terms must all match. Use ↑/↓ and Enter or click a result. Esc cancels without changing the comparison. Only `refs/heads` and existing `refs/remotes` are listed; opening, searching and refreshing never fetch or run `ls-remote`.
- **Commit:** edit title and multiline description. Click Commit, Tab to it and press Enter, or use Ctrl+S. “Stage all changes” defaults on and includes new, modified and deleted files; turn it off to use the current index. Hooks and signing remain active. Failed commits preserve the draft and any staging already performed.
- **MR / PR:** click Create MR and edit remote, source, target, title and description. Tab/Shift+Tab moves between fields and Create/Cancel; Enter in the description inserts a newline. The source must already be pushed. This operation does not push. A successful result exposes a clickable `!MR number` browser link, also used for GitHub PRs. Cross-project/fork MRs are outside this form.
- **Working tree:** lists changes that have not been committed, independently of the comparison branch. `[U]` means unstaged (`git diff`), `[S]` means staged (`git diff --cached`), and `[?]` means untracked (excluding ignored files). A file can appear twice when its index and working-tree versions both differ; each entry opens the corresponding patch. The heading totals tracked insertions/deletions across both layers, counting unique paths; untracked files show a marker rather than an invented line count. Untracked patches are read lazily on click. Deleted/binary files are included. Use Tab and j/k or the mouse wheel to navigate; Enter/click opens the native popup. Nothing is staged by viewing it.
- **Committed on branch:** shows merge-base-to-HEAD file stats from `git diff <comparison>...HEAD`. This section covers committed changes; local edits belong in Working tree above it. Click a file or use Tab, ↑/↓, Enter to open its patch.
- **Graph:** follows the supplied Orca reference: a `COMMITS` section, colored rails at the left, solid ordinary-commit nodes, hollow merge nodes, disclosure arrows, commit subjects and right-aligned branch badges. **History starts collapsed as a one-line bar at the bottom.** Click the heading or Tab to it and press Enter to expand it; folding it moves the bar down and releases its height to the working-tree list. The graph is not projected while folded. Opening history focuses its rows immediately, so j/k works without another Tab. Use Shift+Tab to return to the heading and Enter to fold it. Click or press Enter on a commit to expand its SHA, totals and file stats **inline beneath that commit**. Multiple commits can stay expanded at once; toggling one leaves the others open. Rails continue through inserted rows, and selected rows retain rail colors. Click a file to open its native diff popup. Navigation skips connector/summary rows. The refresh icon reloads local state. “Load older commits” is shown only while history is open and the loaded window may have more records. Chinese text and labels are ellipsized by terminal-cell width.
- **Diff:** a native Herdr popup displays the unified patch. ↑/↓ or mouse wheel scrolls vertically; ←/→ scrolls horizontally; PageUp/PageDown and Home are supported. Esc/q closes it. Standalone mode uses an in-process dialog. Merge diffs compare against the first parent; root commits compare against the empty tree. Renames appear as deletion/addition. Binary files show Git's patch metadata.
- **Refresh:** Ctrl+R reads local repository state. There is no background fetch or filesystem watcher.
- **Rendering:** the panel redraws only on input, resize/focus restoration, or a worker result. Idle panels block on terminal input; the short worker-result poll never redraws on its own. Graph rows, their selectable-item mapping, and file-list widgets are cached. j/k changes selection without rebuilding rows; a new snapshot, commit expansion/collapse, or a different graph width invalidates the relevant cache.
- **Close:** Esc closes the active dialog or panel. Active commit/push/MR work must finish before exiting. Git operations run in a worker, keeping the terminal responsive.

The compact layout keeps the form above independently scrolling working-tree files, committed branch files, and the commit graph with its inline files. **Tab / Shift+Tab** visits only actionable controls: empty lists, unavailable buttons and hidden history content are skipped. Commit requires a title and changes allowed by the staging mode; Push requires a destination; Create MR requires a branch, a commit and a configured remote. Disabled buttons are dimmed. **Enter** activates; clicking a file opens the diff directly. Commit files are navigated inside the graph rather than through a duplicate focus stop.

## Push

Default shortcut: **Ctrl+P**, configurable below. Push deliberately uses the **selected comparison branch as the destination**, which may differ from the current checkout. A dialog displays the exact destination and requires Enter or a click on Push.

| Comparison | Destination |
| --- | --- |
| `origin/release` | `git push origin HEAD:refs/heads/release` |
| local `release`, upstream `upstream/stable` | `git push upstream HEAD:refs/heads/stable` |
| local `release`, no upstream, with `origin` | `git push origin HEAD:refs/heads/release` |

No force push or automatic upstream modification. Git's rejection is displayed. SSH/credential helpers must already work without a terminal prompt.

## Config

Find the config directory:

```sh
herdr plugin config-dir kingdee.git-panel
```

Edit `config.json` and reopen the panel:

```json
{
  "push_key": "ctrl+p",
  "history_limit": 150,
  "default_remote": "origin",
  "hosts": {
    "code.company.example": "gitlab",
    "github.company.example": "github"
  }
}
```

Push shortcuts accept Ctrl/Alt/Shift/Super modifiers with a character, Enter, Space, or F key; choose one your terminal transmits. `default_remote` is the MR fallback when the comparison has no push destination. GitHub/GitLab are inferred from the selected remote hostname; use `hosts` for self-hosted forges with other names. Credentials stay with Git, gh and glab.

Config lives in `HERDR_PLUGIN_CONFIG_DIR`; transient diff handoffs and panel records live in `HERDR_PLUGIN_STATE_DIR`. Commands use argument arrays, not shell-interpolated repository data. Popup handoffs use private temporary files, deleted when consumed or if popup launch fails.

## Develop and validate

```sh
cargo fmt --check
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
cargo run -- panel --repo /absolute/path/to/repository
```

Tests cover real Git repositories (initial commits, merges, special filenames, staged-only commits, pushes to a local bare remote), local-only branch search, MR CLI argument construction, config symlinks/conflicts, Unicode/multiline input, configurable shortcuts, and graph rendering with details at 42/60 columns. They do not create real hosted MRs or validate forge credentials.

Graph previews rendered from a temporary nested-merge fixture using the same Rust projection and the local Herdr theme:

[Collapsed rows](docs/graph-preview.png) · [Expanded file stats](docs/graph-expanded.png)

Regenerate the previews without network access or touching existing repositories:

```sh
cargo run --example graph_preview -- docs/graph-preview.svg
cargo run --example graph_preview -- docs/graph-expanded.svg --expanded
```

```text
Cargo.toml / Cargo.lock  Rust dependencies
herdr-plugin.toml       Official plugin manifest
src/git.rs             Git operations and parsed graph data
src/graph.rs           Orca-style rows, badges and inline rail-preserving details
src/host.rs            Herdr CLI/socket, config and setup
src/theme.rs           Inherited Herdr palettes and custom color overrides
src/ui.rs              Ratatui panel, searchable picker, forms and diff viewer
src/main.rs            panel / toggle / diff / setup dispatch
scripts/build.sh       Reproducible release build into bin/
tests/git_workflow.rs   Git and setup integration tests
```

`sh scripts/build.sh` builds a release binary into `bin/` using a temporary filename and atomic rename, allowing existing processes to finish. Neither runtime nor setup needs the source checkout's absolute build path. A GitHub repo containing this manifest can be installed with `herdr plugin install owner/repo`; add the `herdr-plugin` topic for discovery.

## Releasing

CI checks formatting, manifest/version consistency, tests and Clippy on Linux/macOS, plus the declared Rust 1.88 minimum. A `vX.Y.Z` tag triggers four native builds and publishes a GitHub Release only after they pass. Release archives include the manifest, binary, MIT license, release notes and palette provenance; the publish job generates `SHA256SUMS` for all archives. Action dependencies are pinned to commit SHAs.

For the next release, update `Cargo.toml`, `Cargo.lock`, `herdr-plugin.toml`, the versioned install examples and `CHANGELOG.md`; run the checks, push the default branch, and wait for CI. Then create and push the matching annotated tag. Existing version tags/releases are not overwritten.

```sh
cargo run --locked --example validate_release -- v0.4.2
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
git tag -a v0.4.2 -m "Release v0.4.2"
git push origin v0.4.2
```

Herdr marketplace discovery is separate from GitHub Releases: its index scans the public default branch for `herdr-plugin.toml` when the repository has the `herdr-plugin` topic.

Troubleshooting: `herdr plugin log list --plugin kingdee.git-panel`. If a forge CLI completed without a recognized URL, check the forge before retrying to avoid duplicate MRs.

Remove with `herdr plugin unlink kingdee.git-panel`, then remove the shortcut block and reload config. Source, config and state are preserved by unlink.

## References

- [Herdr plugin guide](https://herdr.dev/docs/plugins/)
- [Herdr socket API](https://herdr.dev/docs/socket-api/)
- [Ratatui table widget](https://ratatui.rs/examples/widgets/table/)
- [GitHub PR creation](https://cli.github.com/manual/gh_pr_create)
- [GitLab MR creation](https://docs.gitlab.com/cli/mr/create/)
