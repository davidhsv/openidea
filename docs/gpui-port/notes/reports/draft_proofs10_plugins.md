I wrote 23 tests in 5 new files (695 lines), but none of them has been compiled or run, so every pass/fail statement below is a prediction. Three are meant to fail until something is fixed. I changed no existing file.

| File                                                                           | Tests |
| ------------------------------------------------------------------------------ | ----- |
| `app-gpui/src/workbench/plugin_runtime/tests/proofs10_host_ui.rs`              | 6     |
| `app-gpui/src/workbench/plugin_runtime/tests/proofs10_events.rs`               | 6     |
| `app-gpui/src/workbench/plugin_runtime/tests/proofs10_host_api.rs`             | 8     |
| `app-gpui/src/workbench/plugin_contributions/tests/proofs10_menus_settings.rs` | 2     |
| `app-gpui/src/workbench/plugins_window/tests/proofs10_lifecycle.rs`            | 1     |

The notes are in `/private/tmp/claude-502/-Users-davidvieira--claude-personal-spares-spare-BDrKLZ/ef45d052-1f0c-4be6-a975-4f7e63cf4971/scratchpad/drafts/proofs10_plugins.md`. They have the three `mod` lines with their anchors, the classification of every `wit:` row, the proposed proof text per test, the spec case tables and the hunks. The tests need no new accessors.

**What the tests cover:** the confirm, multi-line input and folder questions; removing a context key and a status item; one plugin running another's command and getting its result; a window-only status item staying out of a second window. Also file-opened and unsubscribe, file-closed, files-changed, scheduled runs, task progress reports, output channels, secrets (kept in the temp home's file store), running a granted program, opening a file and setting the selection, text search, and the commit log, blame and diff. The project view menu passing the selected file is covered, as is a project setting winning over the global one.

**Expected to fail:**

- **Open editors:** a plugin asking for the window's open editors gets nothing. The plugin host reads them from a report only the webview fills (`mcp_set_open_editors`); the native app never sends it. The notes have a small fix in `plugin_tabs_changed`.
- **UI language:** plugins always read `"en"` for the UI language. The app never calls `set_ui_locale`. I found no place where the native app applies the language setting at runtime, so the fix is only partly contained.
- **Deactivate:** this test needs the rebuilt api14 fixture below.

**`wit:` rows (predicted, if the tests pass)**

- **Become done:** `events.wit#tasks`, `exports.wit#event-handler`, `host.wit#command-runner`, `views.wit#settings`, `workspace.wit#invocation`, plus `ipc:plugin_subscribe`. `views.wit#view-provider` can be ticked now with the existing view tests.
- **Stay partial:**
  - `host.wit#ui`: the language is not served.
  - `events.wit#events`: nothing in the app reports an input-mode change.
  - `exports.wit#lifecycle`: activation when a view opens has no native caller.
  - `host.wit#output`, `host.wit#storage`, `io.wit#secrets`, `io.wit#process`: clear, keys, is-set and poll/write/kill have no fixture caller.
  - `workspace.wit#editor`, `workspace.wit#workspace`, `workspace.wit#vcs`.
- **Not served:**
  - decorations;
  - commit and commit-checker;
  - database connect;
  - plugin terminals, content tabs and view post-message;
  - the UI language;
  - inlay hints and semantic tokens, inline completion, line markers, code lenses, document links, implementations: hypothesis from a grep that the app never calls these backend entry points.
- **Served but untested:** the backend's editor providers (hover, completion, code actions, definition, format), ipc, auth, `vcs.remotes`, `editor.reveal`. The notes give each one's reason. Network fetch can't be tested because it needs the network, and `log` only writes to the plugin's own thread's tracing output, so a test can't capture it.

**Spec files:** `plugins.test.tsx` goes from 13 to 15 of 61 (menu path and selection; host command requests). The others are unchanged: `settings` 5/12, `logEditor` 2/8, `tasks` 3/9 (4/9 if you count the existing language-server cancel test), `views` 15/26, `workspaceContains` 12/13.

**Fixture needed:** api14 gets a `deactivate` that stores a value, plus `deactivated` and `locale` commands (exact diff in the notes). Rebuild with `plugins/build.sh crates/ide-plugin/tests/fixtures/api14`, which covers fixtures as well as plugins.

**Not written:** consent for a running plugin's permission request. That only happens for a permission grant migrated from plugin API 0.1, and no fixture or helper creates one.

**Most likely to fail first:**

- The scheduled-run test: schedules have a 60-second minimum, so the test runs the host's upkeep itself.
- The files-changed test: delivery timing.
- The file-closed test: the shape of the stored data.
- The status-item removal test: what the repository status reports once `.git` is removed.
