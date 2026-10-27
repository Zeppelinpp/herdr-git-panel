# Changelog

## [0.7.3] - 2026-10-28

### Added

- Press `Shift+Enter` in an inline diff to open a larger popup covering 90% of the Herdr workspace width and 88% of its height. Working-tree, branch and history diffs share the existing viewer controls, including `v`, `p`, `Enter` and `a`.
- Enable terminal keyboard disambiguation for modified Enter keys. Use `b` to expand folded context from the bottom in the inline view.
- Configure the Git panel split direction as `right` or `down`.

### Behavior

- Open the popup from the current preview source while preserving the inline view's cursor, selection and folds. The popup reads its own current diff snapshot.

## [0.7.1] - 2026-10-28

### Added

- Press Space in the working-tree list to stage or unstage the selected file while preserving its working copy.
- Press `d` to discard all changes to the selected file, with a file-specific `y`/`n` confirmation and protection against edits made while the dialog is open.
- Move across file lists, inline diffs, history and other non-editing focus areas with `j`/`k` at row boundaries. Tab remains a direct focus jump; active diff selections stay inside the diff.

### Performance and fixes

- Keep file actions on the serialized background Git worker and refresh only working-tree data afterward. Preserve drafts, cached history and the user's current selection.
- Preserve path identity for queued file actions and prevent hidden or clipped discard confirmations from accepting destructive input.

## [0.7.0] - 2026-10-28

### Features

- Preview the selected file inline below the compact file lists. Collapsed commit history gives the preview the available space; expanded history shares it. Enter focuses the diff, Esc returns to files, and `o` opens a popup.
- Show five context lines around each change, with fixed old/new line numbers, bounded inline emphasis, and expandable unchanged intervals. Missing context loads on demand against the displayed file version.
- Select diff lines with `v`, mouse drag or Shift-click, then press `p` to choose a ready Herdr agent pane in the current workspace. Paste repository, file, comparison and line-numbered source into its input for submission there.

### Performance and fixes

- Render only viewport rows, coalesce fast file selection for 75ms, discard stale reads, and preserve loaded context and reading position across same-content refreshes.
- Preserve file mode and missing-final-newline information; handle Unicode, Tab stops and scroll positions beyond 65,535 lines.
- Keep review selections stable during repository refreshes, revalidate agent targets before delivery, and disable delivery when its picker is hidden or too small.

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
