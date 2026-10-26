# Changelog

## [0.6.0] - 2026-10-27

### Fixed

- Lowercase the plugin id to `zeppelinpp.git-panel`. Herdr percent-encodes uppercase letters when it derives the plugin config directory, so `Zeppelinpp.git-panel` produced `plugins/config/%5Aeppelinpp.git-panel` while the documented path and the plugin's own fallback pointed at the unescaped directory. The lowercase id keeps the config directory, the documented path and the manual-run fallback identical.

### Breaking

- Existing 0.5.0 installations must relink or reinstall: the id, config directory and keybinding command change again. Move settings from `plugins/config/%5Aeppelinpp.git-panel/config.json` to `plugins/config/zeppelinpp.git-panel/config.json`, and set the binding command to `zeppelinpp.git-panel.toggle`.

## [0.5.0] - 2026-10-27

### Breaking

- Rename the plugin id from `kingdee.git-panel` to `Zeppelinpp.git-panel` to match the publishing repository. Existing installations must relink or reinstall, because the registry id, managed plugin path, config directory and keybinding command all derive from the id.

### Upgrade

- Set the keybinding command to `Zeppelinpp.git-panel.toggle` in `~/.config/herdr/config.toml`, and move any settings from `plugins/config/kingdee.git-panel/config.json` to `plugins/config/Zeppelinpp.git-panel/config.json`.

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
