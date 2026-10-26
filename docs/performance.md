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

## History module and final review

`history` now owns Git graph rows, multiple expansions, selected SHA/path identity, viewport and cached projection behind one behavioral interface. It returns read-commit/open-file actions and opaque row-hit tokens; the UI never interprets projected indices. Transport rows move into History instead of remaining duplicated in the panel snapshot. Reconciliation is O(rows + expanded commits); j/k only selects cached rows. Missing selected files fall back to their owning commit, while deferred actions reject missing files.

The query and small-fixture measurements above preceded this final History extraction and review fixes. The additional 3,000-record TUI measurement below uses the final executable. See [quality review](quality-review.md) for reviewed scope, resolved findings and validation limits. The visual architecture review lives in the OS temporary directory, outside the repository.
