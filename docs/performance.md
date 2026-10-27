# Automatic refresh and performance

Measured on macOS / Apple M5, 2026-10-26, release build. These are short local measurements, not long-duration leak tests or Linux benchmarks.

## Refresh behavior

`watch` is the repository-change module. Its interface hides native notification classification, bounded path aggregation, debounce deadlines and refresh-rate limits. Native filesystem notifications are its adapter; tests exercise the same scheduling seam with explicit times, then verify actual native events against temporary Git repositories. This gives the module depth: without it, those rules would spread through input handling, worker completion and rendering. The benefit is locality of scheduling rules and leverage across keyboard refreshes, filesystem changes and tests.

`runtime` owns terminal input and wakeup delivery. Its bounded input queue and one-slot wake queue allow the main loop to sleep until input, a repository event or a completed query arrives. There is no recurring refresh poll. A timeout exists only while a known event batch is settling. Filesystem callbacks do not execute Git or render.

`git` owns local queries and ignore semantics. File/index changes query only status and working-tree statistics. HEAD, refs and repository settings request a full snapshot. `stats` prepares the selected comparison once and reuses it for summary and numstat, eliminating repeated branch validation and base resolution. This pins both base and HEAD for the summary and numstat queries. Other reads in a full snapshot can still observe external repository changes between commands.

`ui` applies results by SHA and path identity. Working-tree updates do not invalidate history. Identical results do not redraw. Full refresh retains surviving expanded commits, draft text, staging mode and selected files. While a background query runs, editing and j/k continue; an activation waits with its selected commit/file identity. Modal dialogs postpone automatic refresh. Ctrl+R requested during a query is retained. Only one Git worker executes at a time.

Events settle for 500ms, with a maximum batch deadline of 2s and at least 1s between automatic query starts. A running query or open modal may postpone that deadline. Native delivery latency and query execution add to visible refresh latency. At most 256 distinct pending paths are retained; larger bursts collapse to their common directory so ignored output can still be filtered. Mixed source/output bursts conservatively widen the query. Git reads set `GIT_OPTIONAL_LOCKS=0` to avoid writing index stat data and feeding a notification loop. No refresh fetches.

## Query measurements

Five runs each against the existing `~/work/kwork-server`, read-only. Both measurements used the same repository state and warmed filesystem caches; full refresh loaded 150 history records.

| Query | Git processes | Median wall time | Maximum |
| --- | ---: | ---: | ---: |
| Full snapshot | 17 | 895ms | 1,271ms |
| Working-tree snapshot | 4 | 131ms | 143ms |

A typical file event adds one batched `check-ignore` process before those four queries. An index event requires no ignore query. Directory batches containing tracked files remain relevant even when the directory matches ignore rules.

The previous full snapshot needed 19 Git processes. Preparing the comparison once reduces that to 17; the larger saving comes from avoiding history, refs and comparison work on ordinary saves.

## Running terminal measurements

Actual standalone TUI in a 60×40 PTY, with a temporary repository and Git-command counting. CPU is **plugin process only**, expressed as a fraction of one core; Git subprocess CPU is not included. `ps` time has coarse resolution, so 0% means no measurable increase during the short interval. Terminal bytes count output during each phase.

| Phase | Duration | Plugin CPU | RSS | Git processes | Terminal bytes |
| --- | ---: | ---: | ---: | ---: | ---: |
| Idle | 4.01s | 0% observed | 8.98MiB | 0 | 0 |
| j/k, 20 keys/s | 4.53s | 0.44% | 9.23MiB | 0 | 1,520 |
| Save → visible new file, no input | 0.63s | 1.59% | 9.30MiB | 5 | 422 |
| 600 ignored output files | 3.05s | 0.33% | 9.36MiB | 1 | 0 |
| Idle after changes | 3.01s | 0% observed | 9.36MiB | 0 | 0 |

For comparison, the published v0.4.2 binary against the same small fixture used 3.91MiB idle and 4.17MiB while navigating. Both versions produced zero idle output and zero idle Git queries. Native notification support adds approximately **5MiB RSS** in this macOS experiment; it does not add continuous Git polling. The 600-file ignored burst performed only `check-ignore`, with no status/diff/history query and no redraw.

The temporary Rust measurement harness is at `/var/folders/k6/1d51p33s7_16wtw0jmfrjqhr0000gn/T/herdr-watch-perf-myyl8vqj`; it is not a product dependency. Product implementation and tests remain Rust.

Final History module measurement: same 60×40 PTY, a local synthetic repository containing 5,000 commits, 1,001 refs and 2,001 tracked files, with 3,000 history commits loaded and the history expanded.

| Final phase | Duration | Plugin CPU | RSS | Git processes | Terminal bytes |
| --- | ---: | ---: | ---: | ---: | ---: |
| Idle, history expanded | 4.01s | 0% observed | 12.55MiB | 0 | 0 |
| j/k, 20 keys/s | 4.26s | 0.94% | 12.56MiB | 0 | 21,400 |

Git command deadlines now include output collection. Each command owns a process group; a timeout terminates its hooks/helpers too, including a descendant holding stdout after the parent exits. Regressions cover both forms without invoking real remote operations.

## Practical limits

- Native notifications may be unavailable on network filesystems. Watch startup/runtime errors are displayed; Ctrl+R remains available. `auto_refresh: false` disables watching.
- Linux recursively registers directories, including ignored trees. Filtering avoids Git work but does not reduce inotify registrations. Very large dependency trees can raise startup cost or exhaust system watch limits; this version does not implement pruned Linux registrations.
- Each open panel owns its watcher. Opening several panels for the same repository duplicates notification handling and query costs.
- The working tree, actual Git directory and shared Git directory are watched, including linked worktrees and standard/embedded submodule metadata. Changes to global Git configuration or external global exclude files require manual refresh.
- An already running executable continues using its loaded code after rebuilding. Close and reopen the panel to activate this implementation.

## Folded diff viewer

The viewer keeps Git's patch as the source of change boundaries and starts with
five context lines on either side of each change; more distant intervals fold.
Overlapping context windows are projected once. Opening a diff reads the patch and its content
identity; it does not load file bodies into the plugin. Patch context can expand
immediately. Expanding beyond that context reads the new-side file on a worker,
which prepares one text buffer, line-start offsets and a cached display width.
The UI validates patch lines before attaching the prepared content. Committed and
staged content uses the full blob OID in the patch; worktree content is checked
against metadata and the patch's new-side hash. Refresh also prepares its document
on a worker.

Hidden intervals and expanded context are ranges. Even expanding an entire large
interval does not create a styled object for every line. A compact projection
maps display positions to code blocks and context ranges by binary search; each
frame creates spans only for viewport rows. Vertical offsets use `usize`, and
both axes are clamped using the current viewport. Cursor, keyboard, mouse and
refresh share source-line mappings in both popup and in-pane modes. Background
completion wakes the event loop; idle viewing does not poll or redraw.

The main panel previews the selected file inline. A 75ms selection deadline
coalesces fast navigation before reading Git. One active worker and one replaceable
desired request bound the queue; old results cannot replace a newer selection.
The event loop waits for the deadline or worker completion, with no idle timer.
Working-tree notifications refresh working previews even if statistics match,
while committed previews retain their snapshot. Same-content refreshes update
worktree stamps and keep loaded context, folds and reading position.

Review selection adds only range comparisons to viewport rendering. Export runs
on demand, reads original selected rows (including full text beyond horizontal
clipping), and stops at 2,000 source rows or 128 KiB. Hidden intervals remain
explicit omissions. Target discovery and delivery run on background workers and
wake the UI on completion. Discovery is requested only with `p` or explicit
refresh, so there is no agent polling while browsing. A selection holds its diff
snapshot until released; repository refresh reconciliation then resumes.

A real 120×50 PTY check verifies that the initial inline preview appears without
input, includes exactly five context lines on each side, expands missing content
after a mouse click, and returns to the file list on Esc. The subsequent 150ms idle
observation produced zero terminal bytes. In-memory UI tests also cover selection
changes without popup creation, unchanged statistics after a save, history sharing
the available height, and windows too short to focus a readable preview.

Inline emphasis compares at most 128 equal-count deletion/insertion pairs in a
change block, with a 4,096-byte line limit. It marks the differing range between
equal prefixes and suffixes. Larger or differently shaped replacements keep
ordinary diff colors. Syntax grammar loading is deferred to future work.

Release-mode measurements on 2026-10-27 used a 120×40 Ratatui `TestBackend`,
synthetic inserted lines, and 300 downward moves plus draws. These measure model
construction and in-memory terminal rendering; Git queries, popup startup, theme
application and terminal I/O are excluded. The first case also incurs warm-up.

| Changed lines | Document build | 300 frames |
| --- | ---: | ---: |
| 1,000 | 0.212ms | 60.4ms |
| 10,000 | 0.608ms | 37.3ms |
| 100,000 | 4.54ms | 36.2ms |

Reproduce with `cargo test --release --locked diff_viewport_benchmark --lib -- --ignored --nocapture`.
The measurements show the vertical scroll path stays independent of total changed
line count for this fixture; they do not establish an end-to-end latency or memory
limit. A separate real 120×40 PTY smoke check opened a committed diff, expanded
missing context asynchronously without another keypress, collapsed it, and exited
cleanly. An idle 150ms observation after expansion produced zero terminal bytes.

Limits: the initial patch remains proportional to changed content. First expansion
beyond the patch reads and indexes the entire new-side file; the index adds one
`usize` per line. Worktree hash verification has transient whole-file copies.
Long-line horizontal clipping still scans characters up to the offset, with no
per-character string allocation or sparse column index. Custom clean filters that
cannot be matched by raw/CRLF-normalized content reject expansion. Native Herdr
pane creation and Linux terminal behavior need verification in their respective
environments.

## History module and final review

`history` now owns Git graph rows, multiple expansions, selected SHA/path identity, viewport and cached projection behind one behavioral interface. It returns read-commit/open-file actions and opaque row-hit tokens; the UI never interprets projected indices. Transport rows move into History instead of remaining duplicated in the panel snapshot. Reconciliation is O(rows + expanded commits); j/k only selects cached rows. Missing selected files fall back to their owning commit, while deferred actions reject missing files.

The query and small-fixture measurements above preceded this final History extraction and review fixes. The additional 3,000-record TUI measurement below uses the final executable. See [quality review](quality-review.md) for reviewed scope, resolved findings and validation limits. The visual architecture review lives in the OS temporary directory, outside the repository.

## File actions and keyboard boundaries

Stage/unstage/discard runs on the existing serialized Git worker. A successful or
failed mutation reads only the working-tree snapshot, retaining history projections
and commit drafts. File actions queued behind a repository refresh keep the selected
path identity, and completing an action does not pull selection away from a file
the user navigated to meanwhile. Native file notifications continue to coalesce.

Discard preparation reads only the selected path's status, index entries, HEAD
identity and metadata stamp. Confirmation revalidates those identities before a
path-scoped restore or removal; it does not load the file body just to show a
prompt. A hidden or clipped confirmation cannot accept `y`.

Keyboard boundary traversal scans a fixed set of focus areas, while list and diff
navigation reuse cached projections. History returns whether navigation moved,
which avoids an extra identity clone or projection merely to detect an edge.
Mouse-wheel scrolling stays within its hovered region; Tab retains direct jumps.
