# Repository quality review — 2026-10-26

Review baseline: `08d97d6e3fc6ee5c2bbb439e9235e73fed5ba063`, including all current working-tree changes and new modules. Whole-repository inspection also covered existing Git mutations, forge command construction, Herdr integration, themes, installation and release workflows. The spec source was the originating conversation and README; there is no external issue or separate coding-standard document. GLOSSARY.md and documented behavior were the domain/standards references. Standards and Spec were reviewed independently, then fixes were checked against the reproductions.

## Standards

| Initial finding | Resolution | Evidence |
| --- | --- | --- |
| P2: Killing only the direct process allowed descendants holding stdout/stderr to defeat the timeout. | Each command owns a process group. The same deadline covers child execution and pipe collection; timed-out descendants are terminated. | Public command reproduction improved from 4.011s to 1.001s for a one-second deadline. Integration tests cover a waiting parent and an already exited parent. |
| P2: `check-ignore` rejects paths inside submodules, aborting automatic refresh. | Rejected event paths conservatively request authoritative working-tree status. | Native submodule save test checks relevance and updated parent status. |
| P3: Release archive omitted a new README-linked document. | Packaging includes the complete docs directory and glossary. | Local archive content and unpacked-binary smoke checks. |

History now has useful module depth: its interface owns rows, expansions, SHA/path selection, viewport and projection invalidation. It returns actions and opaque hit tokens. Production Panel code no longer reaches into cache or display indices. There is no hypothetical adapter abstraction and no pass-through module introduced solely to shorten a file. The deletion test holds: deleting History would redistribute this state machine across refresh, keyboard, click and rendering callers.

**Standards: three initial material findings resolved; no remaining material finding in the reviewed tree.** No documented style breach or speculative smell was promoted to a blocker.

## Spec

| Initial finding | Resolution | Evidence |
| --- | --- | --- |
| P2: Moving HEAD between summary and numstat queries produced one-file summary with a two-file list. | Resolve both comparison tips once, then use fixed SHAs for both queries. | An isolated Git shim moves HEAD immediately after `--stat`; the real query returns consistent one-file output, and the test verifies HEAD actually moved. |
| P2: Submodule HEAD-only commits changed the parent's gitlink without any source-file notification. | Standard `.git/modules` metadata changes request parent working-tree refresh; external metadata registrations include submodule directories and attach when those directories appear. | Native test commits with `--allow-empty` inside a submodule and observes parent status refresh. |
| P2: Old-form embedded submodules were skipped, and broad path filtering could miss modules named `objects` or `logs`. | Nested `.git` metadata is recognized; meaningful metadata is matched without excluding valid module names. | Native embedded-submodule HEAD-only test and classification tests for those names. |
| P2: After a failed initial read, partial worktree refreshes had no snapshot to update. | Promote the next automatic event to a full read until a snapshot exists. | UI test starts without a snapshot and recovers branch history from a file event. |

History requirements remain covered through its behavioral interface: independent multiple expansions, correct per-commit file actions, stable selection after row replacement, owning-commit fallback, stale deferred-action rejection, folded lazy projection and j/k cache reuse. UI tests retain focus, theme, rendering, editor and event-routing coverage.

**Spec: four initial behavior findings resolved; no remaining known material spec deviation in the reviewed tree.**

## Overall quality and validation limits

The implemented three optimizations are native automatic refresh, comparison-query reuse, and the History module. Their responsibilities are concentrated in `watch`, `runtime`, `git` and `history`; `ui` coordinates presentation and user intent. This improves locality while keeping observable behavior testable through the same interface callers use.

Validation includes the full Rust suite, formatting, Clippy with warnings denied, manifest/version validation, a local release package, checksum/unpack/run smoke checks, real native-notification fixtures and actual TUI performance measurements. No real hosted MR was created and no user repository was committed or pushed during testing.

Remaining disclosed limits:

- Linux recursively registers ignored directories too; very large trees can exhaust inotify resources. The current local native run was macOS; Linux execution still depends on CI.
- Every open panel owns its watcher and worker, so repeated panels for one repository duplicate resource use.
- Global Git configuration and external global exclude files require manual refresh. Arbitrary external custom submodule Git-directory layouts are not discovered beyond the watched working tree and Git metadata directories.
- Full snapshots comprise several Git reads and are not transactional. Both diff-stat tips are stable, but unrelated history/status reads may see concurrent external changes.
- Large diff payloads are collected in memory. Tests cover normal and large fixtures, not an unlimited-size patch or long-duration memory leak run.
- Credentialed gh/glab and live forge creation remain outside local automated validation; argument construction and URL parsing are covered.
