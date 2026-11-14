All of Part 1 is fixed and green on the final tree: ten whole-suite runs plus a solo and a paired run on top, with 0 failed and 0 ignored everywhere. All 33 Part 2 tests pass, and their three product hunks are applied. One flaky stop remains open, and its cause is in `crates/ide-run`, which is outside my scope.

## 1. Part 1

**Stress before**

| Run                              | Binary     | Load  | Failures                |
| -------------------------------- | ---------- | ----- | ----------------------- |
| 10 procs × 15 rounds             | HEAD       | 34–74 | 7 / 150                 |
| Two whole suites at once (pair1) | trace only | —     | 5 and 6 per copy, 313 s |

The 7/150 split as: `a_tab_that_cannot_be_saved…` ×3; `settle` 3 s run-outs ×4 (amend ×2, add_to_vcs, new_tag).

In pair1 both copies failed `rollback_in_an_editor…`, `rollback_restores…`, `add_to_vcs…`, `in_a_repository_the_git_shortcuts…` and `stop_ends_the_process…`; copy 2 also failed `log_actions::new_branch_new_tag_rebase_onto…`.

Classes, by evidence:

- **(a) FS event lost by macOS.**
  - The rollback dumps have no `event fs` line, only `read done: 0 files`, and "reached later: None" after 85 s.
  - Probe: the 513th live FSEvents stream in one process never delivers. With 2 processes at once the limit fell to #479/#480; with 4, streams went silent by #250–300 (checked every 50).
  - "FSEventStreamStart fails silently" is a hypothesis: notify ignores its result.
  - Why the suite hit it: the test teardown's `App::quit` is a no-op on the test platform, so no test window or project is ever released. Streams grew to 400+ per process; after the fix the maximum seen was about 14.
- **Polling misses same-second edits (measured).** notify's polling fallback keeps mtime in seconds, so it never reported an edit made in the same second as its last scan (`seen=false` after 10 s). The 400-stream budget I tried first fell back to polling; that broke two `smoke_backend_events` tests and raised the "sync may be slow" notice.
- **(b) Commit panel reads outside the window's Git gate.** Dump: `git rebase` failed with `index.lock: File exists` right after `operation Commit ended`. Reading the code: `CommitPanel` read status itself on every `VcsChanged` and after a commit, and a status read writes the index.
  - With the panel fix, `commit_and_rebase_after_fixup` went from 28/150 failures to 0/150.
- **(b) Project tree skipped a forced reload while a read of that folder was in flight.**
- **(c) Tab test.** Closing a tab saved it twice: once on close, once as the previously active editor. Without the fix there were two "Could not save all files" notices; the first one could arrive before the tab came back.
- **(c) Three tests asserting before the state they check:**
  - `log_actions` rebase: the branch moves before HEAD is reattached (`left: "HEAD"`).
  - `the_logs_keys_work…`: the editor's Copy Reference is asynchronous (`left: Some("a.txt:1")`, expected `"before"`).
  - `project_tree::follows_changes_on_disk`: asserted `inflight` empty while the tree's own watcher was reading.
- **(c) Fake JVM in the java debug test.** A plain `sh` loop survived a group SIGINT 5/150 times under load; with `trap 'exit 130' INT` it was 0/150.
- **(d) Starvation in `settle`.** It reached its state at 4.1–5.6 s against a 3 s budget.

**Fixes**

- `crates/ide-vfs/src/watch.rs` (+123): a per-process count of FSEvents streams, capped at 512. Past that, the watcher falls back to polling with a reason, which is surfaced as the existing "sync may be slow" warning. `fsevent_streams()` is now public. New deterministic test: `a_stream_past_the_budget_is_refused_until_one_is_given_back`. A probe showed all 560 watchers delivering with the cap in place.
- `backend.rs` (test only): a thread-local guard closes every project a test's backend opened when the test thread ends.
- `vcs/commit_view.rs`, `workbench/mod.rs`, `workbench/commit.rs`: in a window, the Commit panel is fed only by the window's gated read. `CommitPanel::new` is now used only by tests. New test: `the_commit_panel_reads_nothing_itself_while_a_git_command_of_the_window_runs` (fails with `left: 1` without the fix).
- `project_tree.rs`: a folder asked to reload during a read is read again afterwards. New test: `a_change_reported_while_its_folder_is_read_is_read_again_after` (fails without the fix).
- `workbench/mod.rs`: no autosave of an editor whose tab is closing.
- `settle`: the budget is now 30 s, the same as `wait_long`. This is a longer fixed budget, not one that scales with load.
- Dumps now include the FSEvents stream count, armed/fired debounce numbers, read requests, the tree's own trace, and run sessions.

**Crate gates**

- `cargo test -p ide-vfs`: 52 + 12 passed.
- `cargo clippy -p ide-vfs --all-targets -- -D warnings`: clean.
- `cargo test -p openidea export_bindings`: ok.
- `git status --short src/ src-tauri/`: empty.

**After** (final Part 1 binary, filters plus `project_tree`, the Git tool window test and the new commit-panel test):

| Processes × rounds | Load   | Failures |
| ------------------ | ------ | -------- |
| 10 × 15            | 57–114 | 0 / 150  |
| 3 × 5              | —      | 0        |
| 6 × 5              | —      | 0        |

**Ten whole-suite runs** (6802 tests each, 0 ignored):

- Runs 1–8 solo: 112–114 s each, all green.
- Runs 9 and 10 paired with a second whole suite: 161–162 s, both copies green each time.

After Part 2 on the final tree: one solo run (6835 tests, 119 s) and one paired run (168 s, both copies) were green.

**Still open:** `stop_ends_the_process_and_the_session` fails 1/1 (76 s) when the test binary is a background job of a script. That job starts with SIGINT ignored (measured: `signal.getsignal` returns 1), and the first Stop only sends SIGINT; the code that sends it is in `crates/ide-run`. Repro: `$S/bgtest.sh` with the `dfl.py` wrapper taken out. Every run of mine went through `$S/dfl.py`, which resets SIGINT to default before starting the binary.

## 2. What a user would have seen

- **Past the stream limit, the tree and Git status never update after an external edit.** Hypothesis that it needs hundreds of projects open, or other apps holding many streams. The cap protects only the solo per-process limit; the lower shared limit is still unguarded.
- **A Git operation started right after a commit (rebase after fixup) could fail with `index.lock`.** The Tauri app shares the status write-back (same backend); whether its UI also reads outside a gate is a hypothesis. To confirm, look for `vcs_status` callers in `src/features/vcs/` that run while an operation is in progress.
- **A stale tree folder until the next change on disk.** Mechanism measured; how often users would hit it is a hypothesis.
- **Two error notices when closing a tab that cannot be saved.**
- **Stop not ending a run when the IDE was started with SIGINT ignored** (for example via `nohup`). The Tauri app uses the same `ide-run`; this is a hypothesis — confirm by launching it with `nohup` and pressing Stop on a `sleep` loop.

## 3. Part 2

All 33 tests are declared and pass: proofs8 has 22, not the 23 the notes claim, and proofs9 has 11.

| Test module                                                                                                                                                                               | parity.json key                                                | Status                 |
| ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------- | ---------------------- |
| `editor::view::tests::proofs8_caret_commands`                                                                                                                                             | `unit:…/caretCommands.test.ts`                                 | done                   |
| `workbench::tests::proofs8_build`                                                                                                                                                         | `unit:…/build.test.tsx`                                        | partial                |
| `workbench::tests::proofs8_file_refactor` (2)                                                                                                                                             | `unit:…/fileRefactor.test.tsx`                                 | partial                |
| `features_a::vim::engine::tests::proofs8_vim`                                                                                                                                             | `unit:…/vim.test.ts`                                           | partial                |
| `vcs_diff::comparison::chunk_optimizer::tests::proofs8_chunk_optimizer` (2), `vcs_more::merge_diff3::tests::proofs8_chunk_optimizer` (2)                                                  | `unit:…/chunkOptimizer.test.ts`                                | done                   |
| `run::driver::tests::proofs8_chooser_pinned` (2), `run::chooser_view::tests::proofs8_chooser` (3), `workbench::debug::tests::proofs8_recent`, `workbench::tests::proofs9_run_chooser` (2) | `unit:…/chooserPinned.test.tsx`                                | partial (strict count) |
| `run::store::tests::proofs8_removal` (4)                                                                                                                                                  | `unit:…/removal.test.tsx`                                      | partial                |
| `features_b::viewers::preview_host::tests::proofs8_preview_only`                                                                                                                          | `unit:…/previewOnly.test.tsx`                                  | done                   |
| `agents::session_view::tests::proofs8_agent_reply`                                                                                                                                        | `unit:…/AgentReply.test.tsx`                                   | done                   |
| `everywhere::structure_view::tests::proofs8_file_structure`                                                                                                                               | `unit:…/fileStructure.test.tsx`                                | done                   |
| `editor::intelligence::tests::proofs9_resolve`                                                                                                                                            | `ipc:lsp_resolve_completion`                                   | done                   |
| `editor::intelligence::tests::proofs9_syntax_usages` (2)                                                                                                                                  | `ipc:syntax_identifiers_in_files`, `ipc:syntax_identifiers`    | done                   |
| `editor::intelligence::tests::proofs9_cancel_task`                                                                                                                                        | `ipc:cancel_task`                                              | done                   |
| `workbench::github::tests::proofs9_github`                                                                                                                                                | `ipc:gh_pr_prepare_diff`                                       | done                   |
| `workbench::plugin_tool_windows::tests::proofs9_view_icon`                                                                                                                                | `ipc:plugin_assets`                                            | done                   |
| `workbench::plugin_runtime::tests::proofs9_status_snapshot`                                                                                                                               | `ipc:plugin_status_items`                                      | done                   |
| `workbench::plugins_window::tests::proofs9_settings_plugins`                                                                                                                              | `action:plugins.settings` (done), `settings:plugins` (partial) | done / partial         |
| `workbench::tests::proofs9_log_users`                                                                                                                                                     | `ipc:vcs_log_users`                                            | done                   |

Tests changed: an unused import removed in `proofs8_chooser_pinned.rs`, and two `[x.clone()]` changed to `slice::from_ref` in `proofs8_removal.rs` for clippy. No assertions changed.

Product fixes applied, each checked against the webview:

- `workbench/project_files.rs`: the stop handle is created in the handler before the refactoring task starts (webview `fileRefactor.ts:251–254`).
- `run/driver.rs` and `workbench/debug.rs`: `mark_recent` is called from `debug_config` (webview `run/store.ts:314–318`).
- `workbench/log_window.rs`: the User filter reads users from every repository root (webview `GitLog.tsx:788–794`). Native sorts by byte order; the webview uses `localeCompare`.

I did not re-check that the three Part 2 tests the notes predicted would fail actually fail without these hunks.

## 4. Other checks

- `cargo clippy --all-targets -D warnings` is clean on the final tree, and no added line is over 120 columns.
- rustfmt changed no file I did not edit.
- Nothing under `docs/`, `src/` or `src-tauri/` was touched, and no git command that changes anything was run.
- A temporary probe test and two debugging hooks were removed. The probe is kept at `$S/probe_limit.rs`.

Notes that turned out false:

- "stop_ends_the_process… is slow, not wrong": the dump shows exit 4, the fake JVM's own one-minute timeout, so Stop never ended it.
- "A write right after the first read is lost": the loss was the process-wide FSEvents limit, not timing around the first read.
- proofs8 has 22 tests, not 23.

All dumps and logs are under `/private/tmp/claude-502/-Users-davidvieira--claude-personal-spares-spare-BDrKLZ/ef45d052-1f0c-4be6-a975-4f7e63cf4971/scratchpad/stab2/` (`$S`): `runs/` for stress, `suites/` for whole-suite runs.
