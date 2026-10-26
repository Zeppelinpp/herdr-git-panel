# Herdr Git Panel

A Git sidebar for [Herdr](https://herdr.dev), with searchable branches, commit history, diffs, commits, push, and GitHub/GitLab merge requests. The panel follows your existing Herdr theme.

## Features

- **Branch comparison:** search local and already fetched remote branches by name. The panel never fetches automatically.
- **Working-tree changes:** browse unstaged, staged and untracked files. Open a file to inspect its diff.
- **Committed changes:** see file statistics between the selected comparison branch and the current branch.
- **Commit history:** browse a colored branch/merge graph. Expand multiple commits independently to inspect their file statistics and diffs. History starts collapsed to leave more room for working-tree changes.
- **Commit:** enter a title and multiline description. Stage all changes, or commit only files already staged.
- **Create MR / PR:** choose the remote, source branch, target branch, title and description. After creation, a clickable `!MR number ↗` link appears beside Create MR. Push the source branch before creating the request.
- **Push:** push the current branch to the destination derived from the comparison branch. A confirmation shows the destination before pushing.
- **Automatic refresh:** local file and Git changes update the panel while preserving your draft, selection and expanded commits.

| Action | Shortcut |
| --- | --- |
| Open / close the panel | `Cmd+Shift+R` after setup |
| Move focus | `Tab` / `Shift+Tab` |
| Navigate files and commits | `j` / `k`, `↑` / `↓`, mouse wheel |
| Expand a commit or open a diff | `Enter` or click |
| Commit | `Ctrl+S` |
| Push | `Ctrl+P`, configurable |
| Refresh | `Ctrl+R` |
| Open the created MR / PR | Click its link, or focus it and press `Enter` |
| Close a diff, dialog or panel | `Esc` |

In a diff popup, use `↑` / `↓` to scroll, `←` / `→` to scroll horizontally, and `PageUp` / `PageDown` for larger steps.

## Installation

Requires **Herdr ≥0.9.5**, Git, and macOS or Linux. Creating requests also requires an authenticated `gh` for GitHub or `glab` for GitLab.

### Install from GitHub

Requires **Rust ≥1.88** and Cargo:

```sh
herdr plugin install Zeppelinpp/herdr-git-panel
```

This installs the latest code from the repository's default branch. To update later:

```sh
herdr plugin update kingdee.git-panel
```

Close and reopen an existing panel after updating.

Add this shortcut to `~/.config/herdr/config.toml`:

```toml
[[keys.command]]
key = "cmd+shift+r"
type = "plugin_action"
command = "kingdee.git-panel.toggle"
description = "Toggle Git panel"
```

Reload the config:

```sh
herdr server reload-config
```

If your terminal reserves `Cmd+Shift+R`, configure it to forward the shortcut or choose another key. On Linux, choose a shortcut your terminal transmits.

### Install a prebuilt release

Download the matching archive and `SHA256SUMS` from [the latest release](https://github.com/Zeppelinpp/herdr-git-panel/releases/latest). Rust is not required.

| Platform | Archive target |
| --- | --- |
| macOS Apple Silicon | `aarch64-apple-darwin` |
| macOS Intel | `x86_64-apple-darwin` |
| Linux ARM64 | `aarch64-unknown-linux-gnu` |
| Linux x86_64 | `x86_64-unknown-linux-gnu` |

Linux archives require glibc ≥2.35, such as Ubuntu 22.04 or newer. For other Linux environments, install from source.

Verify the archive against `SHA256SUMS`, extract it to a permanent location, then run inside the extracted directory:

```sh
./bin/herdr-git-panel setup
```

Setup registers that directory, adds the shortcut and reloads Herdr config. Keep the directory in place. Release archives contain the latest published release; they may lag behind the default branch.

### Install a local checkout

```sh
git clone https://github.com/Zeppelinpp/herdr-git-panel.git
cd herdr-git-panel
sh scripts/build.sh
./bin/herdr-git-panel setup
```

For later local changes, rerun `sh scripts/build.sh`, then close and reopen the panel. No Herdr restart is needed. Locally linked installations are updated by rebuilding, rather than `herdr plugin update`.

## Configuration

Find the plugin's configuration directory:

```sh
herdr plugin config-dir kingdee.git-panel
```

The default file is:

```text
~/.config/herdr/plugins/config/kingdee.git-panel/config.json
```

Create or edit `config.json`, then close and reopen the panel. A complete example:

```json
{
  "push_key": "ctrl+p",
  "history_limit": 150,
  "auto_refresh": true,
  "default_remote": "origin",
  "hosts": {
    "git.kingdee.com": "gitlab",
    "github.company.example": "github"
  }
}
```

| Setting | Default | Description |
| --- | --- | --- |
| `push_key` | `ctrl+p` | Shortcut that opens push confirmation. |
| `history_limit` | `150` | Commits loaded initially and per “Load older commits” action. Must be greater than zero. |
| `auto_refresh` | `true` | Automatically refresh local changes. Set to `false` to refresh manually with `Ctrl+R`. |
| `default_remote` | `origin` | MR / PR form fallback when the comparison has no push destination. |
| `hosts` | `{}` | Map self-hosted Git server hostnames to `gitlab` or `github`. |

### Self-hosted GitHub / GitLab

For a domain such as `git.kingdee.com`, set its platform explicitly in `hosts`. Use the hostname only, without `https://` or a repository path.

An `Unknown forge` message means the selected remote's hostname needs this mapping. Ensure the corresponding CLI is installed and authenticated against that server before retrying. Authentication stays with Git, `gh` and `glab`.

### Push destination

The comparison branch determines where the current branch is pushed:

| Selected comparison | Destination |
| --- | --- |
| `origin/release` | `origin`, branch `release` |
| Local `release`, tracking `upstream/stable` | `upstream`, branch `stable` |
| Local `release`, without an upstream | `origin`, branch `release`, if `origin` exists |

The confirmation shows the exact destination. Push does not force-update branches or change upstream settings.

### Theme and refresh

Colors follow Herdr's configured theme, including custom colors and accent. After changing the Herdr theme, press `Ctrl+R` or reopen the panel.

If automatic refresh is unavailable, the panel shows a message and manual refresh remains available. Network filesystems and very large Linux directory trees may need manual refresh. The panel never fetches remote branches; fetch through your normal Git workflow first.
