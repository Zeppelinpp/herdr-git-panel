# Changelog

## [0.4.2] - 2026-10-26

First published release of Git Panel for Herdr.

### Features

- Native Rust sidebar toggled with Cmd+Shift+R and file diffs in Herdr popups.
- Search local branches and already fetched remote references without automatically fetching.
- Separate working-tree and committed-branch stats, including staged, unstaged and untracked files.
- Commit title/description fields, GitHub PR and GitLab MR creation through gh/glab, and clickable MR links.
- Push to the selected comparison branch with an explicit destination confirmation.
- Orca-inspired colored commit rails, inline file stats, Unicode-aware labels and inherited Herdr themes.
- On-demand rendering and cached lists, with no idle redraw timer.

### Fixes

- Skip unavailable controls and empty lists in Tab/Shift+Tab navigation.
- Start history collapsed at the bottom so working-tree changes get more space.
- Use a visible, theme-accent insertion cursor in focused inputs.
- Focus history rows immediately after expansion so j/k navigation continues through commits and files.

### Distribution

- Source installation through `herdr plugin install Zeppelinpp/herdr-git-panel --ref v0.4.2`.
- Prebuilt macOS and Linux packages for ARM64 and x86_64, with SHA-256 checksums.
- Automated validation and tag-triggered GitHub Releases.
